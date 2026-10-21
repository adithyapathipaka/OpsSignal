# opssignal

Turn application and workflow events into routed operational notifications:
Slack, webhooks, PagerDuty, Microsoft Teams, Opsgenie, email and the console,
from one event model. The routing, retry and delivery logic is a Rust core,
so it behaves the same from Python, the CLI and the C library.

> **Status: beta.** The Python bindings with the Airflow and dbt adapters are
> the tested path today.

## Install

```bash
pip install --pre opssignal          # local and https:// configs
pip install --pre 'opssignal[aws]'   # + S3, AppConfig, DynamoDB, Secrets Manager, SES
pip install --pre 'opssignal[gcp]'   # + Cloud Storage, Firestore, Secret Manager
pip install --pre 'opssignal[azure]' # + Blob Storage, App Configuration, Cosmos DB, Key Vault
pip install --pre 'opssignal[all]'   # all three clouds
```

Wheels cover CPython 3.9 and later on Linux (x86_64, aarch64, glibc and
musl), macOS on Apple Silicon and Windows x86_64. Other platforms, such as
Intel Macs, build from the sdist, which needs a Rust toolchain.

## Send a signal

```python
import opssignal

opssignal.notify(
    source="billing-api",
    event_type="payment_failed",
    title="Card declined for order 1042",
    severity="error",
)
```

## Airflow

```python
from opssignal.integrations.airflow import failure_callback

default_args = {"on_failure_callback": failure_callback}
```

## Route it

Point `SIGNAL_CONFIG` at a `signal.yaml` (a local path, `https://`, `s3://`,
`gs://`, `az://` and more), or put one in the working directory:

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

No config, or no matching route? Signals go to the console, so nothing is
silently dropped.

## Links

- Documentation: https://adithyapathipaka.github.io/OpsSignal/
- Source and issues: https://github.com/adithyapathipaka/OpsSignal
- License: Apache-2.0
