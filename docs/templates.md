# Message templates

!!! info "Added in v1.0.0-beta.2"
    Providers take a `template` block that reshapes the message they send.

Every provider has a built-in message format. A `template` block under a
provider replaces parts of that message with your own text, written in
[Jinja](https://jinja.palletsprojects.com/en/stable/templates/) syntax:

```yaml
providers:
  slack:
    webhook_url: "${SLACK_WEBHOOK_URL}"
    template:
      text: |
        {{ emoji }} *[{{ environment | default("no env") | upper }}] {{ title }}*
        {% if message %}{{ message }}{% endif %}
        {% if log_url %}<{{ log_url }}|View logs>{% endif %}
  pagerduty:
    routing_key: "${PD_ROUTING_KEY}"
    template:
      summary: "[{{ service }}] {{ title }}"
```

Templates are set per provider, and only the parts you set change. A provider
without a `template` block, or a part a template doesn't set, keeps the
built-in format, so existing configs send exactly what they did before.

## Parts you can template

| Provider | Parts | Replaces | Extra variables |
|---|---|---|---|
| `console` | `text` | The printed line. | |
| `slack` | `text` | The message `text`. | `emoji`: the severity's Slack emoji, such as `:x:` |
| `webhook` | `body` | The whole JSON body. Must render to valid JSON; see [Webhook bodies](#webhook-bodies). | |
| `pagerduty` | `summary` | `payload.summary`. Cut to PagerDuty's 1024 characters. | |
| `teams` | `title`, `text` | The card's title and message text blocks. The fact table and log button stay. | |
| `opsgenie` | `message`, `description` | The alert `message` (cut to 130 characters) and `description` (cut to 15000). | |
| `email` | `subject`, `body` | The subject (after `subject_prefix`, which still applies) and the plain-text body. A subject is joined onto one line. | |

Everything else a provider sends stays as the built-in format: PagerDuty's
`dedup_key`, severity and custom details, Opsgenie's alias, priority and tags,
email threading headers. That keeps deduplication and routing on the receiving
side working whatever the text says.

## Variables

Every template sees the signal's fields by name:

| Variable | Notes |
|---|---|
| `title`, `source`, `event_type`, `severity` | Always set. `severity` is lowercase: `info`, `success`, `warning`, `error`, `critical`. |
| `message`, `environment`, `service`, `component`, `team`, `run_id`, `correlation_id`, `trace_id`, `log_url`, `dedup_key` | Undefined when the signal doesn't set them, so `{% if message %}` works and `{{ message }}` prints nothing. |
| `metadata` | The signal's metadata object, such as `{{ metadata.dag_id }}`. Loop over it with the `items` filter. |
| `id`, `timestamp` | The signal id, and the time it was created as RFC 3339 in UTC. |

Filters and tests from Jinja's built-in set work, such as `upper`, `lower`,
`default`, `replace`, `trim`, `join`, `length` and `tojson`.

## Mistakes stop the load

A template is checked when the config loads, like the rest of the file. These
are load-time errors, with the key that failed in the message:

- a syntax error (`{{ title `)
- a part the provider doesn't have (`providers.slack.template.txt`)
- a variable that doesn't exist (`{{ tittle }}`), including another
  provider's extra variable (`emoji` outside Slack)

`metadata.<key>` is not checked, since metadata differs from signal to signal.

## A template never costs the alert

If a template still fails while a signal is sent (for example, `{{ metadata.count + 1 }}`
on a signal whose `count` is text), or renders to nothing but whitespace, the
provider logs a warning and sends its built-in format instead. The alert goes
out either way.

## Webhook bodies

The webhook `body` template must produce JSON. Use the `tojson` filter for
every value, so quotes and newlines in a title are escaped:

```yaml
providers:
  webhook:
    url: "https://alerts.example.com/hook"
    template:
      body: |
        {
          "summary": {{ title | tojson }},
          "level": {{ severity | upper | tojson }},
          "labels": {"env": {{ environment | default("none") | tojson }}}
        }
```

A body that is not valid JSON once rendered is logged, and the full signal is
sent instead, as without a template.

## Escaping

Templates don't escape anything. Slack and Teams render their own markdown, so
a `*` or `_` in a title formats text there, as it does with the built-in
format.

## Templates and layered configs

`template` belongs to its provider entry, and a later
[layer](configuration.md#layered-configs) replaces a provider entry whole. A
team file that wants its own Slack text restates the provider:

```yaml
# teams/data-platform.yaml
extends: ../default.yaml
providers:
  slack:
    webhook_url: "${DATA_SLACK_WEBHOOK_URL}"
    template:
      text: "{{ emoji }} [data-platform] {{ title }}"
```

## Checking a template

[`opssignal preview`](cli.md#opssignal-preview) prints what each provider
would be sent for a signal, without sending or recording anything:

```bash
opssignal preview --config signal.yaml \
  --source airflow --event-type task_failed --severity error \
  --title "load failed" --environment prod --metadata '{"dag_id": "orders"}'
```

## Digests

!!! info "Added in v1.0.0-beta.3"

A [grouped route](configuration.md#grouping-and-digests) sends a digest, which
templates see as a signal like any other: `title` reads
`<first title> (+N more)` and `message` lists the members. `metadata.digest`
is set only on digests, with `count`, `group` (the `group_by` values),
`first_at`, `last_at` and `signals` (`id`, `title`, `severity`, `source`,
`event_type` and `timestamp` of up to 100 members):

```yaml
providers:
  slack:
    webhook_url: ${SLACK_WEBHOOK_URL}
    template:
      text: |
        {% if metadata.digest %}{{ emoji }} {{ metadata.digest.count }} dbt failures in run {{ run_id }}
        {% for s in metadata.digest.signals %}• {{ s.title }}
        {% endfor %}{% else %}{{ emoji }} {{ title }}{% endif %}
```

## Python and C

Templates live in the config, so the Python package and the C library use them
with no code changes.
