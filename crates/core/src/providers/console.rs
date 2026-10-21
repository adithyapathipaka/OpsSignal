use crate::delivery::Provider;
use crate::signal::{Signal, SignalError};
use crate::template::{self, Templates};

/// Always-available fallback. Per the architecture review: unmatched
/// signals must go SOMEWHERE — a notification SDK that silently drops
/// signals is worse than having no SDK at all. Console is the
/// guaranteed-to-work last resort.
#[derive(Default)]
pub struct ConsoleProvider {
    templates: Option<Templates>,
}

impl ConsoleProvider {
    pub fn new() -> Self {
        Self::default()
    }

    fn line_for(&self, signal: &Signal) -> String {
        template::render(&self.templates, "text", signal, &[]).unwrap_or_else(|| {
            format!(
                "[signal:{}] {} {} — {}",
                signal.source, signal.severity, signal.event_type, signal.title
            )
        })
    }

    /// Uses `templates` for the parts they set; see [`crate::template`].
    pub fn with_templates(mut self, templates: Option<Templates>) -> Self {
        self.templates = templates;
        self
    }
}

#[async_trait::async_trait]
impl Provider for ConsoleProvider {
    fn name(&self) -> &'static str {
        "console"
    }

    fn preview(&self, signal: &Signal) -> Option<serde_json::Value> {
        Some(serde_json::json!({ "text": self.line_for(signal) }))
    }

    async fn send(&self, signal: &Signal) -> Result<(), SignalError> {
        println!("{}", self.line_for(signal));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::SignalInput;

    #[tokio::test]
    async fn console_always_succeeds() {
        let signal = Signal::from_input(SignalInput {
            source: "x".into(),
            event_type: "y".into(),
            title: "z".into(),
            ..Default::default()
        });
        assert_eq!(ConsoleProvider::new().name(), "console");
        ConsoleProvider::new()
            .send(&signal)
            .await
            .expect("console never fails");
    }

    #[tokio::test]
    async fn text_template_is_accepted() {
        let provider = ConsoleProvider::new()
            .with_templates(template::for_test("console", &[("text", "{{ title }}")]));
        provider
            .send(&Signal::from_input(SignalInput {
                source: "t".into(),
                event_type: "e".into(),
                title: "hello".into(),
                ..Default::default()
            }))
            .await
            .expect("console never fails");
    }
}
