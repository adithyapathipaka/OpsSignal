# Providers

A provider delivers a signal to one destination. Configure each one under
`providers` in `signal.yaml`, then name it in a route. Keep credentials out of
the file with `${VAR}` or a secret reference such as `${aws-sm:prod/opssignal#pd_key}`
(see [Configuration](configuration.md#secrets-from-a-secret-store)).

| Provider | Key | Required fields | Optional fields |
|---|---|---|---|
| [Console](#console) | `console` | none | `enabled` |
| [Slack](#slack) | `slack` | `webhook_url` | none |
| [Webhook](#webhook) | `webhook` | `url` | none |
| [PagerDuty](#pagerduty) | `pagerduty` | `routing_key` | `url` |
| [Microsoft Teams](#microsoft-teams) | `teams` | `webhook_url` | none |
| [Opsgenie](#opsgenie) | `opsgenie` | `api_key` | `url`, `tags` |

All providers are in the base package; none needs a cloud extra.

A route's providers are tried in order and delivery **stops at the first one
that succeeds**, so later providers are fallbacks, not extra copies. A route
`providers: [pagerduty, teams]` pages through PagerDuty and posts to Teams only
if PagerDuty could not be reached. Any non-2xx response or connection error is
a failed attempt; each provider gets up to 3 attempts with backoff.

## Console

Prints one line per signal to stdout. It is always registered, because
unmatched signals fall back to it, and cannot be turned off.

## Slack

```yaml
providers:
  slack:
    webhook_url: "${SLACK_WEBHOOK_URL}"
```

Posts the title, message and a severity emoji to a Slack
[incoming webhook](https://api.slack.com/messaging/webhooks).

## Webhook

```yaml
providers:
  webhook:
    url: "https://alerts.example.com/opssignal"
```

`POST`s the full signal as JSON, with every field of the
[signal model](reference.md).

## PagerDuty

```yaml
providers:
  pagerduty:
    routing_key: "${PAGERDUTY_ROUTING_KEY}"
    # EU-hosted accounts only:
    # url: "https://events.eu.pagerduty.com/v2/enqueue"

routes:
  - match:
      severity: [critical]
      environment: prod
    providers: [pagerduty, slack]
```

Triggers an alert through the
[Events API v2](https://developer.pagerduty.com/docs/events-api-v2/trigger-events/).
`routing_key` is the integration key of an **Events API v2** integration on a
PagerDuty service (Service > Integrations > Add integration). `url` defaults to
`https://events.pagerduty.com/v2/enqueue`.

How a signal maps to the event:

| PagerDuty field | From the signal |
|---|---|
| `dedup_key` | `dedup_key` (the core computes one from source, event type, title and environment if the caller gives none), cut to 255 characters |
| `payload.summary` | `title`, cut to 1024 characters |
| `payload.source` | `source` |
| `payload.severity` | `severity`; `success` becomes `info` |
| `payload.timestamp` | `timestamp` |
| `payload.component` | `component`, if set |
| `payload.group` | `service`, if set |
| `payload.class` | `event_type` |
| `payload.custom_details` | `signal_id`, `event_type`, `severity`, and whichever of `message`, `environment`, `team`, `run_id`, `correlation_id`, `trace_id` and `metadata` are set |
| `links` | `log_url`, if set, as "Logs" |

Because the dedup key is stable, the same failure sent again adds to the open
alert instead of paging a second time. OpsSignal only triggers alerts; it
does not acknowledge or resolve them.

Route only the severities that should wake someone to PagerDuty. A route that
sends `info` signals there will page on them.

## Microsoft Teams

```yaml
providers:
  teams:
    webhook_url: "${TEAMS_WEBHOOK_URL}"

routes:
  - match:
      severity: [warning, error]
    providers: [teams]
```

Posts an [Adaptive Card](https://adaptivecards.io/) to a Teams channel. Create
the URL with the **Workflows** app: in the channel, choose
*Workflows > Post to a channel when a webhook request is received*, and copy the
URL it gives you. Legacy Office 365 connector incoming-webhook URLs accept the
same message, but Microsoft is retiring them.

The card shows:

- the title, coloured by severity (red for `error` and `critical`, amber for
  `warning`, green for `success`);
- the message, if set;
- facts for severity, source and event type, plus environment, service,
  component, team and run ID when set;
- a **View logs** button when `log_url` is set.

## Opsgenie

```yaml
providers:
  opsgenie:
    api_key: "${OPSGENIE_API_KEY}"
    # EU-hosted accounts only:
    # url: "https://api.eu.opsgenie.com/v2/alerts"
    tags: [data-platform]

routes:
  - match:
      severity: [critical, error]
      environment: prod
    providers: [opsgenie, slack]
```

Creates an alert through the
[Alert API](https://docs.opsgenie.com/docs/alert-api#create-alert).
`api_key` is the key of an **API** integration on the Opsgenie team that should
receive the alert (Teams > your team > Integrations > Add integration > API).
The team's routing rules and escalations decide who is notified. `url`
defaults to `https://api.opsgenie.com/v2/alerts`.

How a signal maps to the alert:

| Opsgenie field | From the signal |
|---|---|
| `message` | `title`, cut to 130 characters |
| `alias` | `dedup_key`, cut to 512 characters |
| `description` | `message`, followed by a "Logs:" line when `log_url` is set |
| `priority` | `severity`: `critical` is P1, `error` P2, `warning` P3, `info` and `success` P5 |
| `source` | `source` |
| `entity` | `service/component`, or whichever of the two is set |
| `tags` | the configured `tags`, then the severity and the environment, up to 20 |
| `details` | `signal_id`, `event_type`, `severity`, `timestamp`, whichever of `environment`, `service`, `component`, `team`, `run_id`, `correlation_id`, `trace_id` and `log_url` are set, and each top-level `metadata` key as `metadata.<key>` |

Because the alias is stable, the same failure sent again raises the open
alert's count instead of opening a second alert. OpsSignal only creates
alerts; it does not acknowledge or close them.

Atlassian is folding Opsgenie into Jira Service Management. Its operations
alert API takes the same request, so a migrated team can point `url` at the
endpoint shown on its JSM API integration and keep the rest of the config.
