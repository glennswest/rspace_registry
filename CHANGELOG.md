# Changelog

## [Unreleased]

### 2026-07-20
- **packaging:** Add `packaging/` — a reproducible `build-packages.sh` plus a
  systemd unit (`rspace-registry.service`), default env file, and a reference
  `Containerfile`. The script builds a static musl binary (glibc fallback) and
  produces a `.rpm`, a `.deb`, and a `FROM scratch` OCI image archive, each
  bundling the systemd unit, with a `SHA256SUMS`. Used to publish the `v0.7.0`
  release assets.

### 2026-07-19
- **docs/deploy:** Add `deploy/k8s/` — apply-ready manifests for `--auth k8s`:
  registry ServiceAccount bound to `system:auth-delegator` (the only standing
  permission it needs, for TokenReview + SAR), an optional `Repository` CRD,
  puller/pusher/maintainer ClusterRoles matching the op→verb table with
  example per-namespace RoleBindings, and a Deployment + Service. All four
  validate against real k8s schemas via `kubectl --dry-run=client`.
- **docs:** Add `docs/k8s-auth.md` (request flow, RBAC, the `/token` endpoint,
  node boot-order + kubelet credential-provider config for stormcos, CLI
  reference — completes phase 3's doc half of issue #2) and
  `docs/storage-classes.md` (class placement, quotas, zero-downtime
  migration, admin endpoints). README documentation + layout sections updated
  to point at them.

## [v0.7.0] — 2026-07-19

Per-class storage quotas (issue #2 phase 4, first slice). Caps how much a
repo class may consume on its volume, so the bursty, non-dedupable
classes (data volumes, thousands of microVM instances) can't starve
boot-critical `system` images sharing a node.

### Added

- **`QuotaStorage`** (`rspace-registry-core`) — a transparent `Storage`
  decorator over a `RepoRouter`. Delegates every call, but on the write
  paths (`blob_write`, `upload_finalize`) first checks the incoming bytes
  against the matching quota. Enforcement is at the storage boundary, so no
  handler needs to know about quotas. Idempotent re-pushes of an
  already-present blob add no bytes and are never rejected.
- **`Storage::used_bytes`** — new trait method (default sums `blob_size`
  over `list_all_blobs`) measuring a backend's blob bytes; overridable by
  backends with a cheaper measure (e.g. `statfs` on a dedicated mount).
- **`StorageError::QuotaExceeded`** → HTTP **413 Payload Too Large** with a
  `DENIED` OCI error envelope naming the class and limit.
- **CLI** — `--quota <pattern>=<size>` (repeatable, longest-match wins) and
  `--quota-class <name>=<size>` (sugar for `<name>/*=<size>`). Sizes accept
  a byte count or a binary-unit suffix (`K`/`Ki`, `M`/`Mi`, `G`/`Gi`,
  `T`/`Ti`). `--quota-cache-ttl` (default `30s`) tunes usage-scan cadence.
- **`GET /admin/quotas`** — each quota's `pattern`, `max_bytes`,
  `used_bytes`, and `used_pct`.
- **Tests** — under/over-quota writes, idempotent re-push not rejected,
  unmatched repos unlimited, usage report, and an HTTP push that 413s over
  quota while a small one succeeds + admin reporting.

### Notes

- Accounting is **approximate**: a class's usage is its volume's blob
  bytes, cached with a short TTL and nudged upward on each accepted write.
  A small overshoot is possible under heavy concurrency — the same
  trade-off Quay makes. Lower `--quota-cache-ttl` tightens it at the cost
  of more volume scans.
- Quotas require a repo-routed layout (`--repo-root`/`--repo-class`); they
  wrap the router as the top-level storage.

## [v0.6.0] — 2026-07-19

Completes phase 3 of the `--auth k8s` model (issue #2): the Docker
distribution token-exchange endpoint the `Bearer` challenge points at.

### Added

- **`GET /token`** — the token endpoint named by the `Bearer` challenge's
  `realm`. A client that follows the distribution token flow (rather than
  presenting the token directly) authenticates here with the Kubernetes
  token as the Basic password (`podman login` does this automatically) and
  receives it back as the bearer token to use against `/v2/`. Response:
  `{ "token", "access_token", "expires_in" }`. We do **not** mint a scoped
  token of our own — the k8s token is the identity and SAR still enforces
  authorization per request, so a granted-vs-requested scope gap can't let
  anything through. The requested `scope` is parsed (per the distribution
  token spec, `repository:<name>:<actions>`) for logging only.
  Unauthenticated by the blanket middleware (it performs its own credential
  check) and only served under `--auth k8s`.
- **`--auth-k8s-token-url <url>`** — absolute URL advertised as the
  challenge `realm`. Defaults to `http(s)://<listen>/token` (https when
  `--cert` is set); set it explicitly in production since `--listen` may
  bind `0.0.0.0`. Replaces the previous non-URL `realm` default.
- **Tests** — `parse_scope` unit tests; token-endpoint authn (valid /
  bad / missing credentials); and a full HTTP bearer flow: 401 challenge →
  `GET /token` with Basic → issued bearer token → retry reaches the handler.

### Notes

- Phase 4 of issue #2 (a `Repository` CRD, per-namespace quotas, robot
  accounts) remains open in the Quay-parity backlog.

## [v0.5.0] — 2026-07-18

Hardens class migration (v0.4.0) into a production-shaped feature:
zero-miss cutover, background jobs, named classes, and an offline CLI
path.

### Changed

- **BREAKING (behavior): migration is now zero-miss.** `migrate::run`
  no longer does copy → cutover → catch-up (which had a small read-miss
  window). It instead **overlay-cuts-over first** — repointing the rule
  at a `MultiStore` whose primary is the new volume and whose fallback is
  the old — then backfills old → new, then collapses onto the new volume.
  Reads resolve new-first and fall back to old for the whole migration, so
  nothing is ever briefly unreachable, even before a byte is copied. A
  failed backfill leaves the route on the overlay (a correct union), so
  failure never loses reachability.

### Added

- **Background migrations** — `POST /admin/repo-migrate` accepts
  `"async": true`, returns `202` with a `job_id`, and runs the migration
  on a background task. Poll `GET /admin/jobs/<id>` (or list `GET
  /admin/jobs`) for `running` / `done` (+report) / `failed` (+error). A
  multi-TB class move no longer holds the admin request open. In-memory,
  process-local registry (`jobs` module); a restart forgets in-flight
  jobs, which are recovered by re-issuing the idempotent migration.
- **Named repo classes** — `--repo-class <name>=<path>` (repeatable) is
  readable sugar for a `--repo-root <name>/*=<path>` rule, so you declare
  `system`, `partner`, `customer`, `microvm`, `data`, … each on its own
  volume. Composes with `--repo-root` (longest-match still wins).
  `GET /admin/repo-classes` lists them; `POST /admin/repo-migrate` accepts
  `"class": "data"` (expands to `data/*`).
- **Offline `migrate` subcommand** — `rspace-registry migrate
  --pattern <p> | --class <c> --to <path> [--drain]` moves a class between
  volumes as a one-shot process (build the current layout with
  `--repo-root`/`--repo-class`), for maintenance windows or when the
  server isn't running.
- **`MultiStore` reuse** inside `migrate` for the read-overlay; no new
  storage type.
- **Tests** — async job lifecycle (202 → poll → done + report + cutover),
  migrate-by-class-name, `GET /admin/repo-classes`, unknown-job 404, on
  top of the existing zero-miss/drain/idempotent/pinning suite.

### Notes

- In-flight uploads: a session started on the old volume before the
  overlay cutover is backend-local; a later chunk routes to the new
  primary and the caller retries onto the new volume. Finished blobs and
  manifests are unaffected. (Documented on `migrate`.)

## [v0.4.0] — 2026-07-18

Live class migration between storage roots. `--repo-root` already places
a class of repos (`system/*`, `microvm/*`, `data/*`, …) on a volume;
this release lets you *move* a whole class to a different volume with no
downtime — motivated by breaking apart system / partner / customer /
microVM / data-volume content and relocating the bursty, non-dedupable
classes off boot-critical storage.

### Added

- **`migrate::run(router, pattern, new_backend, drain)`** (`rspace-registry-core`)
  — drain + cutover for a repo class:
  1. **Copy pass** replicates every repo matching `pattern` (all tags,
     manifest digests, and reachable blobs) from its current backend to a
     fresh one, while the old backend keeps serving traffic.
  2. **Cutover** atomically repoints the `pattern` rule at the new backend
     (reuses `RepoRouter::upsert`).
  3. **Catch-up pass** re-copies to pick up anything written to the old
     backend during the copy window (idempotent, content-addressed).
  4. **Drain (optional)** deletes the migrated repos' manifests from the
     old backend and runs a GC pass so the orphaned blobs are swept and the
     old volume's capacity is reclaimed.
  A more-specific rule (e.g. `data/keep` alongside `data/*`) keeps its
  repos pinned — only repos that actually resolve to the source backend
  move.
- **`POST /admin/repo-migrate`** — body
  `{ "pattern": "data/*", "to": "/mnt/bulk2", "drain": false }`. Builds an
  `FsStorage` at `to`, runs the migration, and returns a report
  (`repos_migrated`, `blobs_copied`, `bytes_copied`, `manifests_copied`,
  `blobs_purged`, `bytes_purged`, `cutover`, `duration_ms`).
- **`RepoRouter::backend_for` / `backend_for_pattern`** — resolve a repo's
  current backend / a rule's bound backend, so migration can identify the
  source volume before cutover.
- **Tests** — core drain+cutover, idempotent re-run, drain reclaims the old
  volume, unknown-pattern error, more-specific-rule pinning; plus an HTTP
  test driving `POST /admin/repo-migrate` end-to-end and confirming a pull
  is served from the new volume after cutover.

### Notes

- Between cutover and the end of the catch-up pass there is a small
  consistency window: a read for content written to the *old* backend
  during the copy window but not yet re-copied can 404 briefly. It
  self-heals when the catch-up pass completes; avoid migrating a class
  under heavy fresh writes. Documented on `migrate`.
- The migration runs synchronously inside the admin request; a very large
  class blocks that one request until done. A background-job variant can
  come later if needed.

## [v0.3.0] — 2026-07-17

Cluster-delegated auth. The registry can now authenticate and authorize
against a Kubernetes cluster instead of a registry-local user database,
mirroring the built-in OpenShift registry: it holds no credentials, it
validates presented tokens, and RBAC decides who can pull/push where.

### Added

- **`--auth k8s`** (issue #2) — a third auth mode alongside `--auth-file`
  (htpasswd) and no-auth. Unauthenticated requests get a Docker
  distribution `WWW-Authenticate: Bearer realm="<realm>/token",service="rspace-registry"`
  challenge. The presented credential is a Kubernetes token (user or
  ServiceAccount); basic-auth username is ignored, so
  `podman login -u anything -p $TOKEN` works, as does a direct
  `Authorization: Bearer <token>`.
- **Authentication via TokenReview** — tokens are validated against the
  API server (`authentication.k8s.io/v1/tokenreviews`). Verdicts (positive
  and negative) are cached for `--auth-cache-ttl` (default `2m`) to keep
  per-blob overhead off the API server.
- **Authorization via SubjectAccessReview** — the repo path maps to a
  namespace (`<namespace>/<name>[/...]`) and each operation runs a SAR
  (`authorization.k8s.io/v1/subjectaccessreviews`): pull→`get`,
  push→`update`, delete→`delete`, catalog/tags→`list`, `/admin/*`→`update`,
  against a configurable virtual resource (default `rspace.io/repositories`).
  No CRD required — roles just reference the resource, like OpenShift's
  `imagestreams/layers`.
- **`--auth-allow-loopback`** — the boot-order fast path: requests from
  `127.0.0.1`/`::1` skip auth entirely, so a stormcos node can serve
  preloaded images to CRI-O **before** the API server exists.
- **CLI** — `--auth <none|htpasswd|k8s>`, `--auth-k8s-api <url>`,
  `--auth-k8s-resource <group/res>`, `--auth-k8s-default-ns <ns>`,
  `--auth-cache-ttl <dur>`, `--auth-allow-loopback`. `--auth-file` still
  implies htpasswd.
- **Tests** — unit tests for token-flow, per-verb SAR, loopback fast path,
  namespace resolution, and (method, path)→verb classification; HTTP-level
  integration tests driving the middleware over a fake `Reviewer` (bearer
  challenge, authn, authz, loopback).

### Notes

- All API-server interaction sits behind the `Reviewer` trait; the real
  `ApiReviewer` reads the in-cluster ServiceAccount mount
  (`/var/run/secrets/kubernetes.io/serviceaccount/`) and talks HTTPS with
  the cluster CA. `--auth-k8s-api` overrides the API server URL for
  out-of-cluster use.
- Phase 3 of issue #2 (a token-exchange endpoint implementing the full
  Docker distribution token server, plus kubelet credential-provider docs)
  is not yet implemented — clients present the token directly today.

### 2026-07-17
- **fix (registry):** Accept the spec-canonical trailing-slash form of upload start (`POST /v2/<name>/blobs/uploads/`), which podman/docker send; the bare form keeps working. Previously 404'd, breaking real client pushes.
- **fix (registry):** Serve manifests with their embedded `mediaType` as Content-Type instead of always defaulting to OCI. A Docker v2s2 manifest served under an OCI Content-Type made podman reject pulls with "invalid mixed OCI image with Docker v2s2 config".
- **fix (fs):** `FsStorage::new` opens a read-only root (e.g. a preloaded store snapshot mounted as a MultiStore fall-through partition) instead of failing on layout `mkdir`; writes to such a partition fail at operation time. Needed by stormcos's system-registry static pod, whose release partition mounts read-only.

### 2026-05-28
- **BREAKING (core):** Thread `repo: &str` through every blob and upload op on the `Storage` trait (`blob_exists`/`size`/`read`/`write`/`delete`, `upload_create`/`status`/`append`/`finalize`/`cancel`). Enables per-repo storage routing (issue #1). Single-backend impls (`FsStorage`, `MultiStore` children) ignore the parameter.
- **feat (registry):** Cross-repo blob mount (`POST /v2/<target>/blobs/uploads?mount=&from=`) now copies bytes between backends when source and target route to different storage roots.
- **chore (core):** Update `gc::run` to locate a blob's backend via `blob_exists` probes before sweep — works correctly under both single-backend and routing storages.
- **chore (core):** Replication reconciler now tracks `(repo, digest)` pairs so blobs land on the right per-repo backend on each secondary.

### 2026-05-27
- **ci:** Gate every `ci.yml` job with `if: github.server_url != 'https://github.com'` so GitHub Actions records the workflow_run but skips all jobs (zero minutes) — CI runs on forcicd.g8.lo (Forgejo + act_runner). Pin `Swatinem/rust-cache@v2.7.3` for forcicd's node20 runner. Add `build-linux` job (x86_64 + aarch64 cross-compile of `rspace-registry`); disable the macOS job (no forcicd Mac runner).
- **ci:** Add `release.yml` — forcicd builds x86_64 + aarch64 tarballs on `v*` tags and publishes them to the canonical github.com release via `softprops/action-gh-release@v2.0.8` using the `GH_PAT` secret; alpha-by-default release type with `workflow_dispatch` promotion.
- **chore:** `cargo fmt` pass; fix clippy lints (`ReplicateConfig` derivable `Default`, `sort_by_key` over `sort_by`).

## [v0.2.0] — 2026-05-24

Multi-partition support. The registry can now front N filesystem
partitions, write to a designated primary, and run a background
reconciler that fans tag-reachable content out to secondaries.

### Added

- **`MultiStore`** (`rspace-registry-core`) — composes N child `Storage` backends. Reads start at primary and fall through to secondaries on `NotFound`; writes (incl. upload sessions) target the primary only; deletes apply to every partition; listings union across all and dedupe.
- **`replicate::run()`** reconciler — copies tag-reachable manifests + their referenced blobs from primary to each secondary. Optional shell-style tag glob (`prod-*`, `v?.0`, `*`) narrows scope. Idempotent (content-addressed).
- **`replicate::spawn_loop()`** runs the reconciler on a `tokio::time::interval` cadence with structured logging per pass.
- **`GET /admin/partitions`** — per-partition blob / manifest / repo counts and primary flag.
- **`POST /admin/replicate`** — trigger a reconciler pass; body `{"tag_glob": "..."}` is optional. Returns counts + duration.
- **CLI** — repeatable `--partition name=/path`, `--primary <name>`, `--replicate-interval <0|60s|5m|1h>` (0 disables loop), `--replicate-tag-glob <pattern>`. New `replicate` subcommand for a one-shot pass.
- **Integration tests** — writes-to-primary-only, replicate-all fan-out + idempotent re-run, tag-glob narrowing, HTTP GET fall-through when primary is wiped, 404 when MultiStore isn't wired in. Plus unit tests for MultiStore semantics and the glob matcher.

### Notes

- The active-partition pivot decision is made by another component outside the registry — the registry just exposes which partition is primary at startup and serves reads from any.
- During an in-flight upload, the primary cannot change (the upload UUID is local to one backend). External pivots should drain or accept retries.

## [v0.1.0] — 2026-05-23

First usable cut. OCI Distribution Spec v1.1 push/pull round-trip works
end-to-end against the filesystem `Storage` backend, with optional
htpasswd auth, optional TLS, mark-and-sweep GC, and the referrers API.

### Added

- **HTTP service (registry binary)** — full OCI Distribution Spec v1.1 surface:
  - `GET /v2/` version check.
  - `GET /v2/_catalog` with `n` / `last` pagination.
  - `GET /v2/<name>/tags/list` with `n` / `last` pagination.
  - `GET` / `HEAD` / `PUT` / `DELETE /v2/<name>/manifests/<ref>` (tag or digest).
  - `GET` / `HEAD` / `DELETE /v2/<name>/blobs/<digest>`.
  - Upload sessions: `POST /v2/<name>/blobs/uploads/` (incl. monolithic `?digest=` and cross-repo `?mount=&from=`), `PATCH`, `PUT?digest=`, `GET`, `DELETE`.
  - `GET /v2/<name>/referrers/<digest>` with `artifactType` filter and `OCI-Filters-Applied` header.
  - `POST /admin/gc` admin trigger.
- **Manual path-pattern router** so multi-segment repo names (`tenant/team/repo`) work without losing axum middleware ergonomics.
- **OCI error envelope** (`{"errors":[{"code":..., "message":..., "detail":...}]}`) with standard codes and `From<StorageError>` mapping.
- **htpasswd auth** (bcrypt + plaintext) with `WWW-Authenticate: Basic` challenge on 401.
- **TLS termination** via `axum-server` + `rustls` (`--cert` / `--key`).
- **`rspace-registry gc` subcommand** for one-shot mark-and-sweep across the data dir.
- **Storage trait** (`rspace-registry-core`): blob + manifest + upload-session + listing + GC surface.
- **`gc::run()`** mark-and-sweep engine with `GcReport` (manifests scanned, reachable blobs, deleted bytes).
- **OCI `Manifest` / `Descriptor` parser** with media-type constants and `referenced_digests()`.
- **`FsStorage`** backend implementing the full trait — content-addressed blobs, tag-pointer manifests, upload sessions as append-only files under `uploads/<uuid>`, repo enumeration via recursive walk.
- **End-to-end integration test suite** covering push/pull round-trip, error shapes, referrers (with and without `artifactType` filter), cross-repo mount, and GC reaping.

### Project

- **Workspace skeleton** — three crates (binary `rspace-registry`, library `rspace-registry-core`, FS storage `rspace-registry-fs`), pinned to Rust 1.75+.
- **README + CLAUDE.md** with work plan, OCI endpoint conformance table, and cross-reference to sibling [`rspacefs`](https://github.com/glennswest/rspacefs).

### Notes

- The `rspacefs`-shared storage backend (zero-copy with `containers-storage`) is deferred to v0.2.x.
- Multi-partition + replicate-and-pivot support is captured for v0.2.0 (see CLAUDE.md work plan).
