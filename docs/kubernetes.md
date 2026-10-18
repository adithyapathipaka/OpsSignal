# Kubernetes

On Kubernetes, the routing rules go in a ConfigMap and the provider secrets go
in a Secret. Every pod mounts the same ConfigMap, so one edit changes routing
for every app in the namespace, and OpsSignal picks it up without a restart.

## 1. Routes in a ConfigMap

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: opssignal-config
data:
  signal.yaml: |
    providers:
      slack:
        webhook_url: "${SLACK_WEBHOOK_URL}"
    routes:
      - match:
          severity: [error, critical]
          environment: prod
        providers: [slack]
    storage:
      sink: sqlite
      path: /var/lib/opssignal/signals.db
```

The file holds no secrets. `${SLACK_WEBHOOK_URL}` is filled in from each pod's
environment when the config loads.

## 2. Secrets in a Secret

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: opssignal-secrets
type: Opaque
stringData:
  SLACK_WEBHOOK_URL: "https://hooks.slack.com/services/..."
```

In practice this Secret usually comes from Vault, External Secrets or a sealed
secret rather than being applied by hand.

## 3. Wire both into the Deployment

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: my-service
spec:
  template:
    spec:
      containers:
        - name: app
          image: my-service:latest
          env:
            - name: SIGNAL_CONFIG
              value: /etc/opssignal/signal.yaml
          envFrom:
            - secretRef:
                name: opssignal-secrets
          volumeMounts:
            - name: opssignal-config
              mountPath: /etc/opssignal
              readOnly: true
            - name: opssignal-data
              mountPath: /var/lib/opssignal
      volumes:
        - name: opssignal-config
          configMap:
            name: opssignal-config
        - name: opssignal-data
          emptyDir: {}
```

!!! warning "Mount the directory, not the file"
    Kubernetes never updates a ConfigMap mounted with `subPath`. Mount the
    whole ConfigMap as a directory, as above, or live reload will never see
    an edit.

## Changing the routes

Edit the ConfigMap:

```bash
kubectl edit configmap opssignal-config
```

The kubelet updates the mounted file within about a minute, and OpsSignal
checks it every 60 seconds (`SIGNAL_CONFIG_REFRESH_SECS`), so a change reaches
every pod in roughly two minutes with no restart. An edit that fails to load is
logged and ignored, and the previous routes stay in place.

Two kinds of change still need a rollout (`kubectl rollout restart
deployment/my-service`):

- **Secrets held in environment variables.** Environment variables are fixed
  when a container starts, so a rotated Slack URL in the Secret reaches a pod
  only after it restarts. A `${aws-sm:...}`, `${gcp-sm:...}` or
  `${azure-kv:...}` placeholder reads the secret store directly and picks up a
  rotation on its own (see
  [Secrets from a secret store](configuration.md#secrets-from-a-secret-store)).
- **`storage` settings**, which are never reloaded live.

## Rolling it out to every app

A Helm chart or Kustomize component that adds the `env`, `envFrom`,
`volumeMounts` and `volumes` blocks above lets every Deployment pick up
OpsSignal without any app touching the config.

## A default per cluster, a file per team

A ConfigMap can hold several files. Keep the platform default and each team's
file side by side, and point each service at its team's file, which
[`extends`](configuration.md#layered-configs) the default:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: opssignal-config
data:
  default.yaml: |
    providers:
      slack:
        webhook_url: "${ORG_SLACK_WEBHOOK}"
    routes:
      - match: { severity: [error, critical] }
        providers: [slack]
  payments.yaml: |
    extends: default.yaml
    routes:
      - match: { source: payments }
        providers: [slack]
```

```yaml
env:
  - name: SIGNAL_CONFIG
    value: /etc/opssignal/payments.yaml
```

The default can also live in a bucket while team files stay in ConfigMaps:
`extends: s3://ops-config/prod/default.yaml`.

## Across clusters: a config in a bucket

To share one config across clusters or with non-Kubernetes hosts, put
`signal.yaml` in S3, GCS or Azure Blob Storage and point `SIGNAL_CONFIG` at it:

```yaml
env:
  - name: SIGNAL_CONFIG
    value: s3://ops-config/prod/signal.yaml
  - name: AWS_REGION
    value: us-east-1
```

On AKS the equivalent is `SIGNAL_CONFIG=az://ops-config/prod/signal.yaml`
with `AZURE_STORAGE_ACCOUNT_NAME` set to the storage account. Python images
need the matching extra, such as `pip install 'opssignal[aws]'`.

Give the pod's service account read access to that object through IRSA on EKS,
or workload identity on GKE or AKS; no access keys are needed. See
[Remote configs](configuration.md#remote-configs) for the cache and fallback
behaviour.
