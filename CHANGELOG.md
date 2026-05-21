# Changelog

## [Unreleased]

### 2026-05-21
- **chore:** Initial project skeleton — Cargo workspace, three crates (binary `rspace-registry`, library `rspace-registry-core`, FS storage `rspace-registry-fs`).
- **feat (core):** OCI `Digest` type with sha256 + sha512 parsing, round-trip tests, rejection of malformed input.
- **feat (core):** `Storage` trait — minimal blob + manifest surface sufficient for OCI Distribution Spec v1.1.
- **feat (fs):** Filesystem-backed `Storage` impl with content-addressed blob layout and tag-pointer-to-digest manifest scheme. Atomic writes via tmp+rename. Unit tests for round-trip and digest-mismatch rejection.
- **docs:** README + CLAUDE.md with work plan, OCI endpoint conformance table, and cross-references to the sibling `rspacefs` repo.
