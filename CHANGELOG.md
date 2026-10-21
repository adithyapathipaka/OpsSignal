# Changelog

All notable changes to OpsSignal are listed here, newest first. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions
follow [Semantic Versioning](https://semver.org/). Pre-releases (`-beta.N`)
may still change config keys, the Python API and the SQLite schema.

The Python packages use PEP 440 spelling for the same version, so
`1.0.0-beta.1` is `1.0.0b1` on PyPI.

## [Unreleased]

Changes merged to `main` that are not in a release yet. They will ship in
the next tag.

## [1.0.0-beta.2] - 2026-10-21

Second beta: send one signal to several places at once, shape each
provider's message, and keep noisy routes quiet. Configs written for
`1.0.0-beta.1` keep working unchanged, except that a misspelled key inside
a route's `match` is now reported instead of silently matching everything.

### Added

- **Fan-out delivery:** a route can set `mode: fanout` to deliver to every
  provider it lists at once, each retried on its own, so a provider that
  already got the signal is never sent it again because another failed.
  `failover` stays the default. A fan-out that reaches only some providers
  is reported as partial, with each provider's result, and the CLI exits 1
  ([#26](https://github.com/adithyapathipaka/OpsSignal/pull/26)).
- **Message templates per provider:** an optional `template` block under any
  provider replaces parts of its message with Jinja text, such as Slack's
  `text`, PagerDuty's `summary`, Teams' `title`/`text`, Opsgenie's
  `message`/`description`, email's `subject`/`body`, console's line, or the
  whole webhook JSON `body`. Templates see every signal field plus
  `metadata.*`. Syntax errors, unknown parts and unknown variables fail at
  load time; a template that fails at send time falls back to the built-in
  format, so the alert still goes out. Configs without templates send
  exactly what they did before. Python and the C library get this from the
  config, with no API change
  ([#32](https://github.com/adithyapathipaka/OpsSignal/pull/32)).
- `opssignal preview` prints what each provider would be sent for a signal,
  without sending or recording it, and `opssignal notify`/`preview` take
  `--metadata` as a JSON object
  ([#32](https://github.com/adithyapathipaka/OpsSignal/pull/32)).
- **Dedup and rate limits per route:** `dedup: 30m` sends each incident (dedup
  key) at most once per window, and `rate_limit: {max: 10, per: 1h}` caps how
  many signals a route sends. Held-back signals are stored with status
  `skipped`. The history lives in the SQLite file, so it holds across
  processes on one host (cron, Airflow tasks, the CLI) and across restarts.
  A failed send never suppresses the next one. The CLI exits 0 for a
  suppressed signal and prints the reason
  ([#31](https://github.com/adithyapathipaka/OpsSignal/pull/31)).

### Changed

- Rust API: `ConsoleProvider` is now a struct built with
  `ConsoleProvider::new()`, the `Provider` trait has a `preview` method
  with a default, so existing implementations still compile, and
  `DeliveryResult` gains a `Partial` status with per-provider results.
- The SQLite sink keeps one `deliveries` row per signal and provider, so a
  signal's history shows which providers already have it.
- PyPI pages for `opssignal`, `opssignal-aws`, `opssignal-gcp` and
  `opssignal-azure` get a project description, links, keywords and
  classifiers ([#25](https://github.com/adithyapathipaka/OpsSignal/pull/25)).
  PyPI fixes a version's metadata at its first upload, so this shows from
  the next release; the `1.0.0b1` pages stay without a description.

### Fixed

- An unknown key inside a route's `match` (such as `enviroment: prod`) is now
  a load-time error. It used to be ignored, so the route matched every signal
  ([#31](https://github.com/adithyapathipaka/OpsSignal/pull/31)).

## [1.0.0-beta.1] - 2026-10-20

First public build, published to PyPI (`pip install --pre opssignal`) and
crates.io (`opssignal-core`, `opssignal-cli`).

### Added

- **Providers:** Slack, Microsoft Teams, PagerDuty (Events API v2),
  Opsgenie, email (SMTP, or Amazon SES with `opssignal[aws]`), generic
  webhook and console. A route lists providers in order and delivery stops
  at the first one that succeeds, so later providers are fallbacks. Each
  provider gets up to 3 attempts with backoff.
- **Config sources:** local file, `https://`, `s3://`, `gs://`, `az://`,
  AWS AppConfig (`appconfig://`), Azure App Configuration
  (`azappconfig://`), and DynamoDB, Firestore and Cosmos DB items.
- **Live reload** every 60 seconds (`SIGNAL_CONFIG_REFRESH_SECS`); a broken
  edit is logged and skipped. Remote configs are cached locally and used
  when the source is unreachable.
- **Layered configs** with `extends:` or a comma-separated `SIGNAL_CONFIG`
  list: a platform default file plus per-team files.
- **Secrets:** `${VAR}`, `${aws-sm:...}`, `${gcp-sm:...}` and
  `${azure-kv:...}` references, cached in memory only.
- **Reliability:** every signal is stored in SQLite as `pending` before
  delivery, then marked `delivered` or `failed`. In-flight signals are
  flushed on exit (Rust drop, Python `atexit`, C `signal_flush`).
- **Python:** `notify()` with full signal context, an Airflow
  `failure_callback`, and a dbt adapter (`run_results.json` or a
  `dbtRunner` callback).
- **CLI:** `opssignal notify`, which exits non-zero when an alert was not
  delivered.
- **C ABI** in `crates/ffi` for C hosts and future bindings.
- **Packaging:** a cloud-free base package with `opssignal[aws]`, `[gcp]`,
  `[azure]` and `[all]` extras, and matching cargo features for the CLI
  and C library. abi3 wheels for Python 3.9+ on Linux (glibc and musl,
  x86_64 and aarch64), Windows x86_64 and macOS arm64 (the macOS wheel was
  added to `1.0.0b1` on 2026-10-21). The Rust crates need Rust 1.89+.

### Known limitations

- No fan-out: to send one signal to two places, use two routes.
- Signals left `pending` or `failed` are kept but not redelivered later.
- SQLite is the only sink; a team config file can add routes but not
  remove inherited ones.

[Unreleased]: https://github.com/adithyapathipaka/OpsSignal/compare/v1.0.0-beta.2...HEAD
[1.0.0-beta.2]: https://github.com/adithyapathipaka/OpsSignal/compare/v1.0.0-beta.1...v1.0.0-beta.2
[1.0.0-beta.1]: https://github.com/adithyapathipaka/OpsSignal/releases/tag/v1.0.0-beta.1
