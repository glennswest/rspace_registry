# Changelog

## [Unreleased]

### 2026-05-24
- **feat (core):** `MultiStore` adapter — composes N child `Storage` backends. Reads start at primary and fall through to secondaries on `NotFound`; writes (incl. uploads) target the primary only; deletes apply to every partition; listings union across all and dedupe. Primary is fixed at construction (no in-registry pivot — that's handled by another component).
- **feat (core):** `replicate::run()` reconciler — copies tag-reachable manifests + their blobs from primary to each secondary; optional shell-style tag glob narrows scope. Idempotent. `replicate::spawn_loop()` runs the reconciler on a `tokio::time::interval` cadence.
- **test (core):** Unit tests for MultiStore (write-to-primary-only, read fallthrough, prefer-primary, delete-from-all, list union, duplicate/unknown primary rejection) and the glob matcher.

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
