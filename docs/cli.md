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
| `--config` | no | | Path to a config file. Overrides `$SIGNAL_CONFIG` and `./signal.yaml`. |

Unlike the Python `notify()`, the CLI waits for delivery, with a 5 second
timeout, so a CI step can fail when an alert didn't go out.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | Delivered. |
| `1` | Delivery failed or timed out, or a [`fanout`](configuration.md#delivery-modes) route reached only some of its providers. The outcome is still recorded in the sink, and stderr lists each provider's result. |
| `2` | Bad input (blank title, unknown severity) or a config that fails to load. |

Errors are printed to stderr as `error: ...`.
