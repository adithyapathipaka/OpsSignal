use crate::delivery::Provider;
use crate::signal::{Action, Severity, Signal, SignalError};
use crate::template::{self, Templates};

/// Web API base URL, for bot-token mode.
pub const DEFAULT_API_URL: &str = "https://slack.com/api";

/// Posts to Slack, through an incoming webhook or, with a bot token,
/// the Web API. Only the Web API can change a message once posted, so
/// only a bot token lets an `acknowledge` or `resolve` update the
/// alert's message in place; through a webhook it is posted as a new
/// message.
pub struct SlackProvider {
    target: Target,
    client: reqwest::Client,
    templates: Option<Templates>,
}

enum Target {
    Webhook(String),
    Bot {
        token: String,
        channel: String,
        api_url: String,
    },
}

impl SlackProvider {
    pub fn new(webhook_url: String) -> Self {
        SlackProvider {
            target: Target::Webhook(webhook_url),
            client: reqwest::Client::new(),
            templates: None,
        }
    }

    /// Posts with `chat.postMessage` as the bot `token` (`xoxb-...`)
    /// into `channel`, an ID or `#name` the bot is a member of.
    /// `api_url` defaults to [`DEFAULT_API_URL`].
    pub fn with_bot(token: String, channel: String, api_url: Option<String>) -> Self {
        SlackProvider {
            target: Target::Bot {
                token,
                channel,
                api_url: api_url
                    .unwrap_or_else(|| DEFAULT_API_URL.to_string())
                    .trim_end_matches('/')
                    .to_string(),
            },
            client: reqwest::Client::new(),
            templates: None,
        }
    }

    /// Uses `templates` for the parts they set; see [`crate::template`].
    pub fn with_templates(mut self, templates: Option<Templates>) -> Self {
        self.templates = templates;
        self
    }

    /// The emoji a message starts with: the action's for an
    /// `acknowledge` or `resolve`, else the severity's.
    fn emoji_for_signal(signal: &Signal) -> &'static str {
        match signal.action {
            Action::Trigger => Self::emoji_for(signal.severity),
            Action::Acknowledge => ":eyes:",
            Action::Resolve => ":white_check_mark:",
        }
    }

    fn emoji_for(severity: Severity) -> &'static str {
        match severity {
            Severity::Info => ":information_source:",
            Severity::Success => ":white_check_mark:",
            Severity::Warning => ":warning:",
            Severity::Error => ":x:",
            Severity::Critical => ":rotating_light:",
        }
    }
}

#[async_trait::async_trait]
impl Provider for SlackProvider {
    fn name(&self) -> &'static str {
        "slack"
    }

    fn preview(&self, signal: &Signal) -> Option<serde_json::Value> {
        Some(self.body_for(signal))
    }

    async fn send(&self, signal: &Signal) -> Result<(), SignalError> {
        self.send_with_ref(signal, None).await.map(|_| ())
    }

    /// In bot mode, returns `channel/ts` of the posted message, and
    /// updates that message when given it for an `acknowledge` or
    /// `resolve`.
    async fn send_with_ref(
        &self,
        signal: &Signal,
        alert_ref: Option<&str>,
    ) -> Result<Option<String>, SignalError> {
        let mut body = self.body_for(signal);
        let (token, channel, api_url) = match &self.target {
            Target::Webhook(url) => {
                self.post(url, None, &body).await?;
                return Ok(None);
            }
            Target::Bot {
                token,
                channel,
                api_url,
            } => (token, channel, api_url),
        };
        let earlier = alert_ref
            .filter(|_| signal.action.is_lifecycle())
            .and_then(|r| r.split_once('/'));
        if let Some((channel, ts)) = earlier {
            body["channel"] = channel.into();
            body["ts"] = ts.into();
            let url = format!("{api_url}/chat.update");
            self.post(&url, Some(token), &body).await?;
            return Ok(alert_ref.map(str::to_string));
        }
        body["channel"] = channel.clone().into();
        let url = format!("{api_url}/chat.postMessage");
        let reply = self.post(&url, Some(token), &body).await?;
        // The reply names the channel by ID, which chat.update needs.
        Ok(match (reply["channel"].as_str(), reply["ts"].as_str()) {
            (Some(channel), Some(ts)) => Some(format!("{channel}/{ts}")),
            _ => None,
        })
    }
}

impl SlackProvider {
    fn body_for(&self, signal: &Signal) -> serde_json::Value {
        let emoji = Self::emoji_for_signal(signal);
        let text = template::render(&self.templates, "text", signal, &[("emoji", emoji.into())])
            .unwrap_or_else(|| {
                let prefix = match signal.action {
                    Action::Trigger => "",
                    Action::Acknowledge => "Acknowledged: ",
                    Action::Resolve => "Resolved: ",
                };
                format!(
                    "{emoji} *{prefix}{}*\n{}",
                    signal.title,
                    signal.message.clone().unwrap_or_default()
                )
            });
        serde_json::json!({ "text": text })
    }

