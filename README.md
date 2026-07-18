# rspace_registry — Rust OCI Registry Head

A Rust implementation of the OCI Distribution Spec, intended to sit alongside [rspacefs](https://github.com/glennswest/rspacefs) as the on-node registry that talks directly to the same containers-storage substrate that CRI-O reads through `mount_program`.

Long-term goal: feature parity with Quay (multi-tenancy, robot accounts, signing, mirroring, GC, scanning hooks) — but in pure Rust, with rspacefs as the unified storage substrate. Short-term goal: a small, correct OCI v1.1 registry head that closes the dev loop on rspacefs (push images locally; pull through CRI-O via rspacefs; verify byte-for-byte identity end-to-end).

This is a **sibling project** to rspacefs. It is developed in parallel; the integration points are versioned and minimal.

## Status

**v0.4.0 — live class migration between volumes.** Repo classes
(`system/*`, `partner/*`, `customer/*`, `microvm/*`, `data/*`, …) are
placed on volumes with `--repo-root`; you can now **move a whole class
to a different volume with no downtime**:

```bash
# Relocate the bursty, non-dedupable data-volume class off boot-critical
# storage — copy live, cut over, then reclaim the old volume.
curl -X POST localhost:5000/admin/repo-migrate \
  -d '{"pattern":"data/*","to":"/mnt/bulk2","drain":true}'
```

Copy pass → atomic cutover → catch-up pass → optional drain (delete +
GC the old volume). Idempotent and restartable (content-addressed); a
more-specific rule like `data/keep` stays pinned.

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

Earlier: `MultiStore` composes N partitions with a background
reconciler; `--repo-root pattern=/path` places repos on different
mounts; `GET /admin/partitions`, `POST /admin/replicate`,
`GET /admin/repo-roots`, `POST /admin/repo-root`,
`POST /admin/repo-migrate` admin endpoints.

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
