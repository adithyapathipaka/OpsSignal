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
    | `appconfig://application/environment/profile` | AWS AppConfig | Python: `opssignal[aws]`. See [Managed config services](#managed-config-services). |
    | `azappconfig://store/key?label=prod` | Azure App Configuration | Python: `opssignal[azure]`. `?label=` is optional. |
    | `dynamodb://table/id` | Amazon DynamoDB item | Python: `opssignal[aws]`. See [Databases](#databases). |
    | `firestore://project/collection/document` | Firestore document | Python: `opssignal[gcp]`. |
    | `cosmos://account/database/container/id` | Azure Cosmos DB item | Python: `opssignal[azure]`. |

2. `./signal.yaml` in the current working directory.
3. Nothing found: the zero-config default (console provider, SQLite sink at
   `./signals.db`).

`SIGNAL_CONFIG` can also list several files, separated by commas, which are
merged as described in [Layered configs](#layered-configs). The CLI also
accepts `--config <path or URL>` (or a comma-separated list), which overrides
both.

### Installing cloud support

`pip install opssignal` reads local files and HTTPS only. Each cloud is a pip
extra that installs a small native add-on, which `opssignal` finds on its own:

| Install | Adds |
|---|---|
| `pip install 'opssignal[aws]'` | `s3://`, `appconfig://`, `dynamodb://`, `${aws-sm:...}` secrets |
| `pip install 'opssignal[gcp]'` | `gs://`, `firestore://`, `${gcp-sm:...}` secrets |
| `pip install 'opssignal[azure]'` | `az://`, `azappconfig://`, `cosmos://`, `${azure-kv:...}` secrets |
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
| `cargo build -p opssignal-cli --features aws` | `s3://`, `appconfig://`, `dynamodb://`, `${aws-sm:...}` |
| `cargo build -p opssignal-cli --features gcp` | `gs://`, `firestore://`, `${gcp-sm:...}` |
| `cargo build -p opssignal-cli --features azure` | `az://`, `azappconfig://`, `cosmos://`, `${azure-kv:...}` |
| `cargo build -p opssignal-cli --features all` | all three |

The same features exist on `opssignal-ffi` for the C library. If you depend on
the `opssignal-core` crate directly, the features are `aws`, `gcp` and `azure`;
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

### Managed config services

AWS AppConfig and Azure App Configuration hold the same YAML as a file, and
add change control a bucket doesn't have. AppConfig validates a change before
it goes live, rolls it out gradually with a deployment strategy, and rolls it
back automatically on a CloudWatch alarm. App Configuration keeps a label per
environment, a revision history and snapshots, so a bad edit can be restored
to an earlier value. Each team's file is one profile or key,
and `extends:` works between them:

```yaml
# appconfig://opssignal/prod/payments
extends: default          # the "default" profile of the same application and environment
```

```yaml
# azappconfig://ops-config/opssignal/teams/payments?label=prod
extends: ../default       # key opssignal/default, same store and label
```

| | AWS AppConfig | Azure App Configuration |
|---|---|---|
| URL | `appconfig://<application>/<environment>/<profile>` (names or IDs) | `azappconfig://<store>/<key>[?label=<label>]` |
| Credentials | The AWS chain above | Workload identity, service principal or managed identity, as above |
| Permissions | `appconfig:StartConfigurationSession`, `appconfig:GetLatestConfiguration` | App Configuration Data Reader |
| Endpoint override | `AWS_ENDPOINT_URL_APPCONFIGDATA` | `AZURE_APPCONFIG_ENDPOINT`, with `{store}` in it, for sovereign clouds |

**What polling costs.** AppConfig bills for each configuration it sends, so
OpsSignal keeps its polling session between checks: an unchanged profile
returns an empty, unbilled answer. App Configuration is asked with the last
ETag, so an unchanged key costs a request but no download. Each check is
still one request per file per process, which matters on App Configuration's
request quotas when thousands of pods poll: raise
`SIGNAL_CONFIG_REFRESH_SECS` there if needed.

**Cache and fallback.** After every successful fetch, the raw file is cached on
local disk, before `${VAR}` substitution, so no secret is ever written there.
When the source can't be reached, times out (10 seconds), or returns a file
that fails to load, OpsSignal uses the last good cached copy. If there is no
cached copy either, it starts on the zero-config default (console only) and
logs why, so a bucket outage never stops a service from starting or sending
signals. It switches to the real config as soon as the source is back.

### Databases

Use a database when teams change their own routing through an internal UI or
API instead of pull requests: the tool writes each team's YAML into one item,
and OpsSignal reads it like a file. Writes, change history (DynamoDB Streams,
the Cosmos DB change feed, Firestore triggers) and access control stay in the database. OpsSignal
only ever reads.

Each item holds the whole YAML of one file in a single string attribute,
`config` by default:

| | DynamoDB | Firestore | Cosmos DB |
|---|---|---|---|
| URL | `dynamodb://<table>/<id>` | `firestore://<project>/<collection>/<document>` (nested paths allowed) | `cosmos://<account>/<database>/<container>/<id>` |
| Item lookup | String partition key `id` (`?key=` to rename); tables with a sort key are not supported | Document path; `?database=` for a non-default database | Item id; partition key value is the id, or `?pk=` |
| YAML attribute | `config` (`?value=` to rename) | string field `config` (`?field=`) | string property `config` (`?field=`) |
| Permissions | `dynamodb:GetItem` | `datastore.entities.get` (Cloud Datastore Viewer) | Cosmos DB Built-in Data Reader (data-plane RBAC; account keys are not used) |
| Endpoint override | `AWS_ENDPOINT_URL_DYNAMODB` (DynamoDB Local) | `FIRESTORE_EMULATOR_HOST` | `AZURE_COSMOS_ENDPOINT`, with `{account}` in it |

`extends:` names another item of the same table, database or container:

```yaml
# dynamodb://opssignal-config/payments
extends: default          # item "default" of the same table
```

In Firestore a relative entry resolves like a path (`../shared/db-alerts` from
`opssignal/payments`). In Cosmos DB the parent's partition key value is taken
to be its id; use a full `cosmos://` URL with `?pk=` otherwise.

Each check is one read per file per process: an eventually consistent
`GetItem` (half a read unit), one Firestore document read, or one Cosmos DB
point read (sent with the last ETag, so an unchanged item is a 304).

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

## Layered configs

Large organisations rarely keep every route in one file. OpsSignal lets a file
build on others, so a platform team can own a default and each team adds only
what is its own:

```yaml
# default.yaml, owned by the platform team
providers:
  slack:
    webhook_url: "${ORG_SLACK_WEBHOOK}"
routes:
  - match: { severity: [error, critical] }
    providers: [slack]
storage:
  path: /var/lib/opssignal/signals.db
```

```yaml
# teams/payments.yaml
extends: ../default.yaml
providers:
  webhook:
    url: "${PAYMENTS_PAGER_URL}"
routes:
  - match: { source: payments, severity: [critical] }
    providers: [webhook, slack]
```

Point `SIGNAL_CONFIG` at the team file and the default comes with it.

There are two ways to combine files, and they can be mixed:

- **`extends:`** in a file names one parent, or a list of them. A relative
  entry is resolved against the file's own location: the same directory, or
  the same bucket or container for `s3://`, `gs://` and `az://`, or the same
  site for `https://`. A URL or an absolute path is used as is.
- **A list in `SIGNAL_CONFIG`**, such as
  `SIGNAL_CONFIG=teams/payments.yaml,shared/db-alerts.yaml`. Use this when a
  service needs alerts that several teams share, without one team file having
  to name another.

Files are merged lowest priority first: parents before the file that extends
them, and earlier entries of a list before later ones. A file reached more
than once, such as a default that every team extends, is loaded once.

| Section | How it merges |
|---|---|
| `providers` | By name. A later file's `slack`, `webhook`, `pagerduty`, `teams` or `console` replaces the earlier one whole. |
| `routes` | Added together. A later file's routes go first, so the more specific file is matched first and its providers are tried first, while the inherited routes still catch everything else. |
| `storage` | Taken from the last file that sets it. |

Because delivery stops at the first provider that succeeds (see
[`routes`](#routes)), a team route that matches a signal effectively decides
where it goes, and the inherited providers become its fallbacks. Routes are
checked after merging, so a team file can route to a provider that only its
parent configures. A route naming an unknown provider is reported with the
file it is in, such as `teams/payments.yaml: routes[0] references provider
'pagerduty'`.

Rules that keep chains manageable:

- A cycle (`a.yaml` extends `b.yaml`, which extends `a.yaml`) is a load-time
  error that shows the loop.
- A chain can be at most 8 files deep and load at most 64 files.
- A remote file can extend other remote files but not local paths, so a file
  served over the network can never read the host's disk. A local file can
  extend a remote one, such as a team ConfigMap that extends a default in a
  bucket.

Every file in the chain is reloaded and cached on its own. An edit to the
default reaches every team on the next check. When a remote file can't be
fetched, its cached copy is used; if the fresh files load but no longer fit
together (a team routes to a provider someone just removed from the default),
the cached copies of the whole chain are used instead. Only if neither works
does OpsSignal start on the zero-config default. A missing or broken local
file in the chain stops loading, as a single local file does.

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
| `pagerduty` | `routing_key`, `url` (optional) | Triggers a PagerDuty alert through the Events API v2. `url` defaults to the US endpoint; EU accounts set `https://events.eu.pagerduty.com/v2/enqueue`. |
| `teams` | `webhook_url` | Microsoft Teams Workflows (or legacy incoming) webhook URL. Posts an Adaptive Card. |

[Providers](providers.md) describes what each one sends and how to get its
URL or key.

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

## Secrets from a secret store

A placeholder whose name has a colon reads a secret store instead of the
environment. It works in any file of the chain, local or remote:

```yaml
providers:
  slack:
    webhook_url: "${aws-sm:prod/opssignal#slack_url}"
  webhook:
    url: "${azure-kv:ops-vault/pagerduty-hook}"
```

| Placeholder | Reads | Needs |
|---|---|---|
| `${aws-sm:<name or ARN>}` | AWS Secrets Manager, current version | `opssignal[aws]` |
| `${gcp-sm:<project>/<secret>[/<version>]}` | GCP Secret Manager, `latest` unless pinned | `opssignal[gcp]` |
| `${azure-kv:<vault>/<name>[/<version>]}` | Azure Key Vault, current version unless pinned | `opssignal[azure]` |

- `#field` at the end reads one field of a JSON secret, so one secret can hold
  several values (`${aws-sm:prod/opssignal#slack_url}`). Fields of one secret
  share one read.
- A full ARN is read in its own region; a name is read in `AWS_REGION`.
  `gcp-sm:` also takes the full `projects/<p>/secrets/<s>/versions/<v>` form.
- Credentials are the same chains as the cloud's config sources: IRSA, ECS and
  instance roles on AWS; workload identity, `GOOGLE_APPLICATION_CREDENTIALS`
  and the metadata server on GCP; workload identity, a service principal and
  managed identity on Azure. The identity needs read access to the secret
  only (`secretsmanager:GetSecretValue`, `roles/secretmanager.secretAccessor`,
  or the Key Vault Secrets User role).
- Endpoint overrides, for private endpoints, sovereign clouds and emulators:
  `AWS_ENDPOINT_URL_SECRETSMANAGER`, `GOOGLE_SECRET_MANAGER_ENDPOINT`, and
  `AZURE_KEYVAULT_ENDPOINT` with `{vault}` in it.

**Rotation.** Each reload checks the secrets as well as the files, so a rotated
secret takes effect without a restart, within `SIGNAL_SECRETS_TTL_SECS` plus
one reload interval.

**Cost.** Every store bills per read (about $0.05 per 10,000 on AWS, $0.03 on
GCP and Azure). Values are kept in memory for `SIGNAL_SECRETS_TTL_SECS`, 300 by
default, so each process reads each secret about 8,600 times a month instead of
43,000 at the 60-second reload interval. `0` reads on every reload.

**Failures.** A malformed reference, an unknown store, or a store whose extra
isn't installed stops startup like any other config mistake. A store that
can't be reached is treated like an unreachable remote file: at startup the
client runs on the default config (console only) and picks the file up once
the store answers; on reload the current config stays. Values are only held in
memory. The [config cache](#remote-configs) stores the file with its
placeholders, never the secrets.

## Load-time errors

These all stop the config from loading, and the message says which rule failed:

- invalid YAML
- an unknown key anywhere in the file
- an unset `${VAR}`, or an unterminated or empty `${}` placeholder
- a `${store:...}` secret reference that is malformed, names an unknown store,
  or needs an extra that isn't installed
- a route that references an unconfigured provider, or lists no providers
- an `extends` that is not a path or a list of paths, forms a cycle, goes more
  than 8 files deep, or (from a remote file) names a local path
- `SIGNAL_CONFIG` pointing at a path that is not a file, or using an
  unsupported URL scheme
- `SIGNAL_CONFIG_REFRESH_SECS` that is not a whole number
- a cloud URL whose support isn't installed (see
  [Installing cloud support](#installing-cloud-support))

A remote source that can't be fetched is not on this list: it falls back as
described in [Remote configs](#remote-configs).
