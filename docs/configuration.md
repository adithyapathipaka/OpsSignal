# Configuration

## Where the config comes from

OpsSignal looks for its config in this order:

1. The path in the `SIGNAL_CONFIG` environment variable. If it is set, the
   file must exist, or loading fails.
2. `./signal.yaml` in the current working directory.
3. Nothing found: the zero-config default (console provider, SQLite sink at
   `./signals.db`).

The CLI also accepts `--config <path>`, which overrides both.

The config is loaded once per process, on the first signal.

## Example

```yaml
providers:
  slack:
    webhook_url: "${SLACK_WEBHOOK_URL}"
  webhook:
    url: "https://ops.example.com/hooks/opssignal"

routes:
  - match:
      severity: [error, critical]
      environment: prod
    providers: [slack]
  - match:
      source: ci
    providers: [webhook]

storage:
  sink: sqlite
  path: "./signals.db"
```

`signal.yaml` is gitignored in this repository. Commit only files that use
`${VAR}` placeholders for secrets, like
[`python/examples/signal.example.yaml`](https://github.com/adithyapathipaka/OpsSignal/blob/main/python/examples/signal.example.yaml).

## `providers`

| Key | Fields | Notes |
|---|---|---|
| `console` | `enabled` (optional) | Always registered, because it is the routing fallback. It cannot be turned off. |
| `slack` | `webhook_url` | Slack incoming-webhook URL. |
| `webhook` | `url` | Receives the full signal as a JSON `POST`. |

Unknown provider keys are rejected, so a typo such as `slak:` fails at load
time instead of silently dropping a provider.

## `routes`

Each route has a `match` block and a list of `providers`. Every condition in
`match` must hold; an omitted condition matches anything.

| Field | Type | Matches when |
|---|---|---|
| `severity` | list of `info`, `success`, `warning`, `error`, `critical` | the signal's severity is in the list |
| `environment` | string | the signal's `environment` equals it |
| `source` | string | the signal's `source` equals it |

The matching routes' providers form an ordered list (in file order, with
consecutive duplicates removed). Delivery tries them in that order and
**stops at the first one that succeeds**. Each provider gets up to 3
attempts with exponential backoff before the next one is tried. Later
providers are therefore fallbacks, not extra copies.

If no route matches, the list is just `console`. That fallback is built into
the routing engine and cannot be configured away.

A route that names a provider not configured under `providers` is a load-time
error, as is a route with an empty `providers` list.

## `storage`

| Field | Default | Notes |
|---|---|---|
| `sink` | `sqlite` | Only `sqlite` exists today. Postgres is on the roadmap. |
| `path` | `./signals.db` | Created if missing. |

Every signal is written to the sink with its delivery status (`delivered` or
`failed`), the provider that delivered it, and any error.

## Environment variables in values

`${NAME}` anywhere in a string value is replaced with the value of the
environment variable `NAME`. Several placeholders can appear in one value.

- An **unset** variable is a load-time error that names the variable. It is
  never silently replaced with an empty string.
- Substitution happens after YAML parsing, so secrets that contain quotes,
  colons or newlines can't break the file's structure.

## Load-time errors

These all stop the config from loading, and the message says which rule failed:

- invalid YAML
- an unknown key anywhere in the file
- an unset `${VAR}`, or an unterminated or empty `${}` placeholder
- a route that references an unconfigured provider, or lists no providers
- `SIGNAL_CONFIG` pointing at a path that is not a file
