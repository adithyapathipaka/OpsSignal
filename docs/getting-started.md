# Getting started

## Python and Airflow

Install the package (it ships a compiled Rust extension):

```bash
pip install opssignal
```

To load `signal.yaml` from a cloud bucket, add the matching extra:
`opssignal[aws]`, `opssignal[gcp]` or `opssignal[azure]` (see
[Installing cloud support](configuration.md#installing-cloud-support)).

!!! note
    The package is not published to PyPI yet. Until it is, build it from a
    checkout with `uv sync --dev && (cd python && uv run --project .. maturin develop)`.

Send a signal directly:

```python
from opssignal import notify

notify(
    source="airflow",
    event_type="task_failed",
    title="dbt model orders failed",
    severity="error",          # info | success | warning | error | critical
    message="relation does not exist",
    environment="prod",
)
```

Or drop the Airflow adapter into a DAG:

```python
from opssignal.integrations.airflow import failure_callback

default_args = {
    "on_failure_callback": failure_callback,
}
```

`notify()` is fire-and-forget: it validates the input, hands the signal to a
background runtime and returns immediately, so a slow Slack webhook never
delays the task being monitored.

- Invalid input (empty `source`, `event_type` or `title`) raises `ValueError`.
- A broken `signal.yaml` raises `RuntimeError` on every call until it is fixed.
- An unknown `severity` string falls back to `info` rather than raising.

!!! warning "Short-lived processes"
    Because delivery happens in the background, a signal can be lost if the
    process exits immediately after `notify()`. A `flush()` call is planned.

## Command line

```bash
cargo install --path crates/cli   # installs the `opssignal` binary
opssignal notify --source ci --event-type build_failed --severity error --title "main is red"
```

The CLI waits for delivery (up to 5 seconds) and exits non-zero if it fails.
See [CLI](cli.md).

## No configuration needed

With no `signal.yaml`, every signal goes to the console provider and is
recorded in `./signals.db`. Add a config file when you want Slack or webhooks;
see [Configuration](configuration.md).
