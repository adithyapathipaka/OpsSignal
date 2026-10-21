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
timeout, so a CI step can fail when an alert didn't go out.

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

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Delivered, or held back by a route's [`dedup` or `rate_limit`](configuration.md#dedup-and-rate-limits) (stderr then shows `suppressed: ...` with the reason). |
| `1` | Delivery failed or timed out, or a [`fanout`](configuration.md#delivery-modes) route reached only some of its providers. The outcome is still recorded in the sink, and stderr lists each provider's result. |
| `2` | Bad input (blank title, unknown severity) or a config that fails to load. |

Errors are printed to stderr as `error: ...`.
