//! `Storage` trait — the seam between OCI HTTP handlers and byte placement.
//!
//! Implementations live in sibling crates:
//!
//! - `rspace-registry-fs` — filesystem directory tree (default)
//! - `rspace-registry-rspacefs` (future) — direct integration with
//!   containers-storage layer dirs so push + runtime share the same bytes
//!
//! Trait shape mirrors the OCI Distribution Spec v1.1 operations a registry
//! actually needs. Anything beyond that (auth, signing, mirroring, GC
//! scheduling) lives one layer up in the HTTP handlers / control plane.

use async_trait::async_trait;
use thiserror::Error;

use crate::digest::Digest;

/// A manifest reference: tag string or digest.
#[derive(Debug, Clone)]
pub enum Reference {
    Tag(String),
    Digest(Digest),
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("not found")]
    NotFound,
    #[error("digest mismatch: expected {expected}, got {got}")]
    DigestMismatch { expected: Digest, got: Digest },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("internal: {0}")]
    Internal(String),
}

/// Minimal Storage trait. Sufficient for OCI Distribution v1.1 blob+manifest
/// surface. GC, catalogue, and listing are layered on top via helper traits
/// in a later iteration.
#[async_trait]
pub trait Storage: Send + Sync {
    async fn blob_exists(&self, digest: &Digest) -> Result<bool, StorageError>;
    async fn blob_size(&self, digest: &Digest) -> Result<u64, StorageError>;
    async fn blob_read(&self, digest: &Digest) -> Result<Vec<u8>, StorageError>;
    /// Atomically write a blob whose content hashes to `expected`. The
    /// backend must reject a mismatched digest with `DigestMismatch`.
    async fn blob_write(&self, expected: &Digest, content: &[u8]) -> Result<(), StorageError>;
    async fn blob_delete(&self, digest: &Digest) -> Result<(), StorageError>;

    async fn manifest_get(
        &self,
        repo: &str,
        reference: &Reference,
    ) -> Result<Vec<u8>, StorageError>;
    /// Store a manifest at `reference`. Returns the canonical digest the
    /// caller should use in the `Docker-Content-Digest` response header.
    async fn manifest_put(
        &self,
        repo: &str,
        reference: &Reference,
        content: &[u8],
    ) -> Result<Digest, StorageError>;
    async fn manifest_delete(
        &self,
        repo: &str,
        reference: &Reference,
    ) -> Result<(), StorageError>;
}
