# OpsSignal

A Rust-powered SDK for turning application and workflow events into structured,
routed operational notifications: Slack, webhooks, and more, from a single
event model.

!!! warning "Status: early alpha (v0.1 in progress)"
    The one real, tested integration target today is **Airflow**, via the
    Python bindings. The CLI and a C ABI (for future Java/Go bindings) exist
    but are young. Treat this as an Airflow/dbt-shop tool for now, not yet the
    generic "any backend application" SDK it aims to become.

## What it does

```mermaid
flowchart LR
    A[Application or workflow event] --> B[Rust core<br/>validate, route, deliver, record]
    B --> C[Console]
    B --> D[Slack]
    B --> E[Webhook]
    B --> G[PagerDuty]
    B --> H[Microsoft Teams]
    B -.-> F[(SQLite sink)]
```

Existing tools solve parts of this problem. Alertmanager routes alerts but
isn't an embeddable application SDK, Airflow callbacks are Python-only, and
Slack or Teams SDKs are provider-specific. OpsSignal combines a shared Rust
core, a common signal model and a provider abstraction, so the same routing
and retry logic behaves the same way from every calling language.

## What it is not

OpsSignal is **not** an event bus, message queue or workflow orchestrator. It
does not do event sourcing, complex event processing or long-running
scheduling. It sits downstream of those systems and turns their events into
notifications.

## Where to go next

- [Getting started](getting-started.md): install, send a first signal from Python or the CLI.
- [Configuration](configuration.md): `signal.yaml`, providers, routes, storage.
- [Providers](providers.md): Slack, Teams, PagerDuty, webhook and console.
- [CLI](cli.md): flags and exit codes for shell and CI use.
- [Architecture](ARCHITECTURE.md): components, delivery semantics, design rules.
- [API reference](reference.md): generated Rust docs for `opssignal-core`.

## Roadmap

| Version | Scope |
|---|---|
| v0.1 (current) | Rust core, Console/Webhook/Slack/PagerDuty/Teams providers, SQLite sink, Python bindings, Airflow and dbt adapters, CLI |
| v0.2 | Dedup, retry/backoff tuning, Alertmanager provider |
| v0.3 | Postgres sink |
| v0.4 | Dagster + Prefect adapters |
| v0.5+ | Java/Go bindings, only if real external demand appears |

## License

Apache-2.0, with no contributor agreement. See
[LICENSE](https://github.com/adithyapathipaka/OpsSignal/blob/main/LICENSE).
