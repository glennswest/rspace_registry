# Changelog

## [Unreleased]

### 2026-05-23
- **feat (registry):** Full OCI Distribution Spec v1.1 HTTP service — `/v2/`, `/v2/_catalog`, `/v2/<name>/tags/list`, `/v2/<name>/manifests/<ref>` (GET/HEAD/PUT/DELETE), `/v2/<name>/blobs/<digest>` (GET/HEAD/DELETE), full upload session lifecycle (`POST /uploads/`, `PATCH`, `PUT?digest=`, `GET`, `DELETE`) including monolithic POST and cross-repo mount, `/v2/<name>/referrers/<digest>` with `artifactType` filter and `OCI-Filters-Applied` header, and `POST /admin/gc` trigger.
- **feat (registry):** Manual path-pattern router so multi-segment repo names (`tenant/team/repo`) work without losing axum's middleware ergonomics.
- **feat (registry):** OCI error envelope (`{"errors":[{...}]}`) with standard codes (`BLOB_UNKNOWN`, `MANIFEST_INVALID`, `NAME_INVALID`, etc.) and `From<StorageError>` mapping.
- **feat (registry):** htpasswd auth middleware (bcrypt + plaintext) and `WWW-Authenticate: Basic` challenge on 401.
- **feat (registry):** TLS termination via axum-server + rustls (`--cert`/`--key`).
- **feat (registry):** `rspace-registry gc` subcommand runs mark-and-sweep over the data dir.
- **feat (core):** Extend `Storage` trait with chunked upload sessions (`upload_create`/`append`/`status`/`finalize`/`cancel`), listing methods (`list_repos`, `list_tags`, `list_manifest_digests`, `list_all_blobs`), and `UploadStatus` type.
- **feat (core):** OCI `Manifest` / `Descriptor` parsing module with media-type constants and `referenced_digests()` walker for image manifests and indexes.
- **feat (core):** Mark-and-sweep `gc::run()` engine with `GcReport` (manifests scanned, reachable blobs, deleted bytes).
- **feat (fs):** Implement upload sessions backed by `uploads/<uuid>` append-only files; finalise via same-fs rename with copy-fallback.
- **feat (fs):** Implement repo/tag/blob enumeration with recursive `manifests/` walk to support slash-separated repo names.
- **chore (core):** Derive `Ord` on `Digest` and `Algorithm` so reachable-set sweeps via `BTreeSet`.

### 2026-05-21
- **chore:** Initial project skeleton — Cargo workspace, three crates (binary `rspace-registry`, library `rspace-registry-core`, FS storage `rspace-registry-fs`).
- **feat (core):** OCI `Digest` type with sha256 + sha512 parsing, round-trip tests, rejection of malformed input.
- **feat (core):** `Storage` trait — minimal blob + manifest surface sufficient for OCI Distribution Spec v1.1.
- **feat (fs):** Filesystem-backed `Storage` impl with content-addressed blob layout and tag-pointer-to-digest manifest scheme. Atomic writes via tmp+rename. Unit tests for round-trip and digest-mismatch rejection.
- **docs:** README + CLAUDE.md with work plan, OCI endpoint conformance table, and cross-references to the sibling `rspacefs` repo.
