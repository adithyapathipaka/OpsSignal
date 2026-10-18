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

- **Secrets.** Environment variables are fixed when a container starts, so a
  rotated Slack URL in the Secret reaches a pod only after it restarts.
- **`storage` settings**, which are never reloaded live.

## Rolling it out to every app

A Helm chart or Kustomize component that adds the `env`, `envFrom`,
`volumeMounts` and `volumes` blocks above lets every Deployment pick up
OpsSignal without any app touching the config.

## Across clusters: a config in a bucket

To share one config across clusters or with non-Kubernetes hosts, put
`signal.yaml` in S3 or GCS and point `SIGNAL_CONFIG` at it:

```yaml
env:
  - name: SIGNAL_CONFIG
    value: s3://ops-config/prod/signal.yaml
  - name: AWS_REGION
    value: us-east-1
```

Give the pod's service account read access to that object through IRSA on EKS
or workload identity on GKE; no access keys are needed. See
[Remote configs](configuration.md#remote-configs) for the cache and fallback
behaviour.
