//! Background config reload.
//!
//! Polls the config source on a fixed interval and swaps in a new
//! [`Pipeline`] when the text changes. Polling (rather than inotify) is
//! deliberate: a Kubernetes ConfigMap mount updates by swapping a
//! symlink, which file watchers often miss, and the same loop then
//! covers remote sources too.
//!
//! The storage section is not reloaded: reopening the sink under live
//! traffic could lose writes, so a changed `storage:` block is logged
//! and applied on the next restart.

use crate::config::StorageConfig;
use crate::source::{self, ConfigSource};
use crate::{Pipeline, SharedPipeline};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct Reloader {
    pub source: ConfigSource,
    pub cache: Option<PathBuf>,
    pub every: Duration,
    /// The text the current pipeline was built from. `None` when the
    /// client started on the default config, so the first good fetch
    /// is always applied.
    pub last_text: Option<String>,
    pub storage: StorageConfig,
    pub pipeline: SharedPipeline,
}

pub(crate) fn spawn(runtime: &tokio::runtime::Runtime, mut reloader: Reloader) {
    runtime.spawn(async move {
        let mut ticker = tokio::time::interval(reloader.every);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // the first tick fires immediately
        loop {
            ticker.tick().await;
            reloader.check().await;
        }
    });
}

impl Reloader {
    /// One poll. Every failure keeps the pipeline already in place.
    pub(crate) async fn check(&mut self) {
        let text = match self.source.fetch().await {
            Ok(text) => text,
            Err(e) => {
                tracing::warn!("config reload skipped: {e}; keeping the current config");
                return;
            }
        };
        if self.last_text.as_deref() == Some(text.as_str()) {
            return;
        }
        let config = match source::parse(&self.source, &text) {
            Ok(config) => config,
            Err(e) => {
                tracing::error!("config reload rejected: {e}; keeping the current config");
                return;
            }
        };
        if config.storage != self.storage {
            tracing::warn!(
                "{}: storage settings changed; they take effect after a restart",
                self.source
            );
        }
        let next = Arc::new(Pipeline::from_config(&config));
        *self.pipeline.write().unwrap_or_else(|e| e.into_inner()) = next;
        if let Some(cache) = &self.cache {
            source::write_cache(cache, &text);
        }
        self.last_text = Some(text);
        tracing::info!("reloaded config from {}", self.source);
    }
}
