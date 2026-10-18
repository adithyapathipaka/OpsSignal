//! Layered configs: a file can `extends:` one or more parents, and
//! `$SIGNAL_CONFIG` can list several files. A typical enterprise setup
//! is one shared default, a file per team that extends it, and shared
//! alert files that several teams extend.
//!
//! The chain is flattened depth first, parents before children and
//! earlier entries of a list before later ones, so the result runs from
//! lowest to highest priority. A file reached twice (two teams that
//! extend the same default) is loaded once, at its first position.

use crate::config::Layer;
use crate::signal::SignalError;
use crate::source::{self, ConfigSource, LoadOptions};
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;

/// Deepest `extends` chain accepted, counting the root.
pub const MAX_DEPTH: usize = 8;
/// Most files one chain may load.
pub const MAX_FILES: usize = 64;

pub(crate) enum Mode<'a> {
    /// Startup: a remote file that can't be fetched or parsed falls
    /// back to its cached copy.
    Startup(&'a LoadOptions),
    /// Startup, second try: every remote file comes from its cached
    /// copy, for when the fresh files load but don't merge.
    CacheOnly(&'a LoadOptions),
    /// Background reload: any failure keeps the config in place, so no
    /// fallback is needed.
    Reload,
}

/// One file of the chain, with the raw text it was built from.
pub(crate) struct Fetched {
    pub source: ConfigSource,
    pub text: String,
}

pub(crate) struct ChainError {
    pub err: SignalError,
    /// True for failures a person has to fix (a local file that is
    /// missing or broken, a cycle, an unsupported scheme), which stop
    /// startup. False for a remote file that is unreachable or broken
    /// with no cached copy, which falls back to the default config.
    pub fatal: bool,
}

impl ChainError {
    fn fatal(err: SignalError) -> Self {
        ChainError { err, fatal: true }
    }
}

/// Loads every file reachable from `roots`, lowest priority first.
pub(crate) async fn walk(
    roots: &[ConfigSource],
    mode: Mode<'_>,
) -> Result<Vec<(Fetched, Layer)>, ChainError> {
    let mut walk = Walk {
        mode,
        stack: Vec::new(),
        seen: HashSet::new(),
        order: Vec::new(),
    };
    for root in roots {
        walk.visit(root.clone()).await?;
    }
    Ok(walk.order)
}

/// `(source, raw text)` of each walked file, to tell when one changes.
pub(crate) fn texts(files: &[(Fetched, Layer)]) -> source::Texts {
    files
        .iter()
        .map(|(f, _)| (f.source.to_string(), f.text.clone()))
        .collect()
}

/// Merges a walked chain into one config.
pub(crate) fn merge(files: &[(Fetched, Layer)]) -> Result<crate::config::Config, ChainError> {
    let any_local = files.iter().any(|(f, _)| !f.source.is_remote());
    crate::config::Config::from_layers(
        files
            .iter()
            .map(|(f, layer)| (Some(f.source.to_string()), layer.clone())),
    )
    .map_err(|err| ChainError {
        err,
        fatal: any_local,
    })
}

struct Walk<'a> {
    mode: Mode<'a>,
    stack: Vec<String>,
    seen: HashSet<String>,
    order: Vec<(Fetched, Layer)>,
}

impl<'a> Walk<'a> {
    fn visit<'s>(
        &'s mut self,
        source: ConfigSource,
    ) -> Pin<Box<dyn Future<Output = Result<(), ChainError>> + Send + 's>>
    where
        'a: 's,
    {
        Box::pin(async move {
            let key = source.to_string();
            if self.stack.contains(&key) {
                let cycle = [self.stack.as_slice(), &[key]].concat().join(" -> ");
                return Err(ChainError::fatal(config_err(format!(
                    "`extends` cycle: {cycle}"
                ))));
            }
            if self.seen.contains(&key) {
                return Ok(());
            }
            if self.stack.len() >= MAX_DEPTH {
                return Err(ChainError::fatal(config_err(format!(
                    "{key}: `extends` chain is deeper than {MAX_DEPTH} files"
                ))));
            }
            if self.seen.len() >= MAX_FILES {
                return Err(ChainError::fatal(config_err(format!(
                    "{key}: config chain loads more than {MAX_FILES} files"
                ))));
            }
            source.check_supported().map_err(ChainError::fatal)?;

            let (text, layer) = self.load(&source).await?;
            self.stack.push(key.clone());
            for parent in &layer.extends {
                let parent = source.join(parent).map_err(ChainError::fatal)?;
                self.visit(parent).await?;
            }
            self.stack.pop();
            self.seen.insert(key);
            self.order.push((Fetched { source, text }, layer));
            Ok(())
        })
    }

    async fn load(&self, source: &ConfigSource) -> Result<(String, Layer), ChainError> {
        if let (Mode::CacheOnly(opts), true) = (&self.mode, source.is_remote()) {
            return read_cache(opts, source).map_err(|err| ChainError { err, fatal: false });
        }
        let fresh = match source.fetch().await {
            Ok(text) => parse(source, &text).map(|layer| (text, layer)),
            Err(e) => Err(e),
        };
        let err = match fresh {
            Ok(loaded) => return Ok(loaded),
            Err(e) => e,
        };
        if !source.is_remote() {
            return Err(ChainError::fatal(err));
        }
        if let Mode::Startup(opts) = &self.mode {
            match read_cache(opts, source) {
                Ok(loaded) => {
                    tracing::warn!("{err}; using the cached copy");
                    return Ok(loaded);
                }
                Err(cache_err) => tracing::warn!("{cache_err}"),
            }
        }
        Err(ChainError { err, fatal: false })
    }
}

fn read_cache(opts: &LoadOptions, source: &ConfigSource) -> Result<(String, Layer), SignalError> {
    let path = opts
        .cache_path(source)
        .ok_or_else(|| config_err(format!("{source}: config cache is off")))?;
    let text = std::fs::read_to_string(&path).map_err(|e| {
        config_err(format!(
            "{source}: no cached copy at {}: {e}",
            path.display()
        ))
    })?;
    let layer = parse(source, &text).map_err(|e| {
        config_err(format!(
            "cached config at {} is unusable: {}",
            path.display(),
            source::message(&e)
        ))
    })?;
    Ok((text, layer))
}

fn parse(source: &ConfigSource, text: &str) -> Result<Layer, SignalError> {
    Layer::from_yaml_str(text).map_err(|e| config_err(format!("{source}: {}", source::message(&e))))
}

fn config_err(msg: impl Into<String>) -> SignalError {
    SignalError::Config(msg.into())
}
