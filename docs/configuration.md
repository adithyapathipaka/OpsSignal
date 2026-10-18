# Configuration

## Where the config comes from

OpsSignal looks for its config in this order:

1. The `SIGNAL_CONFIG` environment variable. It can be a local path or a URL:

    | Value | Source | Notes |
    |---|---|---|
    | `/etc/opssignal/signal.yaml` | local file | Must exist, or loading fails. |
    | `https://config.example.com/signal.yaml` | HTTPS `GET` | Plain `http://` is accepted only for `localhost`. |
    | `s3://bucket/path/signal.yaml` | Amazon S3 (or any S3-compatible store) | Python: `opssignal[aws]`. |
    | `gs://bucket/path/signal.yaml` | Google Cloud Storage | Python: `opssignal[gcp]`. |
    | `az://container/path/signal.yaml` | Azure Blob Storage | Python: `opssignal[azure]`. Set `AZURE_STORAGE_ACCOUNT_NAME` to the storage account. |

2. `./signal.yaml` in the current working directory.
3. Nothing found: the zero-config default (console provider, SQLite sink at
   `./signals.db`).

The CLI also accepts `--config <path or URL>`, which overrides both.

### Installing cloud support

`pip install opssignal` reads local files and HTTPS only. Each cloud is a pip
extra that installs a small native add-on, which `opssignal` finds on its own:

| Install | Adds |
|---|---|
| `pip install 'opssignal[aws]'` | `s3://` |
| `pip install 'opssignal[gcp]'` | `gs://` |
| `pip install 'opssignal[azure]'` | `az://` |
| `pip install 'opssignal[all]'` | all three |

If `SIGNAL_CONFIG` names a cloud whose extra isn't installed, the first
`notify()` raises `RuntimeError` naming the extra to install (the CLI exits
with code 2 and names the feature to build with). That is treated
as a setup mistake, so it fails straight away instead of falling back.

The CLI and the C library follow the same rule: the base build reads local
files and HTTPS only, and each cloud is added on top of it with a cargo
feature named like the pip extra:

| Build | Adds |
|---|---|
| `cargo build -p opssignal-cli --features aws` | `s3://` |
| `cargo build -p opssignal-cli --features gcp` | `gs://` |
| `cargo build -p opssignal-cli --features azure` | `az://` |
| `cargo build -p opssignal-cli --features all` | all three |

The same features exist on `opssignal-ffi` for the C library. If you depend on
the `opssignal-core` crate directly, the features are `s3`, `gcs` and `azure`;
without them the core reads files and HTTPS only.

## Remote configs

One `signal.yaml` in a bucket can serve every service. Each process sets
`SIGNAL_CONFIG` to the same URL and keeps its secrets in its own environment
(see [Environment variables in values](#environment-variables-in-values)), so
the shared file never holds a token.

**Credentials.** OpsSignal uses each platform's standard credential chain and
has no keys of its own:

- **S3:** `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`, web identity (EKS IRSA),
  ECS task roles, or the EC2 instance profile. Set `AWS_REGION` to the bucket's
  region. `AWS_ENDPOINT_URL` points it at MinIO or another S3-compatible store.
- **GCS:** `GOOGLE_APPLICATION_CREDENTIALS` or `GOOGLE_SERVICE_ACCOUNT`,
  gcloud application-default credentials, then the metadata server (GKE
  workload identity, GCE service accounts).
- **Azure:** AKS workload identity (`AZURE_FEDERATED_TOKEN_FILE`, set by the
  workload identity webhook), managed identity, a service principal
  (`AZURE_CLIENT_ID`/`AZURE_CLIENT_SECRET`/`AZURE_TENANT_ID`), a SAS token
  (`AZURE_STORAGE_SAS_KEY`) or an account key.
- **HTTPS:** no authentication; use a URL that is reachable only from your
  network, or a pre-signed URL.

The process only needs read access to that one object (`s3:GetObject`,
`storage.objects.get`, or the Storage Blob Data Reader role on Azure).

**Cache and fallback.** After every successful fetch, the raw file is cached on
local disk, before `${VAR}` substitution, so no secret is ever written there.
When the source can't be reached, times out (10 seconds), or returns a file
that fails to load, OpsSignal uses the last good cached copy. If there is no
cached copy either, it starts on the zero-config default (console only) and
logs why, so a bucket outage never stops a service from starting or sending
signals. It switches to the real config as soon as the source is back.

## Live reload

The config is re-read every 60 seconds and changes are applied without a
restart, for local files and remote sources alike. A change replaces the
routes and providers as one unit, so no signal sees new routes with old
providers.

A reload that can't be fetched or fails to load is skipped and logged, and the
current config stays in place. A broken edit can't take alerting down.

Changes to `storage` are not applied live, because reopening the sink under
traffic could lose writes. They are logged and take effect on the next
restart.

| Variable | Default | Effect |
|---|---|---|
| `SIGNAL_CONFIG_REFRESH_SECS` | `60` | Seconds between checks. `0` loads once and never reloads. |
| `SIGNAL_CONFIG_CACHE_DIR` | `$XDG_CACHE_HOME/opssignal`, else `~/.cache/opssignal` | Where remote configs are cached. |

The CLI sends one signal and exits, so it loads the config once.

See [Kubernetes](kubernetes.md) for a ConfigMap and Secret setup that uses
live reload.

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
- `SIGNAL_CONFIG` pointing at a path that is not a file, or using an
  unsupported URL scheme
- `SIGNAL_CONFIG_REFRESH_SECS` that is not a whole number
- a cloud URL whose support isn't installed (see
  [Installing cloud support](#installing-cloud-support))

A remote source that can't be fetched is not on this list: it falls back as
described in [Remote configs](#remote-configs).
