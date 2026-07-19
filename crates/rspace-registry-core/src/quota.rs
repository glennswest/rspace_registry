//! Per-class storage quotas.
//!
//! A quota caps the bytes a class of repos may consume on its volume, so
//! the bursty, non-dedupable classes (data volumes, thousands of microVM
//! instances) can't starve boot-critical `system` images sharing the node.
//! Quotas are keyed by the same glob patterns as routing (`data/*`,
//! `customer/acme/*`), longest-match wins, and map naturally onto the
//! per-class volumes declared with `--repo-class` / `--repo-root`.
//!
//! [`QuotaStorage`] is a transparent decorator over a [`RepoRouter`]: it
//! delegates every `Storage` call, but on the write paths (`blob_write`,
//! `upload_finalize`) it first checks that the incoming bytes fit under the
//! matching quota. Enforcement is at the storage boundary, so no handler
//! needs to know about quotas.
//!
//! ## Accounting
//!
//! A class's usage is the total blob bytes on its backing volume
//! ([`Storage::used_bytes`]). That's an O(blobs) scan, so it is cached per
//! backend with a short TTL and nudged upward on each accepted write to
//! keep a burst of sequential pushes honest between refreshes. Quota
//! enforcement is therefore *approximate* — a small overshoot under heavy
//! concurrency is possible, which matches how registries like Quay treat
//! it. Idempotent re-pushes of an already-present blob add no bytes and are
//! never rejected.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::RwLock;
use uuid::Uuid;

use crate::digest::Digest;
use crate::replicate::glob_match;
use crate::repo_router::RepoRouter;
use crate::storage::{Reference, Storage, StorageError, UploadStatus};

/// A single quota rule: at most `max_bytes` of blob content for repos
/// matching `pattern`.
#[derive(Clone, Debug)]
pub struct Quota {
    pub pattern: String,
    pub max_bytes: u64,
}

/// A quota's current state, for `GET /admin/quotas`.
#[derive(Clone, Debug)]
pub struct QuotaStatus {
    pub pattern: String,
    pub max_bytes: u64,
    /// `None` if the backing volume couldn't be measured.
    pub used_bytes: Option<u64>,
}

struct Cached {
    value: u64,
    expires: Instant,
}

pub struct QuotaStorage {
    router: Arc<RepoRouter>,
    quotas: Vec<Quota>,
    /// Usage cache keyed by backend pointer identity.
    usage: RwLock<HashMap<usize, Cached>>,
    ttl: Duration,
}

impl QuotaStorage {
    pub fn new(router: Arc<RepoRouter>, quotas: Vec<Quota>, ttl: Duration) -> Self {
        Self {
            router,
            quotas,
            usage: RwLock::new(HashMap::new()),
            ttl,
        }
    }

    /// The most specific (longest-pattern) quota matching `repo`, if any.
    fn matching_quota(&self, repo: &str) -> Option<&Quota> {
        self.quotas
            .iter()
            .filter(|q| glob_match(&q.pattern, repo))
            .max_by_key(|q| q.pattern.len())
    }

    /// Cached current usage of a backend, refreshing past the TTL.
    async fn usage_of(&self, backend: &Arc<dyn Storage>) -> Result<u64, StorageError> {
        let key = Arc::as_ptr(backend) as *const () as usize;
        if let Some(v) = {
            let now = Instant::now();
            let map = self.usage.read();
            map.get(&key).filter(|c| c.expires > now).map(|c| c.value)
        } {
            return Ok(v);
        }
        let used = backend.used_bytes().await?;
        self.usage.write().insert(
            key,
            Cached {
                value: used,
                expires: Instant::now() + self.ttl,
            },
        );
        Ok(used)
    }

    /// Add `delta` to a backend's cached usage if a fresh entry exists, so
    /// sequential writes within one TTL window are accounted for. A missing
    /// entry is left absent — the next check recomputes and already includes
    /// the write.
    fn bump(&self, backend: &Arc<dyn Storage>, delta: u64) {
        let key = Arc::as_ptr(backend) as *const () as usize;
        if let Some(c) = self.usage.write().get_mut(&key) {
            c.value = c.value.saturating_add(delta);
        }
    }

    /// Reject the write if `incoming` bytes would push the repo's class over
    /// its quota.
    async fn enforce(&self, repo: &str, incoming: u64) -> Result<(), StorageError> {
        let Some(q) = self.matching_quota(repo) else {
            return Ok(());
        };
        let backend = self.router.backend_for(repo);
        let used = self.usage_of(&backend).await?;
        if used.saturating_add(incoming) > q.max_bytes {
            return Err(StorageError::QuotaExceeded(format!(
                "class {:?}: {} B used + {} B incoming exceeds limit {} B",
                q.pattern, used, incoming, q.max_bytes
            )));
        }
        Ok(())
    }

