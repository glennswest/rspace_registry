# Cluster-delegated auth (`--auth k8s`)

The registry holds **no credentials of its own**. It answers the Docker
distribution bearer-token challenge, validates the presented Kubernetes
token with **TokenReview**, and authorizes each operation with a
**SubjectAccessReview** in the namespace that matches the repository path —
the same model as the built-in OpenShift registry. "Who can pull from
`team-a/*`" is a RoleBinding, not registry config.

## How a request flows

```
podman pull registry:5000/team-a/nginx:v1
      │
      ├─ GET /v2/team-a/nginx/manifests/v1        →  401
      │     WWW-Authenticate: Bearer realm="https://…/token",service="rspace-registry"
      │
      ├─ GET /token  (Authorization: Basic <user>:<k8s-token>)
      │     registry → TokenReview ─────────────►  kube-apiserver
      │     ◄─ { "token": "<k8s-token>", "expires_in": 120 }
      │
      └─ GET /v2/team-a/nginx/manifests/v1  (Authorization: Bearer <k8s-token>)
            registry → TokenReview (cached)  ───►  authenticate
            registry → SubjectAccessReview ──────►  get repositories in ns "team-a"?
            ◄─ 200 (or 403 if the SAR says no)
```

The credential is any Kubernetes token — a user token or a ServiceAccount
token. `podman login -u anything -p $TOKEN` works because the Basic
username is ignored; a direct `Authorization: Bearer <token>` works too.

### op → SAR verb

| Registry op | SAR verb | Resource |
|---|---|---|
| pull (GET/HEAD blob, manifest) | `get` | `rspace.io/repositories` in `<namespace>` |
| list (tags/list, `_catalog`) | `list` | same |
| push (upload POST/PATCH/PUT, PUT manifest) | `update` | same |
| delete (DELETE manifest/blob) | `delete` | same |

The repo path maps to a namespace: `team-a/nginx` → namespace `team-a`.
Single-segment repos (`nginx`) and the cross-repo `_catalog` authorize
against `--auth-k8s-default-ns` (rejected if unset). TokenReview and SAR
verdicts are cached for `--auth-cache-ttl` (default `2m`) to keep the API
server off the per-blob hot path.

## Install

```bash
kubectl apply -f deploy/k8s/10-serviceaccount-rbac.yaml   # registry identity + auth-delegator
kubectl apply -f deploy/k8s/30-repository-rbac-example.yaml # puller/pusher ClusterRoles + example bindings
kubectl apply -f deploy/k8s/40-deployment.yaml            # the registry itself
# optional — only if you want `kubectl get repositories`:
kubectl apply -f deploy/k8s/20-repository-crd.yaml
```

The registry's ServiceAccount is bound to the built-in
`system:auth-delegator` ClusterRole — the **only** standing permission it
holds — which grants `create` on `tokenreviews` and `subjectaccessreviews`
and nothing else.

### No CRD required

SubjectAccessReview checks a resource *string*, not a stored object, so
`rspace.io/repositories` is virtual — RBAC references it directly, exactly
as OpenShift references `imagestreams/layers`. Install
`20-repository-crd.yaml` only if you want repos as real objects or a home
for future per-repo metadata.

## Granting access

Define the reusable ClusterRoles once (`30-repository-rbac-example.yaml`),
then bind subjects per namespace:

```bash
# alice may push to anything under team-a/…
kubectl create rolebinding alice-push -n team-a \
  --clusterrole=rspace-registry-pusher --user=alice@example.com

# everyone authenticated may pull base images from system/…
kubectl create rolebinding pull-system -n system \
  --clusterrole=rspace-registry-puller --group=system:authenticated
```

A ClusterRole referenced by a namespaced RoleBinding only grants access in
that namespace, so it composes cleanly with the repo→namespace mapping.

## Node identity & boot order (stormcos)

On a node, the primary client is CRI-O on loopback, and the registry must
serve preloaded images **before** the API server exists. Two options, both
supported:

- **`--auth-allow-loopback`** — requests from `127.0.0.1`/`::1` skip auth
  entirely. Run the registry as a static pod with this flag so the node
  bootstraps with zero cluster dependency; everything non-loopback still
  goes through TokenReview/SAR once the API server is up.
- **kubelet credential provider** — once the cluster is up, configure a
  credential provider so the kubelet supplies a projected ServiceAccount
  token when pulling from the registry:

  ```yaml
  # /etc/kubernetes/credential-providers.yaml
  apiVersion: kubelet.config.k8s.io/v1
  kind: CredentialProviderConfig
  providers:
    - name: rspace-registry-credential-provider
      matchImages:
        - "registry.rspace-registry.svc:5000"
        - "*.rspace.local"
      defaultCacheDuration: "2m"
      apiVersion: credentialprovider.kubelet.k8s.io/v1
      tokenAttributes:
        serviceAccountTokenAudience: rspace-registry
        requireServiceAccountToken: true
  ```

  (kubelet started with `--image-credential-provider-config` +
  `--image-credential-provider-bin-dir`.) The provider hands the projected
  SA token to the registry as the pull credential; the registry validates
  it via TokenReview and authorizes with SAR like any other client.

The boot-order constraint is hard: keep `--auth-allow-loopback` on for node
static pods so local pulls never depend on the API server.

## CLI reference

| Flag | Default | Purpose |
|---|---|---|
| `--auth k8s` | — | enable TokenReview/SAR auth |
| `--auth-k8s-api <url>` | in-cluster env | API server URL |
| `--auth-k8s-resource <group/res>` | `rspace.io/repositories` | SAR resource |
| `--auth-k8s-default-ns <ns>` | reject | namespace for single-segment repos + catalog |
| `--auth-cache-ttl <dur>` | `2m` | TokenReview/SAR verdict cache TTL |
| `--auth-k8s-token-url <url>` | `http(s)://<listen>/token` | Bearer challenge realm (the `/token` endpoint URL) |
| `--auth-allow-loopback` | off | skip auth for `127.0.0.1`/`::1` (boot-order fast path) |

Set `--auth-k8s-token-url` to the externally-reachable URL in production —
`--listen` often binds `0.0.0.0`, which isn't a client-reachable host.

## Non-goals

- No OIDC/OAuth server of our own — the cluster is the identity provider.
- No self-minted scoped tokens: the k8s token is the identity and SAR
  enforces authorization per request, so there is no stale-grant window.
- htpasswd mode (`--auth-file`) stays for the standalone appliance.
