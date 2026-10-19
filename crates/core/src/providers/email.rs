use crate::config::{EmailConfig, SmtpConfig, SmtpTls};
use crate::delivery::Provider;
use crate::signal::{Signal, SignalError};
use lettre::message::{header::ContentType, Mailbox, MessageBuilder};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// Covers connect, TLS, auth and the whole SMTP exchange of one attempt,
/// so a server that hangs costs one attempt rather than the retry loop.
const SMTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Sends a plain-text email through an SMTP server. Repeats of the same
/// signal keep the same subject and `References` header, so mail
/// clients thread them into one conversation instead of filling the
/// inbox.
pub struct EmailProvider {
    config: EmailConfig,
}

impl EmailProvider {
    pub fn new(config: EmailConfig) -> Self {
        EmailProvider { config }
    }

    fn subject_for(&self, signal: &Signal) -> String {
        let severity = signal.severity.to_string().to_uppercase();
        match &self.config.subject_prefix {
            Some(prefix) if !prefix.is_empty() => {
                format!("{prefix} [{severity}] {}", signal.title)
            }
            _ => format!("[{severity}] {}", signal.title),
        }
    }

    fn body_for(signal: &Signal) -> String {
        let mut body = format!("{}\n", signal.title);
        if let Some(message) = &signal.message {
            body.push('\n');
            body.push_str(message);
            body.push('\n');
        }
        body.push('\n');
        let severity = signal.severity.to_string();
        let mut fields: Vec<(&str, &str)> = vec![
            ("Severity", &severity),
            ("Source", &signal.source),
            ("Event", &signal.event_type),
        ];
        let optional = [
            ("Environment", &signal.environment),
            ("Service", &signal.service),
            ("Component", &signal.component),
            ("Team", &signal.team),
            ("Run ID", &signal.run_id),
            ("Correlation ID", &signal.correlation_id),
            ("Trace ID", &signal.trace_id),
            ("Logs", &signal.log_url),
        ];
        for (label, value) in optional {
            if let Some(value) = value {
                fields.push((label, value));
            }
        }
        for (label, value) in fields {
            body.push_str(&format!("{:<16}{value}\n", format!("{label}:")));
        }
        if signal.metadata.as_object().is_some_and(|m| !m.is_empty()) {
            let metadata = serde_json::to_string_pretty(&signal.metadata)
                .unwrap_or_else(|_| signal.metadata.to_string());
            body.push_str(&format!("\nMetadata:\n{metadata}\n"));
        }
        body.push_str(&format!(
            "\n--\nSignal {} at {}\nSent by OpsSignal\n",
            signal.id,
            signal.timestamp.to_rfc3339()
        ));
        body
    }

    fn message_for(&self, signal: &Signal) -> Result<Message, SignalError> {
        let mut builder: MessageBuilder = Message::builder()
            .from(mailbox(&self.config.from)?)
            .subject(self.subject_for(signal))
            .header(ContentType::TEXT_PLAIN);
        for to in &self.config.to {
            builder = builder.to(mailbox(to)?);
        }
        for cc in &self.config.cc {
            builder = builder.cc(mailbox(cc)?);
        }
        if let Some(key) = &signal.dedup_key {
            let thread = thread_id(key);
            builder = builder.in_reply_to(thread.clone()).references(thread);
        }
        builder
            .body(Self::body_for(signal))
            .map_err(|e| SignalError::Delivery(format!("cannot build the email: {e}")))
    }

    fn transport(smtp: &SmtpConfig) -> Result<AsyncSmtpTransport<Tokio1Executor>, SignalError> {
        let builder = match smtp.tls {
            SmtpTls::Starttls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&smtp.host),
            SmtpTls::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(&smtp.host),
            SmtpTls::None => Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(
                &smtp.host,
            )),
        }
        .map_err(|e| SignalError::Delivery(format!("smtp: {e}")))?;
        let mut builder = builder.port(smtp.port()).timeout(Some(SMTP_TIMEOUT));
        if let (Some(username), Some(password)) = (&smtp.username, &smtp.password) {
            builder = builder.credentials(Credentials::new(username.clone(), password.clone()));
        }
        Ok(builder.build())
    }
}

