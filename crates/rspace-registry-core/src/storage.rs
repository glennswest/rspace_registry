//! `Storage` trait — the seam between OCI HTTP handlers and byte placement.
//!
//! Implementations live in sibling crates:
//!
//! - `rspace-registry-fs` — filesystem directory tree (default)
//! - `rspace-registry-rspacefs` (future) — direct integration with
//!   containers-storage layer dirs so push + runtime share the same bytes
//!
//! ## Per-repo routing
//!
//! Every blob and upload operation carries the repository name. This is
//! how [`crate::repo_router`] can place different repos on different
//! filesystem mounts (per-repo storage roots; see issue #1). Backends
//! that don't care about the repo dimension are free to ignore it —
//! `FsStorage` just stores everything under its single root, and its
//! placement-per-repo is achieved by composing it inside a router.

use async_trait::async_trait;
use thiserror::Error;
use uuid::Uuid;

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
    #[error("invalid argument: {0}")]
    Invalid(String),
    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("internal: {0}")]
    Internal(String),
}

/// Server-side state for an in-progress blob upload.
#[derive(Debug, Clone, Copy)]
pub struct UploadStatus {
    pub id: Uuid,
    /// Current number of bytes received. Next chunk must start at this
    /// offset.
    pub offset: u64,
}

/// Minimal Storage trait. Sufficient for OCI Distribution v1.1 blob+manifest
/// surface. Listing and GC helpers are required on the trait so any backend
/// has to think about them — even a no-op implementation is a real choice.
#[async_trait]
pub trait Storage: Send + Sync {
    // ---- Blobs ----------------------------------------------------------
    //
    // The `repo` parameter lets a routing backend place different repos
    // on different filesystem roots. Single-root backends (`FsStorage`)
    // may ignore it.

    async fn blob_exists(&self, repo: &str, digest: &Digest) -> Result<bool, StorageError>;
    async fn blob_size(&self, repo: &str, digest: &Digest) -> Result<u64, StorageError>;
    async fn blob_read(&self, repo: &str, digest: &Digest) -> Result<Vec<u8>, StorageError>;
    /// Atomically write a blob whose content hashes to `expected`. The
    /// backend must reject a mismatched digest with `DigestMismatch`.
    async fn blob_write(
        &self,
        repo: &str,
        expected: &Digest,
        content: &[u8],
    ) -> Result<(), StorageError>;
    async fn blob_delete(&self, repo: &str, digest: &Digest) -> Result<(), StorageError>;

    // ---- Upload sessions ------------------------------------------------
    //
    // Chunked uploads land in a per-session tmp file scoped to one repo.
    // `upload_finalize` verifies the digest and atomically moves the
    // bytes into the content-addressed blob tree for that repo.

    async fn upload_create(&self, repo: &str) -> Result<UploadStatus, StorageError>;
    async fn upload_status(&self, repo: &str, id: Uuid) -> Result<UploadStatus, StorageError>;
    /// Append a chunk. Returns the new status (post-append offset).
    async fn upload_append(
        &self,
        repo: &str,
        id: Uuid,
        chunk: &[u8],
    ) -> Result<UploadStatus, StorageError>;
    /// Finalise the upload, validating that the accumulated bytes hash to
    /// `expected`, then move into the blob store. Idempotent if the digest
    /// already exists.
    async fn upload_finalize(
        &self,
        repo: &str,
        id: Uuid,
        expected: &Digest,
    ) -> Result<(), StorageError>;
    async fn upload_cancel(&self, repo: &str, id: Uuid) -> Result<(), StorageError>;

    // ---- Manifests ------------------------------------------------------

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
    async fn manifest_delete(&self, repo: &str, reference: &Reference) -> Result<(), StorageError>;

    // ---- Listing / GC ---------------------------------------------------

    /// Sorted, unique list of all repositories that have at least one
    /// manifest. Used by `/v2/_catalog`.
    async fn list_repos(&self) -> Result<Vec<String>, StorageError>;
    /// Sorted, unique list of tags within a repo. Used by
    /// `/v2/<name>/tags/list`.
    async fn list_tags(&self, repo: &str) -> Result<Vec<String>, StorageError>;
    /// Every manifest digest stored under `repo`. Used by referrers queries
    /// and GC.
    async fn list_manifest_digests(&self, repo: &str) -> Result<Vec<Digest>, StorageError>;
    /// Every blob digest stored across the registry. Used by GC sweep.
    /// For a router-style backend this is the union across all child
    /// backends, deduplicated.
    async fn list_all_blobs(&self) -> Result<Vec<Digest>, StorageError>;

    /// Total bytes of blob content held by this backend. Used by quota
    /// accounting. The default sums `blob_size` over `list_all_blobs`;
    /// backends with a cheaper measure (e.g. a `statfs` on a dedicated
    /// mount) should override. `repo` is passed empty because blobs are
    /// content-addressed within a root, independent of repo.
    async fn used_bytes(&self) -> Result<u64, StorageError> {
        let mut total = 0u64;
        for d in self.list_all_blobs().await? {
            total += self.blob_size("", &d).await.unwrap_or(0);
        }
        Ok(total)
    }
}
