# OpsSignal

A Rust-powered SDK for turning application and workflow events into structured,
routed operational notifications — Slack, webhooks, and more, from a single
event model.

> **Status: beta (latest release v1.0.0-beta.1).** Config keys, the Python
> API and the SQLite schema may still change before 1.0.0. The most-tested
> integration targets are **Airflow** and **dbt**, via the Python bindings. A
> CLI and a C ABI exist; Java and Go bindings are designed for (see
> `crates/ffi` and the architecture docs) but **not yet built**. What changed
> in each release is in [`CHANGELOG.md`](CHANGELOG.md).

## What this is

```text
Application event happens
        ↓
Rust core: validate → route → persist → deliver (sync or async, your choice)
        ↓
Notification providers (Console, Webhook, Slack, PagerDuty, Microsoft Teams, Opsgenie, Email)
```

Existing tools solve parts of this problem — Alertmanager routes alerts but
isn't an embeddable application SDK; Airflow callbacks are Python-only;
Slack/Teams SDKs are provider-specific. This project combines a shared Rust
core, a common signal model, and provider abstraction so the same routing and
retry logic works the same way regardless of calling language.

## What this is not

This SDK is **not** an event bus, message queue, or workflow orchestrator. It
does not do event sourcing, complex event processing, or long-running
scheduling. If you need that, use Kafka/SQS/Airflow itself — this SDK sits
downstream of those, converting their events into notifications.

## Quickstart (Python / Airflow)

```bash
pip install opssignal            # local and https:// configs
pip install 'opssignal[aws]'     # + s3:// (also [gcp], [azure], [all])
```

```python
from opssignal.integrations.airflow import failure_callback

default_args = {
    "on_failure_callback": failure_callback,
}
```

Configure routing in `signal.yaml` (see `python/examples/signal.example.yaml`):

```yaml
providers:
  slack:
    webhook_url: "${SLACK_WEBHOOK_URL}"

routes:
  - match:
      severity: [error, critical]
      environment: prod
    providers: [slack]
```

No config? Signals fall back to the console provider — nothing is ever
silently dropped.

## Documentation

Full docs, including configuration, CLI reference, architecture diagrams and
the Rust API reference: **https://adithyapathipaka.github.io/OpsSignal/**

## Architecture

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the full design:
signal model, sync/async delivery semantics, the storage/sink boundary, and
the FFI panic-safety rules every binding must follow.

## Roadmap

| Release | Scope |
|---|---|
| v1.0.0-beta.1 (released) | Rust core, Console/Webhook/Slack/PagerDuty/Teams/Opsgenie/Email providers, SQLite sink, Python bindings, Airflow and dbt adapters, CLI, C ABI |
| Next beta | Fan-out delivery mode for routes |
| Later | Dedup, retry/backoff tuning, Alertmanager provider; Postgres sink; Dagster + Prefect adapters |
| Only on demand | Java/Go bindings — **only if real external demand appears** |

Java and Go are explicitly **not** committed for any near-term release. The
C ABI in `crates/ffi` is designed to make those bindings straightforward to
add later, but building them speculatively with zero users isn't a good use
of a small maintainer team's time.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md).

## License

Apache-2.0 — see [`LICENSE`](LICENSE). This SDK is fully open source with no
contributor agreement: fork it, modify it, build on it, sell it, do what you
want. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the project's open-core
model — a separate, closed-source hosted product is planned, but it lives
outside this repository and doesn't change the terms of anything here.