    /// POSTs `body` and returns the Web API's reply (`null` from a
    /// webhook). The Web API answers 200 with `ok: false` on errors.
    async fn post(
        &self,
        url: &str,
        token: Option<&str>,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, SignalError> {
        let mut request = self.client.post(url).json(body);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let resp = request
            .send()
            .await
            .map_err(|e| SignalError::Delivery(e.to_string()))?;
        if !resp.status().is_success() {
            let what = if token.is_some() {
                "web API"
            } else {
                "webhook"
            };
            return Err(SignalError::Delivery(format!(
                "slack {what} returned status {}",
                resp.status()
            )));
        }
        if token.is_none() {
            return Ok(serde_json::Value::Null);
        }
        let reply: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| SignalError::Delivery(format!("slack web API reply: {e}")))?;
        if reply["ok"].as_bool() != Some(true) {
            return Err(SignalError::Delivery(format!(
                "slack web API error: {}",
                reply["error"].as_str().unwrap_or("unknown")
            )));
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::SignalInput;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn signal(severity: Severity, message: Option<&str>) -> Signal {
        Signal::from_input(SignalInput {
            source: "airflow".into(),
            event_type: "task_failed".into(),
            title: "dbt model failed".into(),
            message: message.map(str::to_string),
            severity,
            ..Default::default()
        })
    }

    #[test]
    fn every_severity_has_a_distinct_emoji() {
        let all = [
            Severity::Info,
            Severity::Success,
            Severity::Warning,
            Severity::Error,
            Severity::Critical,
        ];
        let mut emojis: Vec<&str> = all.iter().map(|s| SlackProvider::emoji_for(*s)).collect();
        emojis.sort_unstable();
        emojis.dedup();
        assert_eq!(emojis.len(), all.len());
        assert_eq!(
            SlackProvider::emoji_for(Severity::Critical),
            ":rotating_light:"
        );
    }

    #[tokio::test]
    async fn posts_slack_formatted_text_to_the_webhook() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/services/T/B/X"))
            .and(header("content-type", "application/json"))
            .and(body_json(serde_json::json!({
                "text": ":x: *dbt model failed*\nmodel orders timed out"
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let provider = SlackProvider::new(format!("{}/services/T/B/X", server.uri()));
        provider
            .send(&signal(Severity::Error, Some("model orders timed out")))
            .await
            .expect("delivered");
    }

    #[tokio::test]
    async fn missing_message_sends_title_only() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_json(serde_json::json!({
                "text": ":information_source: *dbt model failed*\n"
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        SlackProvider::new(server.uri())
            .send(&signal(Severity::Info, None))
            .await
            .expect("delivered");
    }

    #[tokio::test]
    async fn non_success_status_is_a_delivery_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let err = SlackProvider::new(server.uri())
            .send(&signal(Severity::Error, None))
            .await
            .expect_err("500 must fail");
        assert!(matches!(err, SignalError::Delivery(_)));
        assert!(err.to_string().contains("500"));
    }

    #[tokio::test]
    async fn unreachable_webhook_is_a_delivery_error() {
        // Bind then drop a listener so the port is known to be closed.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr")
            .port();

        let err = SlackProvider::new(format!("http://127.0.0.1:{port}/hook"))
            .send(&signal(Severity::Error, None))
            .await
            .expect_err("connection refused must fail");
        assert!(matches!(err, SignalError::Delivery(_)));
    }

    #[tokio::test]
    async fn text_template_replaces_the_default() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_json(serde_json::json!({
                "text": ":x: airflow/task_failed: dbt model failed (model orders timed out)"
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        SlackProvider::new(server.uri())
            .with_templates(template::for_test(
                "slack",
                &[(
                    "text",
                    "{{ emoji }} {{ source }}/{{ event_type }}: {{ title }}{% if message %} ({{ message }}){% endif %}",
                )],
            ))
            .send(&signal(Severity::Error, Some("model orders timed out")))
            .await
            .expect("delivered");
    }

    #[tokio::test]
    async fn failing_template_sends_the_default() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_json(serde_json::json!({
                "text": ":x: *dbt model failed*\n"
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        SlackProvider::new(server.uri())
            .with_templates(template::for_test("slack", &[("text", "{{ title + 1 }}")]))
            .send(&signal(Severity::Error, None))
            .await
            .expect("delivered");
    }

    fn lifecycle(action: Action) -> Signal {
        let mut signal = signal(Severity::Error, None);
        signal.action = action;
        signal
    }

    #[tokio::test]
    async fn webhook_resolve_posts_a_resolved_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_json(serde_json::json!({
                "text": ":white_check_mark: *Resolved: dbt model failed*\n"
            })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let alert_ref = SlackProvider::new(server.uri())
            .send_with_ref(&lifecycle(Action::Resolve), Some("C1/1.2"))
            .await
            .expect("delivered");
        assert_eq!(alert_ref, None);
    }

    #[tokio::test]
    async fn bot_posts_then_updates_the_same_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat.postMessage"))
            .and(header("authorization", "Bearer xoxb-1"))
            .and(body_json(serde_json::json!({
                "channel": "#data-alerts",
                "text": ":x: *dbt model failed*\n"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "channel": "C42", "ts": "1700000000.000100"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/chat.update"))
            .and(body_json(serde_json::json!({
                "channel": "C42",
                "ts": "1700000000.000100",
                "text": ":eyes: *Acknowledged: dbt model failed*\n"
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let provider = SlackProvider::with_bot(
            "xoxb-1".into(),
            "#data-alerts".into(),
            Some(format!("{}/api/", server.uri())),
        );
        let alert_ref = provider
            .send_with_ref(&signal(Severity::Error, None), None)
            .await
            .expect("posted");
        assert_eq!(alert_ref.as_deref(), Some("C42/1700000000.000100"));
        provider
            .send_with_ref(&lifecycle(Action::Acknowledge), alert_ref.as_deref())
            .await
            .expect("updated");
    }

    #[tokio::test]
    async fn bot_api_error_is_a_delivery_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": false, "error": "not_in_channel"
            })))
            .mount(&server)
            .await;

        let err = SlackProvider::with_bot("xoxb-1".into(), "C1".into(), Some(server.uri()))
            .send(&signal(Severity::Error, None))
            .await
            .expect_err("ok: false must fail");
        assert!(err.to_string().contains("not_in_channel"));
    }
}
