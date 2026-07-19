# rspace_registry — Rust OCI Registry Head

A Rust implementation of the OCI Distribution Spec, intended to sit alongside [rspacefs](https://github.com/glennswest/rspacefs) as the on-node registry that talks directly to the same containers-storage substrate that CRI-O reads through `mount_program`.

Long-term goal: feature parity with Quay (multi-tenancy, robot accounts, signing, mirroring, GC, scanning hooks) — but in pure Rust, with rspacefs as the unified storage substrate. Short-term goal: a small, correct OCI v1.1 registry head that closes the dev loop on rspacefs (push images locally; pull through CRI-O via rspacefs; verify byte-for-byte identity end-to-end).

This is a **sibling project** to rspacefs. It is developed in parallel; the integration points are versioned and minimal.

## Status

**v0.7.0 — per-class storage quotas.** Cap how much each class may
consume on its volume, so the bursty `data`/`microvm` classes can't
starve boot-critical `system`:

```bash
rspace-registry --repo-class data=/mnt/bulk --quota-class data=500Gi \
                --repo-class customer=/mnt/cust --quota 'customer/*=2Ti'
# Over-quota pushes get 413; usage is visible at:
curl localhost:5000/admin/quotas
```

`QuotaStorage` wraps the router and enforces on the write path; usage is
the volume's blob bytes (cached, approximate — the Quay trade-off).

**v0.6.0 — k8s token-exchange endpoint.** `--auth k8s` now serves the
`GET /token` endpoint its `Bearer` challenge advertises, so clients that
follow the full distribution token flow (e.g. `podman login`) work, not
just those presenting the token directly. The k8s token stays the
identity; SAR enforces authz per request. Set `--auth-k8s-token-url` to
the externally-reachable realm URL in production.

**v0.5.0 — class migration, hardened.** Declare repo classes on their
own volumes and **move a whole class to a different volume with a
zero-miss cutover** — synchronously or as a background job:

```bash
# Each class on its own volume (sugar for `--repo-root system/*=...` etc.):
rspace-registry --repo-class system=/mnt/system \
                --repo-class microvm=/mnt/nvme \
                --repo-class data=/mnt/bulk

# Relocate the bursty, non-dedupable data class off boot-critical storage,
# in the background, draining the old volume — poll the returned job:
curl -X POST localhost:5000/admin/repo-migrate \
  -d '{"class":"data","to":"/mnt/bulk2","drain":true,"async":true}'
# → {"job_id":"...","state":"running"}
curl localhost:5000/admin/jobs/<id>          # running → done (+report)
```

The migration **overlay-cuts-over first** (new volume primary, old as
read-fallback), backfills old → new, then collapses onto the new volume —
so reads never miss, even mid-migration. Idempotent/restartable; a
more-specific rule like `data/keep` stays pinned. Also available offline:
`rspace-registry migrate --class data --to /mnt/bulk2 --drain`.

**v0.3.0 — cluster-delegated auth (`--auth k8s`).** On top of the
v0.2.0 surface (full OCI Distribution Spec v1.1 push/pull, htpasswd
auth, TLS, mark-and-sweep GC, referrers, multi-partition + replication,
per-repo storage roots):

- **`--auth k8s`** — the registry holds no credentials. It answers the
  Docker bearer-token challenge, validates presented Kubernetes tokens
  via **TokenReview**, and authorizes each operation with a
  **SubjectAccessReview** in the namespace matching the repo path
  (`<namespace>/<name>[/...]`). RBAC decides who can pull/push where —
  mirroring the built-in OpenShift registry.
- Verdicts cached (`--auth-cache-ttl`, default `2m`) to keep API-server
  overhead off the hot path.
- **`--auth-allow-loopback`** — boot-order fast path so a node serves
  preloaded images to CRI-O before the API server exists.

```bash
# Cluster-delegated auth (in a Pod with a mounted ServiceAccount):
rspace-registry --listen 0.0.0.0:5000 --data /var/lib/rspace_registry \
  --auth k8s --auth-k8s-default-ns default --auth-allow-loopback

# A user/SA token is the credential; the username is ignored:
podman login -u unused -p "$(kubectl create token my-sa)" registry.example:5000
```

Earlier: cluster-delegated auth (`--auth k8s`, TokenReview + SAR);
`MultiStore` composes N partitions with a background reconciler;
`--repo-root pattern=/path` places repos on different mounts. Admin
endpoints: `GET /admin/partitions`, `POST /admin/replicate`,
`GET /admin/repo-roots`, `POST /admin/repo-root`,
`POST /admin/repo-migrate`, `GET /admin/repo-classes`,
`GET /admin/quotas`, `GET /admin/jobs[/<id>]`, `POST /admin/gc`.

Active-partition pivot is handled by another component outside the
registry. See [CLAUDE.md](./CLAUDE.md) for the full work plan.

## Why a new registry

| Existing option | Why not |
|---|---|
| [distribution/distribution](https://github.com/distribution/distribution) (Docker registry) | Go; not Rust; storage backend is opaque, can't share with rspacefs without copy |
| [project-zot/zot](https://github.com/project-zot/zot) | Go; nice security story but again storage-opaque |
| [Quay](https://github.com/quay/quay) | Python + JS; battle-tested but heavyweight; not the storage substrate we want |
| [Harbor](https://github.com/goharbor/harbor) | Go; storage is registry-internal — same issue |

None of these share storage with the container runtime — every push lands in the registry's own blob store, then `podman pull` copies the same bytes into containers-storage. With rspace_registry + rspacefs, the bytes land **once** in `/var/lib/containers/storage/overlay/l/` and the registry exposes them. Zero-copy in the local case.

## Architecture (v0)

```
                ┌──────────────────────────────────┐
                │  rspace_registry (Rust binary)   │
   podman ─────►│  ────────────────────────────    │
   push         │   /v2/_catalog                   │
   pull ───────►│   /v2/<name>/manifests/<ref>     │
                │   /v2/<name>/blobs/<digest>      │
                │   /v2/<name>/blobs/uploads       │
                │                                  │
                │   Storage trait                  │
                │     ├── blob_put / blob_get      │
                │     ├── manifest_put / get       │
                │     └── tag_list                 │
                └──────────────┬───────────────────┘
                               │
              ┌────────────────┴────────────────┐
              │                                 │
        ┌─────▼────────┐                ┌──────▼────────────┐
        │ FS backend   │                │ rspacefs-storage  │
        │ (default)    │                │ (v0.x+1)          │
        │ disk dir of  │                │ direct integration│
        │ blob files   │                │ with /var/lib/    │
        │              │                │   containers/...  │
        └──────────────┘                └───────────────────┘
```

## Quickstart (once v0 lands)

```bash
cargo install --path crates/rspace-registry
rspace-registry --listen 0.0.0.0:5000 --data /var/lib/rspace_registry/

# Push:
podman tag busybox localhost:5000/test/busybox:v1
podman push --tls-verify=false localhost:5000/test/busybox:v1

# Pull (on any node):
podman pull --tls-verify=false localhost:5000/test/busybox:v1
```

## Project Layout (planned)

```
rspace_registry/
├── Cargo.toml                  workspace
├── README.md
├── CLAUDE.md                   work plan
├── crates/
│   ├── rspace-registry/        binary: HTTP service + CLI
│   ├── rspace-registry-core/   library: OCI types, storage trait, GC
│   └── rspace-registry-fs/     library: filesystem-backed storage
└── docs/
    ├── api.md                  OCI Distribution Spec v1.1 conformance map
    ├── storage.md              storage backend contract
    └── openshift.md            ImageStream / Route integration notes
```

## License

MIT