fn mailbox(address: &str) -> Result<Mailbox, SignalError> {
    address
        .parse()
        .map_err(|e| SignalError::Delivery(format!("invalid email address {address:?}: {e}")))
}

/// A message id that every email for the same dedup key refers to.
/// Hashed because caller-supplied keys may hold characters a header
/// can't carry.
fn thread_id(dedup_key: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(dedup_key.as_bytes()));
    format!("<{}@opssignal>", &digest[..32])
}

#[async_trait::async_trait]
impl Provider for EmailProvider {
    fn name(&self) -> &'static str {
        "email"
    }

    async fn send(&self, signal: &Signal) -> Result<(), SignalError> {
        let message = self.message_for(signal)?;
        Self::transport(&self.config.smtp)?
            .send(message)
            .await
            .map_err(|e| SignalError::Delivery(format!("smtp: {e}")))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{Severity, SignalInput};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    fn signal(severity: Severity) -> Signal {
        Signal::from_input(SignalInput {
            source: "airflow".into(),
            service: Some("orders".into()),
            event_type: "task_failed".into(),
            title: "dbt model failed".into(),
            message: Some("model orders timed out".into()),
            severity,
            environment: Some("prod".into()),
            log_url: Some("https://logs.example.test/run/1".into()),
            metadata: Some(serde_json::json!({ "dag": "daily" })),
            ..Default::default()
        })
    }

    fn config(port: u16) -> EmailConfig {
        EmailConfig {
            from: "OpsSignal <alerts@example.test>".into(),
            to: vec!["oncall@example.test".into()],
            cc: vec!["data@example.test".into()],
            subject_prefix: Some("[opssignal]".into()),
            smtp: SmtpConfig {
                host: "127.0.0.1".into(),
                port: Some(port),
                username: Some("user".into()),
                password: Some("pass".into()),
                tls: SmtpTls::None,
            },
        }
    }

    /// A one-connection SMTP server that accepts everything except
    /// recipients in `reject_rcpt`, and records what the client sent.
    async fn fake_smtp(reject_rcpt: bool) -> (u16, Arc<Mutex<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let transcript = Arc::new(Mutex::new(String::new()));
        let log = Arc::clone(&transcript);
        tokio::spawn(async move {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let (read, mut write) = socket.into_split();
            let mut lines = BufReader::new(read).lines();
            let _ = write.write_all(b"220 fake ESMTP\r\n").await;
            let mut in_data = false;
            while let Ok(Some(line)) = lines.next_line().await {
                log.lock().expect("log").push_str(&format!("{line}\n"));
                let reply: &[u8] = if in_data {
                    if line == "." {
                        in_data = false;
                        b"250 queued\r\n"
                    } else {
                        continue;
                    }
                } else {
                    let verb = line.split(' ').next().unwrap_or("").to_uppercase();
                    match verb.as_str() {
                        "EHLO" | "HELO" => b"250-fake\r\n250 AUTH PLAIN LOGIN\r\n",
                        "AUTH" => b"235 ok\r\n",
                        "RCPT" if reject_rcpt => b"550 no such user\r\n",
                        "DATA" => {
                            in_data = true;
                            b"354 go ahead\r\n"
                        }
                        "QUIT" => {
                            let _ = write.write_all(b"221 bye\r\n").await;
                            return;
                        }
                        _ => b"250 ok\r\n",
                    }
                };
                let _ = write.write_all(reply).await;
            }
        });
        (port, transcript)
    }

    #[test]
    fn subject_is_stable_and_carries_the_severity() {
        let provider = EmailProvider::new(config(25));
        assert_eq!(
            provider.subject_for(&signal(Severity::Error)),
            "[opssignal] [ERROR] dbt model failed"
        );
        let mut plain = config(25);
        plain.subject_prefix = None;
        assert_eq!(
            EmailProvider::new(plain).subject_for(&signal(Severity::Critical)),
            "[CRITICAL] dbt model failed"
        );
    }

    #[test]
    fn body_lists_the_signal_fields() {
        let signal = signal(Severity::Error);
        let body = EmailProvider::body_for(&signal);
        assert!(body.starts_with("dbt model failed\n\nmodel orders timed out\n"));
        assert!(body.contains("Severity:       error\n"));
        assert!(body.contains("Environment:    prod\n"));
        assert!(body.contains("Logs:           https://logs.example.test/run/1\n"));
        assert!(body.contains("\"dag\": \"daily\""));
        assert!(body.contains(&signal.id.to_string()));
        assert!(!body.contains("Team:"));
    }

    #[test]
    fn repeats_of_a_signal_share_a_thread() {
        let provider = EmailProvider::new(config(25));
        let first = provider
            .message_for(&signal(Severity::Error))
            .expect("first");
        let second = provider
            .message_for(&signal(Severity::Error))
            .expect("second");
        let refs = |m: &Message| {
            let text = String::from_utf8(m.formatted()).expect("utf8");
            text.lines()
                .find(|l| l.starts_with("References:"))
                .map(str::to_string)
        };
        assert!(refs(&first).is_some());
        assert_eq!(refs(&first), refs(&second));
    }

    #[test]
    fn odd_dedup_keys_still_make_a_valid_thread_id() {
        let id = thread_id("a key with <brackets> and spaces");
        assert!(id.starts_with('<') && id.ends_with("@opssignal>"));
        assert!(!id[1..].contains(' '));
    }

    #[test]
    fn default_ports_follow_the_tls_mode() {
        let smtp = |tls| SmtpConfig {
            host: "h".into(),
            port: None,
            username: None,
            password: None,
            tls,
        };
        assert_eq!(smtp(SmtpTls::Starttls).port(), 587);
        assert_eq!(smtp(SmtpTls::Tls).port(), 465);
        assert_eq!(smtp(SmtpTls::None).port(), 25);
    }

    #[tokio::test]
    async fn sends_the_email_over_smtp() {
        let (port, transcript) = fake_smtp(false).await;
        EmailProvider::new(config(port))
            .send(&signal(Severity::Error))
            .await
            .expect("accepted");

        let sent = transcript.lock().expect("log").clone();
        assert!(sent.contains("AUTH PLAIN"), "{sent}");
        assert!(sent.contains("MAIL FROM:<alerts@example.test>"), "{sent}");
        assert!(sent.contains("RCPT TO:<oncall@example.test>"), "{sent}");
        assert!(sent.contains("RCPT TO:<data@example.test>"), "{sent}");
        assert!(
            sent.contains("Subject: [opssignal] [ERROR] dbt model failed"),
            "{sent}"
        );
        assert!(sent.contains("model orders timed out"), "{sent}");
    }

    #[tokio::test]
    async fn rejected_recipient_is_a_delivery_error() {
        let (port, _) = fake_smtp(true).await;
        let err = EmailProvider::new(config(port))
            .send(&signal(Severity::Error))
            .await
            .expect_err("550 must fail");
        assert!(matches!(err, SignalError::Delivery(_)));
        assert!(!err.to_string().contains("pass"), "{err}");
    }

    #[tokio::test]
    async fn unreachable_server_is_a_delivery_error() {
        // Bind then drop, so the port is very likely closed.
        let port = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind")
            .local_addr()
            .expect("addr")
            .port();
        let err = EmailProvider::new(config(port))
            .send(&signal(Severity::Error))
            .await
            .expect_err("nothing listening");
        assert!(matches!(err, SignalError::Delivery(_)));
    }
}
