use crate::delivery::Provider;
use crate::signal::{Severity, Signal, SignalError};
use crate::template::{self, Templates};

/// Posts an Adaptive Card to a Microsoft Teams channel webhook. The
/// message shape works with both Workflows (Power Automate) webhooks,
/// which replace Office 365 connectors, and legacy incoming webhooks.
pub struct TeamsProvider {
    pub webhook_url: String,
    client: reqwest::Client,
    templates: Option<Templates>,
}

impl TeamsProvider {
    pub fn new(webhook_url: String) -> Self {
        TeamsProvider {
            webhook_url,
            client: reqwest::Client::new(),
            templates: None,
        }
    }

    /// Uses `templates` for the parts they set; see [`crate::template`].
    pub fn with_templates(mut self, templates: Option<Templates>) -> Self {
        self.templates = templates;
        self
    }

    /// Adaptive Card text colours.
    fn color_for(severity: Severity) -> &'static str {
        match severity {
            Severity::Info => "default",
            Severity::Success => "good",
            Severity::Warning => "warning",
            Severity::Error | Severity::Critical => "attention",
        }
    }

    fn card_for(&self, signal: &Signal) -> serde_json::Value {
        let title = template::render(&self.templates, "title", signal, &[])
            .unwrap_or_else(|| signal.title.clone());
        let message = template::render(&self.templates, "text", signal, &[])
            .or_else(|| signal.message.clone());
        let mut body = vec![serde_json::json!({
            "type": "TextBlock",
            "text": title,
            "weight": "bolder",
            "size": "medium",
            "color": Self::color_for(signal.severity),
            "wrap": true,
        })];
        if let Some(message) = message {
            body.push(serde_json::json!({
                "type": "TextBlock",
                "text": message,
                "wrap": true,
            }));
        }

        let mut facts = vec![
            fact("Severity", &signal.severity.to_string()),
            fact("Source", &signal.source),
            fact("Event", &signal.event_type),
        ];
        let optional = [
            ("Environment", &signal.environment),
            ("Service", &signal.service),
            ("Component", &signal.component),
            ("Team", &signal.team),
            ("Run ID", &signal.run_id),
        ];
        for (title, value) in optional {
            if let Some(value) = value {
                facts.push(fact(title, value));
            }
        }
        body.push(serde_json::json!({ "type": "FactSet", "facts": facts }));

        let mut card = serde_json::json!({
            "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
            "type": "AdaptiveCard",
            "version": "1.4",
            "body": body,
        });
        if let Some(log_url) = &signal.log_url {
            card["actions"] = serde_json::json!([{
                "type": "Action.OpenUrl",
                "title": "View logs",
                "url": log_url,
            }]);
        }

        serde_json::json!({
            "type": "message",
            "attachments": [{
                "contentType": "application/vnd.microsoft.card.adaptive",
                "contentUrl": null,
                "content": card,
            }],
        })
    }
}

fn fact(title: &str, value: &str) -> serde_json::Value {
    serde_json::json!({ "title": title, "value": value })
}

