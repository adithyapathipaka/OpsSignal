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

Add `"on_success_callback": success_callback` (from the same module) to
resolve the failure alert once the task succeeds; see
[Acknowledge and resolve](lifecycle.md).

`notify()` also takes the rest of the signal's context as keyword arguments:
`service`, `component`, `team`, `run_id`, `correlation_id`, `trace_id`,
`log_url`, `dedup_key`, and `metadata` (any JSON-serialisable object, usually a
dict; dates and other values become strings).

`notify()` is fire-and-forget: it validates the input, hands the signal to a
background runtime and returns immediately, so a slow Slack webhook never
delays the task being monitored.

- Invalid input (empty `source`, `event_type` or `title`) raises `ValueError`.
- A broken `signal.yaml` raises `RuntimeError` on every call until it is fixed.
- An unknown `severity` string falls back to `info` rather than raising.

!!! note "Short-lived processes"
    Delivery happens in the background. Signals still in flight when the
    interpreter exits are flushed for up to 5 seconds; call
    `opssignal.flush(timeout)` to wait at a point of your choosing.

### Waiting for the result

!!! info "Added in v1.0.0-beta.3"

`opssignal.send()` takes the same arguments as `notify()` plus `timeout`
(seconds, default 30), waits for delivery and tells you how it went. Use it
when the caller has to know, such as a CI step that must fail if nobody was
paged, or a script that logs where each alert went:

```python
import opssignal

result = opssignal.send(
    source="ci",
    event_type="deploy_failed",
    title="Deploy of orders-api failed",
    severity="critical",
    environment="prod",
    timeout=10,
)
if not result.delivered:
    for d in result.deliveries:
        print(d.provider, d.status, d.error)
```

`result.status` is one of:

| Status | Meaning |
|---|---|
| `delivered` | Every provider the signal was routed to got it. |
| `partial` | A [fan-out](configuration.md) route reached some providers but not all. |
| `failed` | No provider got it, or the timeout ran out first. |
| `skipped` | Every matching route held it back by `dedup` or `rate_limit`; `result.suppressed` says why. |
| `grouped` | Held for a digest by a route's `group_by` or `group_wait`; `result.grouped` names the routes. The digest goes out after `group_wait` or on `flush()`. |

The result also has `provider` (the providers that delivered, comma
separated), `error`, `attempts`, `deliveries` (one entry per provider with
`provider`, `status`, `error` and `attempts`), `suppressed`, `grouped`,
`signal_id` (its row in the
SQLite history) and `dedup_key` (the incident key, given or computed).
`result.to_dict()` returns it all as plain dicts for logging or JSON.

A failed delivery is reported in the result, not raised. `send()` raises
`ValueError` for invalid input or timeout and `RuntimeError` when there is no
usable config or the signal could not be recorded. Keep `notify()` in
Airflow callbacks and other places where an alert must never slow the task
down.

## dbt

Report a run's failures after any dbt command. dbt writes
`target/run_results.json` on every run, so this works with dbt Core, dbt Fusion
and artifacts downloaded from dbt Cloud, from a shell, CI job or orchestrator
task:

```bash
dbt build; status=$?
python -m opssignal.integrations.dbt target/run_results.json --environment prod
exit $status
```

The reporter always exits 0, so the job still fails or passes on dbt's own
status. From Python, call
`opssignal.integrations.dbt.report_run_results("target/run_results.json", environment="prod")`.

When dbt runs in-process (dbt-core 1.5+), pass the callback to `dbtRunner` to
alert as each node fails, while the rest of the run continues:

```python
from dbt.cli.main import dbtRunner
from opssignal.integrations.dbt import make_callback

dbtRunner(callbacks=[make_callback(environment="prod")]).invoke(["build"])
```

Both send one signal per failed node and per warned test:

| dbt status | Severity | `event_type` | Title |
|---|---|---|---|
| `error`, `fail` | `error` | `dbt_<resource>_failed`, such as `dbt_model_failed` | `dbt model failed: orders` |
| `warn` | `warning` | `dbt_<resource>_warned`, such as `dbt_test_warned` | `dbt test warned: <test name>` |

`service` is the dbt project, `component` the node's `unique_id`, `run_id` the
dbt invocation ID, and `metadata` holds the status, resource type, failure
count, execution time, relation, dbt version, command and target. Skipped nodes
are not reported: they were skipped because an upstream node failed, and that
node has its own signal. The title is stable for a node, so a node that keeps
failing run after run is deduplicated by providers that support it.

A broken source can fail hundreds of tests in one run. Past 20 signals (change
it with `--max-signals` or `max_signals=`, 0 for no limit) the rest are rolled
into one `dbt_run_degraded` signal that lists their IDs.

Route dbt signals like any other, for example:

```yaml
routes:
  - match:
      source: dbt
      severity: [error]
      environment: prod
    providers: [pagerduty, slack]
  - match:
      source: dbt
      severity: [warning]
    providers: [slack]
```

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
