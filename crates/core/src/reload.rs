//! Background config reload.
//!
//! Polls every file of the config chain on a fixed interval and swaps
//! in a new [`Pipeline`] when any of them changes. Polling (rather than
//! inotify) is deliberate: a Kubernetes ConfigMap mount updates by
//! swapping a symlink, which file watchers often miss, and the same
//! loop then covers remote sources too.
//!
//! The storage section is not reloaded: reopening the sink under live
//! traffic could lose writes, so a changed `storage:` block is logged
//! and applied on the next restart.

use crate::chain::{self, Mode};
use crate::config::StorageConfig;
use crate::source::{self, ConfigSource, LoadOptions, Texts};
use crate::{Pipeline, SharedPipeline};
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct Reloader {
    /// The files named in `$SIGNAL_CONFIG`; their `extends` are
    /// re-resolved on every poll, so a new parent is picked up too.
    pub roots: Vec<ConfigSource>,
    pub opts: LoadOptions,
    pub every: Duration,
    /// The texts the current pipeline was built from. `None` when the
    /// client started on the default config, so the first good fetch
    /// is always applied.
    pub last_texts: Option<Texts>,
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
        let files = match chain::walk(&self.roots, Mode::Reload).await {
            Ok(files) => files,
            Err(e) => {
                tracing::warn!(
                    "config reload skipped: {}; keeping the current config",
                    e.err
                );
                return;
            }
        };
        let texts = chain::texts(&files);
        if self.last_texts.as_ref() == Some(&texts) {
            return;
        }
        let config = match chain::merge(&files) {
            Ok(config) => config,
            Err(e) => {
                tracing::error!(
                    "config reload rejected: {}; keeping the current config",
                    e.err
                );
                return;
            }
        };
        let names = self.describe();
        if config.storage != self.storage {
            tracing::warn!("{names}: storage settings changed; they take effect after a restart");
        }
        let next = Arc::new(Pipeline::from_config(&config));
        *self.pipeline.write().unwrap_or_else(|e| e.into_inner()) = next;
        for (fetched, _) in &files {
            if let Some(path) = self.opts.cache_path(&fetched.source) {
                source::write_cache(&path, &fetched.text);
            }
        }
        self.last_texts = Some(texts);
        tracing::info!("reloaded config from {names}");
    }

    fn describe(&self) -> String {
        let names: Vec<String> = self.roots.iter().map(ToString::to_string).collect();
        names.join(", ")
    }
}