#[async_trait::async_trait]
impl Provider for TeamsProvider {
    fn name(&self) -> &'static str {
        "teams"
    }

    fn preview(&self, signal: &Signal) -> Option<serde_json::Value> {
        Some(self.card_for(signal))
    }

    async fn send(&self, signal: &Signal) -> Result<(), SignalError> {
        let resp = self
            .client
            .post(&self.webhook_url)
            .json(&self.card_for(signal))
            .send()
            .await
            .map_err(|e| SignalError::Delivery(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(SignalError::Delivery(format!(
                "teams webhook returned status {}",
                resp.status()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::SignalInput;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn signal(severity: Severity) -> Signal {
        Signal::from_input(SignalInput {
            source: "airflow".into(),
            event_type: "task_failed".into(),
            title: "dbt model failed".into(),
            message: Some("model orders timed out".into()),
            severity,
            environment: Some("prod".into()),
            log_url: Some("https://logs.example.test/run/1".into()),
            ..Default::default()
        })
    }

    fn facts(card: &serde_json::Value) -> Vec<(String, String)> {
        card["attachments"][0]["content"]["body"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["type"] == "FactSet")
            .unwrap()["facts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    f["title"].as_str().unwrap().to_string(),
                    f["value"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn severities_map_to_card_colours() {
        assert_eq!(TeamsProvider::color_for(Severity::Info), "default");
        assert_eq!(TeamsProvider::color_for(Severity::Success), "good");
        assert_eq!(TeamsProvider::color_for(Severity::Warning), "warning");
        assert_eq!(TeamsProvider::color_for(Severity::Error), "attention");
        assert_eq!(TeamsProvider::color_for(Severity::Critical), "attention");
    }

    #[test]
    fn card_has_title_message_facts_and_log_link() {
        let card = TeamsProvider::new(String::new()).card_for(&signal(Severity::Error));
        assert_eq!(card["type"], "message");
        let attachment = &card["attachments"][0];
        assert_eq!(
            attachment["contentType"],
            "application/vnd.microsoft.card.adaptive"
        );
        let content = &attachment["content"];
        assert_eq!(content["type"], "AdaptiveCard");
        assert_eq!(content["body"][0]["text"], "dbt model failed");
        assert_eq!(content["body"][0]["color"], "attention");
        assert_eq!(content["body"][1]["text"], "model orders timed out");
        assert_eq!(
            content["actions"][0]["url"],
            "https://logs.example.test/run/1"
        );
        assert_eq!(
            facts(&card),
            vec![
                ("Severity".to_string(), "error".to_string()),
                ("Source".to_string(), "airflow".to_string()),
                ("Event".to_string(), "task_failed".to_string()),
                ("Environment".to_string(), "prod".to_string()),
            ]
        );
    }

    #[test]
    fn minimal_signal_has_no_message_block_or_actions() {
        let card = TeamsProvider::new(String::new()).card_for(&Signal::from_input(SignalInput {
            source: "ci".into(),
            event_type: "build_failed".into(),
            title: "main is red".into(),
            ..Default::default()
        }));
        let content = &card["attachments"][0]["content"];
        let body = content["body"].as_array().unwrap();
        assert_eq!(body.len(), 2, "title and facts only");
        assert_eq!(body[1]["type"], "FactSet");
        assert!(content.get("actions").is_none());
    }

    #[tokio::test]
    async fn posts_the_card_to_the_webhook() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/workflows/abc"))
            .and(header("content-type", "application/json"))
            .and(body_partial_json(serde_json::json!({ "type": "message" })))
            .respond_with(ResponseTemplate::new(202))
            .expect(1)
            .mount(&server)
            .await;

        TeamsProvider::new(format!("{}/workflows/abc", server.uri()))
            .send(&signal(Severity::Warning))
            .await
            .expect("202 is success");
    }

    #[tokio::test]
    async fn non_success_status_is_a_delivery_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let err = TeamsProvider::new(server.uri())
            .send(&signal(Severity::Error))
            .await
            .expect_err("403 must fail");
        assert!(matches!(err, SignalError::Delivery(_)));
        assert!(err.to_string().contains("403"));
    }

    #[test]
    fn title_and_text_templates_replace_the_text_blocks() {
        let provider = TeamsProvider::new(String::new()).with_templates(template::for_test(
            "teams",
            &[
                ("title", "{{ severity|upper }}: {{ title }}"),
                ("text", "{{ message }} - see {{ log_url }}"),
            ],
        ));
        let card = provider.card_for(&signal(Severity::Error));
        let body = &card["attachments"][0]["content"]["body"];
        assert_eq!(body[0]["text"], "ERROR: dbt model failed");
        assert_eq!(
            body[1]["text"],
            "model orders timed out - see https://logs.example.test/run/1"
        );
        // The facts are not templated.
        assert!(facts(&card).contains(&("Source".to_string(), "airflow".to_string())));
    }
}
