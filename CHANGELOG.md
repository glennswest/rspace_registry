# Changelog

## [Unreleased]

### 2026-07-17
- **fix (registry):** Accept the spec-canonical trailing-slash form of upload start (`POST /v2/<name>/blobs/uploads/`), which podman/docker send; the bare form keeps working. Previously 404'd, breaking real client pushes.

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