    /// Per-quota usage snapshot for the admin endpoint.
    pub async fn report(&self) -> Vec<QuotaStatus> {
        let mut out = Vec::with_capacity(self.quotas.len());
        for q in &self.quotas {
            let used = match self.router.backend_for_pattern(&q.pattern) {
                Some(b) => b.used_bytes().await.ok(),
                None => None,
            };
            out.push(QuotaStatus {
                pattern: q.pattern.clone(),
                max_bytes: q.max_bytes,
                used_bytes: used,
            });
        }
        out
    }

    pub fn quotas(&self) -> &[Quota] {
        &self.quotas
    }
}

#[async_trait]
impl Storage for QuotaStorage {
    // ---- Writes: quota-checked -----------------------------------------

    async fn blob_write(
        &self,
        repo: &str,
        expected: &Digest,
        content: &[u8],
    ) -> Result<(), StorageError> {
        // Idempotent re-push of an existing blob adds no bytes.
        let is_new = !self.router.blob_exists(repo, expected).await?;
        if is_new {
            self.enforce(repo, content.len() as u64).await?;
        }
        self.router.blob_write(repo, expected, content).await?;
        if is_new {
            self.bump(&self.router.backend_for(repo), content.len() as u64);
        }
        Ok(())
    }

    async fn upload_finalize(
        &self,
        repo: &str,
        id: Uuid,
        expected: &Digest,
    ) -> Result<(), StorageError> {
        let is_new = !self.router.blob_exists(repo, expected).await?;
        let size = if is_new {
            let size = self
                .router
                .upload_status(repo, id)
                .await
                .map(|s| s.offset)
                .unwrap_or(0);
            self.enforce(repo, size).await?;
            size
        } else {
            0
        };
        self.router.upload_finalize(repo, id, expected).await?;
        if is_new {
            self.bump(&self.router.backend_for(repo), size);
        }
        Ok(())
    }

    // ---- Everything else: pass-through ---------------------------------

    async fn blob_exists(&self, repo: &str, digest: &Digest) -> Result<bool, StorageError> {
        self.router.blob_exists(repo, digest).await
    }
    async fn blob_size(&self, repo: &str, digest: &Digest) -> Result<u64, StorageError> {
        self.router.blob_size(repo, digest).await
    }
    async fn blob_read(&self, repo: &str, digest: &Digest) -> Result<Vec<u8>, StorageError> {
        self.router.blob_read(repo, digest).await
    }
    async fn blob_delete(&self, repo: &str, digest: &Digest) -> Result<(), StorageError> {
        self.router.blob_delete(repo, digest).await
    }
    async fn upload_create(&self, repo: &str) -> Result<UploadStatus, StorageError> {
        self.router.upload_create(repo).await
    }
    async fn upload_status(&self, repo: &str, id: Uuid) -> Result<UploadStatus, StorageError> {
        self.router.upload_status(repo, id).await
    }
    async fn upload_append(
        &self,
        repo: &str,
        id: Uuid,
        chunk: &[u8],
    ) -> Result<UploadStatus, StorageError> {
        self.router.upload_append(repo, id, chunk).await
    }
    async fn upload_cancel(&self, repo: &str, id: Uuid) -> Result<(), StorageError> {
        self.router.upload_cancel(repo, id).await
    }
    async fn manifest_get(
        &self,
        repo: &str,
        reference: &Reference,
    ) -> Result<Vec<u8>, StorageError> {
        self.router.manifest_get(repo, reference).await
    }
    async fn manifest_put(
        &self,
        repo: &str,
        reference: &Reference,
        content: &[u8],
    ) -> Result<Digest, StorageError> {
        self.router.manifest_put(repo, reference, content).await
    }
    async fn manifest_delete(&self, repo: &str, reference: &Reference) -> Result<(), StorageError> {
        self.router.manifest_delete(repo, reference).await
    }
    async fn list_repos(&self) -> Result<Vec<String>, StorageError> {
        self.router.list_repos().await
    }
    async fn list_tags(&self, repo: &str) -> Result<Vec<String>, StorageError> {
        self.router.list_tags(repo).await
    }
    async fn list_manifest_digests(&self, repo: &str) -> Result<Vec<Digest>, StorageError> {
        self.router.list_manifest_digests(repo).await
    }
    async fn list_all_blobs(&self) -> Result<Vec<Digest>, StorageError> {
        self.router.list_all_blobs().await
    }
}
