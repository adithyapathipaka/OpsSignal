# Acknowledge and resolve

!!! info "Added in v1.0.0-beta.3"
    Signals can acknowledge and resolve the alerts they raised, and the
    Airflow adapter resolves a task's failure alert when the task succeeds.

Every signal has an `action`: `trigger` (the default) raises an alert or
repeats it, `acknowledge` and `resolve` act on an alert raised earlier. The
signal's **dedup key** says which alert. Give the same `dedup_key` you raised
it with, or leave it out and send the same `source`, `event_type`, `title` and
`environment`, from which the core computes the same key.

```python
from opssignal import acknowledge, notify, resolve

notify(source="etl", event_type="load_failed", title="Nightly load failed",
       severity="critical", dedup_key="etl:nightly")

acknowledge("etl:nightly", message="Looking into it")
resolve("etl:nightly", message="Backfilled")
```

`acknowledge()` and `resolve()` are fire-and-forget like `notify()`, and take
the same optional keyword arguments. `notify(..., action="resolve")` does the
same thing. An unknown `action` raises `ValueError` rather than falling back
to `trigger`, which could page someone for an alert meant to be closed.

From a shell:

```bash
opssignal resolve --dedup-key etl:nightly --message "Backfilled"
opssignal acknowledge --source ci --event-type build_failed --title "main is red"
```

From C, add `"action": "resolve"` to the JSON passed to
`signal_notify_async`; `{"dedup_key": "etl:nightly", "action": "resolve"}` is
enough.

## What each provider does

| Provider | `acknowledge` | `resolve` |
|---|---|---|
| PagerDuty | Acknowledges the incident (`event_action: acknowledge`) | Resolves it (`event_action: resolve`) |
| Opsgenie | Acknowledges the alert with that alias | Closes it |
| Slack with `bot_token` | Updates the alert's message in place: ":eyes: Acknowledged: ..." | Updates it: ":white_check_mark: Resolved: ..." |
| Slack with `webhook_url` | Posts a new "Acknowledged" message | Posts a new "Resolved" message |
| Teams, email, console | Sends a message titled "Acknowledged: ..." | "Resolved: ..." (email keeps the alert's thread) |
| Webhook | Posts the signal JSON, with `"action": "acknowledge"` | `"action": "resolve"` |

Only Slack's Web API can change a message once posted, so set
[`bot_token` and `channel`](providers.md#slack) to have the alert's message
updated rather than followed by a new one.

Templates see the action as `{{ action }}`, so one template can word all
three; see [Message templates](templates.md).

## Where an acknowledge or resolve goes

It goes to the providers that got the alert, not through the routes again.
OpsSignal records, in an `alerts` table next to the signal history, which
providers hold an open alert for each dedup key (and, for a Slack bot, which
message). So:

- A resolve reaches exactly the providers that were alerted, whatever the
  routes say about its own severity, and nothing else.
- A route's `dedup` and `rate_limit` never hold back an acknowledge or resolve;
  that would leave the alert open.

When this host has **no record** of an open alert for the key (it was raised
on another worker, or it is resolved already), the signal goes to the
PagerDuty and Opsgenie providers of every route that matches it apart from
severity. Both match the key on their side and ignore a key with nothing
open. Message providers (Slack, Teams, email, webhook, console) are left out
in that case, so a success callback that runs after every task does not post
"Resolved" for alerts that never fired. With neither, nothing is sent: the
result is `skipped` and says why, and the CLI prints `skipped: ...` and exits
`0`.

A signal sent in a [digest](configuration.md#grouping-and-digests) leaves no
record of its own, so resolving it reaches PagerDuty and Opsgenie by key but
not message providers.

Hosts share the record when they share the SQLite file in
[`storage.path`](configuration.md#storage). Workers with their own file still
resolve on PagerDuty and Opsgenie through the fallback above, but a Slack
message raised on another worker is not updated.

## Airflow

```python
from opssignal.integrations.airflow import failure_callback, success_callback

default_args = {
    "on_failure_callback": failure_callback,
    "on_success_callback": success_callback,
}
```

`success_callback` resolves the alert `failure_callback` raised for the same
task, with "Succeeded on try N" as the note. On a host with no record of an
open alert, each success is one call to the PagerDuty Events API or Opsgenie
Alert API for routes that include them, and nothing at all otherwise.
