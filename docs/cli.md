# CLI

The `opssignal` binary sends a signal from a shell, CI job or cron entry. It
uses the same Rust core, config and routing as the Python package.

## Install

```bash
cargo install --path crates/cli
```

## `opssignal notify`

```bash
opssignal notify \
  --source ci \
  --event-type build_failed \
  --severity error \
  --title "main is red" \
  --message "3 tests failed" \
  --environment prod
```

| Flag | Required | Default | Notes |
|---|---|---|---|
| `--source` | yes | | Where the event came from, such as `ci` or `airflow`. |
| `--event-type` | yes | | What happened, such as `build_failed`. |
| `--title` | yes | | One-line summary. Must not be blank. |
| `--severity` | no | `info` | `info`, `success`, `warning`, `error` or `critical` (case-insensitive). |
| `--message` | no | | Longer detail. |
| `--environment` | no | | Used by routes that match on `environment`. |
| `--metadata` | no | | A JSON object, such as `'{"dag_id": "orders"}'`, for [templates](templates.md) to use. Added in v1.0.0-beta.2. |
| `--config` | no | | Path to a config file. Overrides `$SIGNAL_CONFIG` and `./signal.yaml`. |

Unlike the Python `notify()`, the CLI waits for delivery, with a 5 second
timeout, so a CI step can fail when an alert didn't go out. If a
[grouped route](configuration.md#grouping-and-digests) holds the signal, the
CLI sends it before exiting rather than waiting for `group_wait`, since no
later run can join the group; use `notify-batch` to group several signals.

## `opssignal notify-batch`

!!! info "Added in v1.0.0-beta.3"
    Sends many signals in one run, so grouped routes can send one digest.

```bash
dbt_failures_to_json | opssignal notify-batch
opssignal notify-batch failures.ndjson
```

Reads one JSON signal per line from the file, or from stdin when the file is
`-` or left out. Each line has the fields of the C library's JSON payload:
`source`, `event_type`, `title` and `severity` are required, and `message`,
`environment`, `service`, `component`, `team`, `run_id`, `correlation_id`,
`trace_id`, `log_url`, `metadata` and `dedup_key` are optional:

```json
{"source": "dbt", "event_type": "dbt_test_failed", "severity": "error", "title": "not_null_orders_id failed", "run_id": "inv-1"}
```

Every line is checked before anything is sent, so a bad line exits `2` with
its line number and sends nothing. The signals are then sent one after
another, the open groups are sent, and stderr ends with a summary such as
`sent 40 signal(s); 40 held for 1 group send(s)`. The exit code covers every
signal and digest: `1` if any of them failed or was only partly delivered.

## `opssignal preview`

!!! info "Added in v1.0.0-beta.2"
    Prints what each provider would be sent, for checking
    [message templates](templates.md).

```bash
opssignal preview \
  --source airflow \
  --event-type task_failed \
  --severity error \
  --title "load failed" \
  --metadata '{"dag_id": "orders"}'
```

Takes the same flags as `notify`, plus `--provider NAME` to show one provider
whether or not a route picks it. Prints a JSON list of
`{"provider": ..., "payload": ...}` in delivery order. Nothing is sent and
nothing is recorded. Credentials in a payload, such as PagerDuty's
`routing_key`, are shown as `<redacted>`. Email's payload is its subject and
body. Exits `2` on bad input, a config that fails to load, or a `--provider`
that isn't configured.

## `opssignal resolve` and `opssignal acknowledge`

!!! info "Added in v1.0.0-beta.3"
    Resolve or acknowledge an alert raised earlier. See
    [Acknowledge and resolve](lifecycle.md).

```bash
opssignal resolve --dedup-key etl:nightly --message "Backfilled"
```

Name the alert with `--dedup-key`, or with the `--source`, `--event-type`,
`--title` and `--environment` it was raised with. `--message` adds a note.
The command waits for delivery like `notify` and uses the same exit codes.
When there is no open alert to act on and no route to PagerDuty or Opsgenie,
nothing is sent: stderr shows `skipped: ...` and the exit code is `0`.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Delivered, or held back by a route's [`dedup` or `rate_limit`](configuration.md#dedup-and-rate-limits) (stderr then shows `suppressed: ...` with the reason), or an acknowledge or resolve with nothing to act on (`skipped: ...`). A signal held for a [digest](configuration.md#grouping-and-digests) counts by how the digest went. |
| `1` | Delivery failed or timed out, or a [`fanout`](configuration.md#delivery-modes) route reached only some of its providers. The outcome is still recorded in the sink, and stderr lists each provider's result. |
| `2` | Bad input (blank title, unknown severity) or a config that fails to load. |

Errors are printed to stderr as `error: ...`.
