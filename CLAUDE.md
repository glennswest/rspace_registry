# CLAUDE.md — rspace_registry

## What this project is

Rust OCI Distribution Spec v1.1 registry head, intended as a **sibling project** to [rspacefs](https://github.com/glennswest/rspacefs). The unique value over Docker registry / Zot / Harbor is that v0.x+1 integrates **directly** with the same containers-storage substrate (`/var/lib/containers/storage/overlay/l/`) that CRI-O reads via rspacefs's `mount_program`. Push lands bytes once; pull serves them from the same dir; zero-copy in the local case.

Long-term goal: **Quay feature parity**, in Rust.

## Cross-project rules (read these first)

Same rules as every project under `/Volumes/minihome/gwest/projects/`:

1. **All changes are approved.** Do not ask for confirmation.
2. **Commit and push after every logical unit of work.** No uncommitted state.
3. **Maintain `CHANGELOG.md`.**
4. **Docs stay current with code.** Ship them together.
5. **No claude attribution in commits.**
6. **Never build or deploy other projects.** Only build/deploy this one.
7. **To request changes in rspacefs**, write a spec at `../rspacefs/enhancements/`. Do not edit rspacefs from this repo.
8. **Always use `podman`, NOT docker.**
9. **No sensitive data in commits.** Scan diffs before pushing.
10. **Semantic versioning** — bump major/minor/patch per [semver.org](https://semver.org/). Pre-1.0 minor may include breaking changes.

## Build & Deploy

```bash
cargo build --workspace --release      # all crates
cargo build --release -p rspace-registry   # just the binary
cargo test --workspace                  # all tests
cargo run --release -p rspace-registry -- --listen 0.0.0.0:5000 --data /tmp/rspace
```

Deployment is **as a static binary**. Cross-compile for the target arch (matches the rspacefs build pattern):

```bash
cargo build --release --target x86_64-unknown-linux-gnu -p rspace-registry
cargo build --release --target aarch64-unknown-linux-gnu -p rspace-registry
```

## Architecture

| Crate | Purpose |
|---|---|
| `crates/rspace-registry/` | binary; HTTP service + CLI |
| `crates/rspace-registry-core/` | library; OCI types, Storage trait, GC engine |
| `crates/rspace-registry-fs/` | library; filesystem-backed Storage impl (default) |
| `crates/rspace-registry-rspacefs/` (v0.x+1) | library; direct integration with containers-storage layer dirs |

The `Storage` trait separates the OCI surface from byte placement so we can swap backends. v0 uses the FS impl; v0.x+1 adds the rspacefs-shared backend.

## OCI Distribution Spec v1.1 — endpoint conformance

Target: full v1.1 surface. Track each endpoint as it lands.

| Endpoint | Method | Status |
|---|---|---|
| `/v2/` | GET | TODO |
| `/v2/_catalog` | GET | TODO |
| `/v2/<name>/tags/list` | GET | TODO |
| `/v2/<name>/manifests/<reference>` | GET, HEAD | TODO |
| `/v2/<name>/manifests/<reference>` | PUT | TODO |
| `/v2/<name>/manifests/<reference>` | DELETE | TODO |
| `/v2/<name>/blobs/<digest>` | GET, HEAD | TODO |
| `/v2/<name>/blobs/<digest>` | DELETE | TODO |
| `/v2/<name>/blobs/uploads/` | POST | TODO |
| `/v2/<name>/blobs/uploads/<uuid>` | PATCH | TODO |
| `/v2/<name>/blobs/uploads/<uuid>` | PUT | TODO |
| `/v2/<name>/blobs/uploads/<uuid>` | GET | TODO |
| `/v2/<name>/blobs/uploads/<uuid>` | DELETE | TODO |
| `/v2/<name>/referrers/<digest>` | GET | TODO |

## Storage trait (planned)

```rust
#[async_trait]
pub trait Storage: Send + Sync {
    // Blobs (content-addressed)
    async fn blob_exists(&self, digest: &Digest) -> Result<bool>;
    async fn blob_read(&self, digest: &Digest) -> Result<Box<dyn AsyncRead + Send + Unpin>>;
    async fn blob_write(&self) -> Result<Box<dyn BlobWriter>>;   // returns a session
    async fn blob_size(&self, digest: &Digest) -> Result<u64>;
    async fn blob_delete(&self, digest: &Digest) -> Result<()>;

    // Manifests (per-repo, per-reference)
    async fn manifest_get(&self, repo: &str, reference: &Reference) -> Result<Manifest>;
    async fn manifest_put(&self, repo: &str, reference: &Reference, m: &Manifest) -> Result<Digest>;
    async fn manifest_delete(&self, repo: &str, reference: &Reference) -> Result<()>;

    // Catalogue
    async fn repos(&self, paginate: Pagination) -> Result<RepoList>;
    async fn tags(&self, repo: &str, paginate: Pagination) -> Result<TagList>;

    // GC support
    async fn list_blob_refs(&self) -> Result<BTreeSet<Digest>>;  // every digest referenced by any manifest
    async fn list_all_blobs(&self) -> Result<BTreeSet<Digest>>;  // every digest stored
}
```

## Integration with rspacefs (v0.x+1)

When the v0 registry is solid, add `crates/rspace-registry-rspacefs/` with a `Storage` impl that:

- Reads/writes blobs to a containers-storage-shaped layout under `/var/lib/containers/storage/`
- Stores manifests in `/var/lib/containers/storage/manifests/` (parallel to the runtime store)
- Allows `podman pull` and the registry to share the SAME bytes on the SAME node

Integration spec is at [`../rspacefs/enhancements/rspacefs-registry-head.md`](../rspacefs/enhancements/rspacefs-registry-head.md).

## Work Plan

### Current Version: `v0.0.0` (pre-implementation)

### TODO (priority order)

1. **Workspace skeleton** — Cargo workspace, three crates, basic main.rs that prints `--help`.
2. **Storage trait + FS backend** — Implement `Storage` for a directory tree. Add unit tests for blob round-trips and manifest round-trips.
3. **OCI endpoint scaffolding** — Map every endpoint from the conformance table to a handler stub returning `501 Not Implemented`. Pick an HTTP framework (decision: `axum` for ecosystem, async-first).
4. **Blob endpoints** — GET / HEAD / DELETE blobs by digest. POST a new upload, PATCH chunks, PUT to finalise with `?digest=`. Cross-mount via `?mount=&from=` query params (Distribution-Spec section).
5. **Manifest endpoints** — GET / HEAD / PUT / DELETE manifests by tag or digest. Validate `application/vnd.oci.image.manifest.v1+json` and `application/vnd.docker.distribution.manifest.v2+json`. Manifest list (image index) support.
6. **End-to-end `podman push` + `podman pull`** — first acceptance: round-trip alpine, busybox, ubi9 between the registry and podman without TLS. Then add htpasswd auth.
7. **Catalog + tags-list** — `/v2/_catalog` and `/v2/<name>/tags/list` with pagination per spec.
8. **Garbage collection** — Mark-and-sweep across all manifests → set of reachable digests; sweep unreachable blobs.
9. **Referrers API** — `/v2/<name>/referrers/<digest>` for image signatures and SBOMs.
10. **htpasswd auth** — `--auth-file <htpasswd>`. Off by default; warn at startup if no auth.
11. **TLS termination** — `--cert` / `--key`. Don't ship without TLS in production.
12. **rspacefs-shared storage backend (v0.x+1)** — depends on [rspacefs/enhancements/rspacefs-registry-head.md](../rspacefs/enhancements/rspacefs-registry-head.md) v0.x+1.

After v0 lands (push/pull round-trip with FS backend + htpasswd + TLS), bump to `v0.1.0`.

### Quay-parity backlog (long-term, after v0.x+1)

- Multi-tenancy: namespaces, orgs, projects
- Robot accounts + scope-based RBAC
- Image signing — verify cosign signatures on push, enforce allowlist on pull
- Mirror / pull-through cache
- Vulnerability scanning hook (call out to Trivy / Grype / Clair, attach reports)
- Web UI (separate Rust + WASM crate, not in this repo)
- Tag immutability + retention policies
- Audit log to S3 or local journal

### v0.2.0 — Multi-Partition Replicate-and-Pivot (next)

Initiative captured 2026-05-23. Lets the registry drive multiple
rspacefs partitions on one node: boot on A, replicate to B (all or
tag-selected), pivot writes to B without restart. rspacefs already
supports multiple stores via `additionalimagestores`, so this is
mostly a registry-side feature:

- `MultiStore` adapter implementing `Storage` over an ordered list of
  child stores; reads from any, writes to designated primary.
- Reconciler with configurable interval + tag glob (idempotent because
  content-addressed).
- Admin endpoints: `GET /admin/partitions`, `POST /admin/replicate`,
  `POST /admin/pivot { target: "B" }`.
- CLI: `--partition name=/path` (repeatable), `--primary <name>`,
  `--replicate-interval <s>`, `--replicate-tag-glob <pattern>`.
- Pivot semantics chosen: zero-downtime swap. Replication trigger:
  periodic catch-up scan.

Only write a rspacefs enhancement spec if a missing hook surfaces
during implementation.

### Recently Completed
- 2026-05-23: Full OCI Distribution Spec v1.1 HTTP service — version
  check, catalog, tags/list, manifest CRUD, blob CRUD, upload session
  lifecycle (POST/PATCH/PUT/GET/DELETE) incl. monolithic POST + cross-
  repo mount, referrers w/ artifactType filter. OCI error envelope.
- 2026-05-23: htpasswd auth (bcrypt + plaintext) with
  `WWW-Authenticate: Basic` challenge.
- 2026-05-23: TLS termination via axum-server + rustls.
- 2026-05-23: `rspace-registry gc` subcommand + `POST /admin/gc`
  triggers mark-and-sweep across the data dir.
- 2026-05-23: Manifest parser + GC engine (`gc::run` reports
  manifests scanned, reachable blobs, deleted bytes).
- 2026-05-23: Storage trait extended with upload sessions and
  listing; FsStorage backend implements them with append-only upload
  tmp files and same-fs rename to blob store.

## Test plan

| Layer | Test |
|---|---|
| Unit (Storage trait) | `Storage` impls round-trip blobs + manifests, reject malformed digests, handle concurrent uploads |
| Integration (HTTP) | Conformance suite per OCI Distribution Spec v1.1 — fork [opencontainers/distribution-spec](https://github.com/opencontainers/distribution-spec/tree/main/conformance) |
| End-to-end (podman) | `podman push` + `podman pull` round-trip for: alpine, busybox, ubi9, openjdk, a large image (~500 MB) |
| End-to-end (CRI-O via rspacefs) | Same images served from rspace-registry, pulled into a single-node K8s cluster running rspacefs as `mount_program`. Reuses `../rspacefs/tests/k8s/single-node-install/` |

## Cross-references

- **Sibling repo**: `../rspacefs/` — the storage substrate and FUSE mount adapter
- **Integration spec**: `../rspacefs/enhancements/rspacefs-registry-head.md`
- **OCI spec**: https://github.com/opencontainers/distribution-spec/blob/main/spec.md
- **OCI conformance suite**: https://github.com/opencontainers/distribution-spec/tree/main/conformance
