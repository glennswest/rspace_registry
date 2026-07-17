//! Multi-partition `Storage` adapter.
//!
//! Composes N child `Storage` backends into one logical store:
//!
//! - **Reads** start at the primary partition and fall through to each
//!   secondary on `NotFound`. This lets the registry serve content that
//!   hasn't replicated to the primary yet, or that lives only on a
//!   secondary because replication hasn't caught up.
//! - **Writes** (`blob_write`, upload sessions, `manifest_put`) go to the
//!   primary only. Replication to secondaries is a separate concern
//!   (see [`crate::replicate`]).
//! - **Deletes** apply to every partition — once a blob or manifest is
//!   removed, the next read should never find it on a stale replica.
//! - **Listings** union across all partitions, deduplicated and sorted.
//!
//! The primary is fixed at construction. Promoting a different partition
//! to primary (and the lifecycle bookkeeping that goes with it) is
//! handled by another component, not the registry itself.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use crate::digest::Digest;
use crate::storage::{Reference, Storage, StorageError, UploadStatus};

#[derive(Clone)]
pub struct Partition {
    pub name: String,
    pub storage: Arc<dyn Storage>,
}

impl std::fmt::Debug for Partition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Partition")
            .field("name", &self.name)
            .finish()
    }
}

pub struct MultiStore {
    partitions: Vec<Partition>,
    /// Index into `partitions` — the partition that receives writes.
    primary: usize,
}

impl MultiStore {
    /// Construct a `MultiStore`. The partition named `primary` must exist
    /// in `partitions`.
    pub fn new(partitions: Vec<Partition>, primary: &str) -> Result<Self, StorageError> {
        if partitions.is_empty() {
            return Err(StorageError::Invalid(
                "MultiStore needs at least one partition".into(),
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for p in &partitions {
            if !seen.insert(&p.name) {
                return Err(StorageError::Invalid(format!(
                    "duplicate partition name: {}",
                    p.name
                )));
            }
        }
        let primary_idx = partitions
            .iter()
            .position(|p| p.name == primary)
            .ok_or_else(|| {
                StorageError::Invalid(format!("primary partition {primary:?} not in list"))
            })?;
        Ok(Self {
            partitions,
            primary: primary_idx,
        })
    }

    pub fn partitions(&self) -> &[Partition] {
        &self.partitions
    }

    pub fn primary(&self) -> &Partition {
        &self.partitions[self.primary]
    }

    /// All partitions other than the primary, in declared order.
    pub fn secondaries(&self) -> impl Iterator<Item = &Partition> {
        self.partitions
            .iter()
            .enumerate()
            .filter(move |(i, _)| *i != self.primary)
            .map(|(_, p)| p)
    }

    /// Run an async closure across primary first, then secondaries, and
    /// return the first `Ok`. `NotFound` from any one partition is not
    /// fatal — only an `Ok` short-circuits or all partitions exhausting
    /// with `NotFound` (then `NotFound` is returned).
    async fn read_fallthrough<T, F, Fut>(&self, mut f: F) -> Result<T, StorageError>
    where
        F: FnMut(Arc<dyn Storage>) -> Fut,
        Fut: std::future::Future<Output = Result<T, StorageError>>,
    {
        let primary = self.partitions[self.primary].storage.clone();
        match f(primary).await {
            Ok(v) => return Ok(v),
            Err(StorageError::NotFound) => {}
            Err(e) => return Err(e),
        }
        for (i, p) in self.partitions.iter().enumerate() {
            if i == self.primary {
                continue;
            }
            match f(p.storage.clone()).await {
                Ok(v) => return Ok(v),
                Err(StorageError::NotFound) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(StorageError::NotFound)
    }
}

#[async_trait]
impl Storage for MultiStore {
    // ---- Blobs ----------------------------------------------------------

    async fn blob_exists(&self, repo: &str, digest: &Digest) -> Result<bool, StorageError> {
        for p in &self.partitions {
            if p.storage.blob_exists(repo, digest).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn blob_size(&self, repo: &str, digest: &Digest) -> Result<u64, StorageError> {
        let repo = repo.to_string();
        let digest = digest.clone();
        self.read_fallthrough(move |s| {
            let r = repo.clone();
            let d = digest.clone();
            async move { s.blob_size(&r, &d).await }
        })
        .await
    }

    async fn blob_read(&self, repo: &str, digest: &Digest) -> Result<Vec<u8>, StorageError> {
        let repo = repo.to_string();
        let digest = digest.clone();
        self.read_fallthrough(move |s| {
            let r = repo.clone();
            let d = digest.clone();
            async move { s.blob_read(&r, &d).await }
        })
        .await
    }

    async fn blob_write(
        &self,
        repo: &str,
        expected: &Digest,
        content: &[u8],
    ) -> Result<(), StorageError> {
        self.primary()
            .storage
            .blob_write(repo, expected, content)
            .await
    }

    async fn blob_delete(&self, repo: &str, digest: &Digest) -> Result<(), StorageError> {
        let mut deleted_any = false;
        let mut last_err: Option<StorageError> = None;
        for p in &self.partitions {
            match p.storage.blob_delete(repo, digest).await {
                Ok(()) => deleted_any = true,
                Err(StorageError::NotFound) => {}
                Err(e) => last_err = Some(e),
            }
        }
        if deleted_any {
            Ok(())
        } else if let Some(e) = last_err {
            Err(e)
        } else {
            Err(StorageError::NotFound)
        }
    }

    // ---- Upload sessions ------------------------------------------------
    //
    // All upload activity targets the primary. A pivot of the primary
    // partition (managed externally) would orphan in-flight uploads;
    // clients should retry. Documented in CHANGELOG.

    async fn upload_create(&self, repo: &str) -> Result<UploadStatus, StorageError> {
        self.primary().storage.upload_create(repo).await
    }

    async fn upload_status(&self, repo: &str, id: Uuid) -> Result<UploadStatus, StorageError> {
        self.primary().storage.upload_status(repo, id).await
    }

    async fn upload_append(
        &self,
        repo: &str,
        id: Uuid,
        chunk: &[u8],
    ) -> Result<UploadStatus, StorageError> {
        self.primary().storage.upload_append(repo, id, chunk).await
    }

    async fn upload_finalize(
        &self,
        repo: &str,
        id: Uuid,
        expected: &Digest,
    ) -> Result<(), StorageError> {
        self.primary()
            .storage
            .upload_finalize(repo, id, expected)
            .await
    }

    async fn upload_cancel(&self, repo: &str, id: Uuid) -> Result<(), StorageError> {
        self.primary().storage.upload_cancel(repo, id).await
    }

    // ---- Manifests ------------------------------------------------------

    async fn manifest_get(
        &self,
        repo: &str,
        reference: &Reference,
    ) -> Result<Vec<u8>, StorageError> {
        let repo = repo.to_string();
        let reference = reference.clone();
        self.read_fallthrough(move |s| {
            let repo = repo.clone();
            let reference = reference.clone();
            async move { s.manifest_get(&repo, &reference).await }
        })
        .await
    }

    async fn manifest_put(
        &self,
        repo: &str,
        reference: &Reference,
        content: &[u8],
    ) -> Result<Digest, StorageError> {
        self.primary()
            .storage
            .manifest_put(repo, reference, content)
            .await
    }

    async fn manifest_delete(&self, repo: &str, reference: &Reference) -> Result<(), StorageError> {
        let mut deleted_any = false;
        let mut last_err: Option<StorageError> = None;
        for p in &self.partitions {
            match p.storage.manifest_delete(repo, reference).await {
                Ok(()) => deleted_any = true,
                Err(StorageError::NotFound) => {}
                Err(e) => last_err = Some(e),
            }
        }
        if deleted_any {
            Ok(())
        } else if let Some(e) = last_err {
            Err(e)
        } else {
            Err(StorageError::NotFound)
        }
    }

    // ---- Listing --------------------------------------------------------

    async fn list_repos(&self) -> Result<Vec<String>, StorageError> {
        let mut set: BTreeSet<String> = BTreeSet::new();
        for p in &self.partitions {
            for r in p.storage.list_repos().await? {
                set.insert(r);
            }
        }
        Ok(set.into_iter().collect())
    }

    async fn list_tags(&self, repo: &str) -> Result<Vec<String>, StorageError> {
        let mut set: BTreeSet<String> = BTreeSet::new();
        for p in &self.partitions {
            for t in p.storage.list_tags(repo).await? {
                set.insert(t);
            }
        }
        Ok(set.into_iter().collect())
    }

    async fn list_manifest_digests(&self, repo: &str) -> Result<Vec<Digest>, StorageError> {
        let mut set: BTreeSet<Digest> = BTreeSet::new();
        for p in &self.partitions {
            for d in p.storage.list_manifest_digests(repo).await? {
                set.insert(d);
            }
        }
        Ok(set.into_iter().collect())
    }

    async fn list_all_blobs(&self) -> Result<Vec<Digest>, StorageError> {
        let mut set: BTreeSet<Digest> = BTreeSet::new();
        for p in &self.partitions {
            for d in p.storage.list_all_blobs().await? {
                set.insert(d);
            }
        }
        Ok(set.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::Algorithm;

    /// In-memory `Storage` for unit testing — simple enough to verify
    /// MultiStore's read-fallthrough / write-to-primary semantics
    /// without touching the filesystem.
    #[derive(Default)]
    struct MemStore {
        blobs: tokio::sync::Mutex<std::collections::HashMap<Digest, Vec<u8>>>,
        tags: tokio::sync::Mutex<std::collections::HashMap<(String, String), Digest>>,
        manifest_bytes: tokio::sync::Mutex<std::collections::HashMap<(String, Digest), Vec<u8>>>,
    }

    #[async_trait]
    impl Storage for MemStore {
        async fn blob_exists(&self, _repo: &str, d: &Digest) -> Result<bool, StorageError> {
            Ok(self.blobs.lock().await.contains_key(d))
        }
        async fn blob_size(&self, _repo: &str, d: &Digest) -> Result<u64, StorageError> {
            self.blobs
                .lock()
                .await
                .get(d)
                .map(|v| v.len() as u64)
                .ok_or(StorageError::NotFound)
        }
        async fn blob_read(&self, _repo: &str, d: &Digest) -> Result<Vec<u8>, StorageError> {
            self.blobs
                .lock()
                .await
                .get(d)
                .cloned()
                .ok_or(StorageError::NotFound)
        }
        async fn blob_write(
            &self,
            _repo: &str,
            expected: &Digest,
            content: &[u8],
        ) -> Result<(), StorageError> {
            self.blobs
                .lock()
                .await
                .insert(expected.clone(), content.to_vec());
            Ok(())
        }
        async fn blob_delete(&self, _repo: &str, d: &Digest) -> Result<(), StorageError> {
            self.blobs
                .lock()
                .await
                .remove(d)
                .map(|_| ())
                .ok_or(StorageError::NotFound)
        }
        async fn upload_create(&self, _repo: &str) -> Result<UploadStatus, StorageError> {
            Ok(UploadStatus {
                id: Uuid::new_v4(),
                offset: 0,
            })
        }
        async fn upload_status(&self, _repo: &str, id: Uuid) -> Result<UploadStatus, StorageError> {
            Ok(UploadStatus { id, offset: 0 })
        }
        async fn upload_append(
            &self,
            _repo: &str,
            id: Uuid,
            chunk: &[u8],
        ) -> Result<UploadStatus, StorageError> {
            Ok(UploadStatus {
                id,
                offset: chunk.len() as u64,
            })
        }
        async fn upload_finalize(
            &self,
            _repo: &str,
            _id: Uuid,
            _expected: &Digest,
        ) -> Result<(), StorageError> {
            Ok(())
        }
        async fn upload_cancel(&self, _repo: &str, _id: Uuid) -> Result<(), StorageError> {
            Ok(())
        }
        async fn manifest_get(
            &self,
            repo: &str,
            reference: &Reference,
        ) -> Result<Vec<u8>, StorageError> {
            let digest = match reference {
                Reference::Digest(d) => d.clone(),
                Reference::Tag(t) => self
                    .tags
                    .lock()
                    .await
                    .get(&(repo.to_string(), t.clone()))
                    .cloned()
                    .ok_or(StorageError::NotFound)?,
            };
            self.manifest_bytes
                .lock()
                .await
                .get(&(repo.to_string(), digest))
                .cloned()
                .ok_or(StorageError::NotFound)
        }
        async fn manifest_put(
            &self,
            repo: &str,
            reference: &Reference,
            content: &[u8],
        ) -> Result<Digest, StorageError> {
            let digest = Digest {
                algorithm: Algorithm::Sha256,
                hex: hex::encode(<sha2::Sha256 as sha2::Digest>::digest(content)),
            };
            self.manifest_bytes
                .lock()
                .await
                .insert((repo.to_string(), digest.clone()), content.to_vec());
            if let Reference::Tag(t) = reference {
                self.tags
                    .lock()
                    .await
                    .insert((repo.to_string(), t.clone()), digest.clone());
            }
            Ok(digest)
        }
        async fn manifest_delete(
            &self,
            repo: &str,
            reference: &Reference,
        ) -> Result<(), StorageError> {
            match reference {
                Reference::Tag(t) => self
                    .tags
                    .lock()
                    .await
                    .remove(&(repo.to_string(), t.clone()))
                    .map(|_| ())
                    .ok_or(StorageError::NotFound),
                Reference::Digest(d) => self
                    .manifest_bytes
                    .lock()
                    .await
                    .remove(&(repo.to_string(), d.clone()))
                    .map(|_| ())
                    .ok_or(StorageError::NotFound),
            }
        }
        async fn list_repos(&self) -> Result<Vec<String>, StorageError> {
            let mut set: BTreeSet<String> = BTreeSet::new();
            for (repo, _) in self.tags.lock().await.keys() {
                set.insert(repo.clone());
            }
            for ((repo, _), _) in self.manifest_bytes.lock().await.iter() {
                set.insert(repo.clone());
            }
            Ok(set.into_iter().collect())
        }
        async fn list_tags(&self, repo: &str) -> Result<Vec<String>, StorageError> {
            let mut out: Vec<String> = self
                .tags
                .lock()
                .await
                .keys()
                .filter_map(|(r, t)| if r == repo { Some(t.clone()) } else { None })
                .collect();
            out.sort();
            Ok(out)
        }
        async fn list_manifest_digests(&self, repo: &str) -> Result<Vec<Digest>, StorageError> {
            let mut out: Vec<Digest> = self
                .manifest_bytes
                .lock()
                .await
                .keys()
                .filter_map(|(r, d)| if r == repo { Some(d.clone()) } else { None })
                .collect();
            out.sort();
            Ok(out)
        }
        async fn list_all_blobs(&self) -> Result<Vec<Digest>, StorageError> {
            let mut out: Vec<Digest> = self.blobs.lock().await.keys().cloned().collect();
            out.sort();
            Ok(out)
        }
    }

    fn digest_of(s: &[u8]) -> Digest {
        Digest {
            algorithm: Algorithm::Sha256,
            hex: hex::encode(<sha2::Sha256 as sha2::Digest>::digest(s)),
        }
    }

    fn make_multi() -> (MultiStore, Arc<MemStore>, Arc<MemStore>) {
        let a = Arc::new(MemStore::default());
        let b = Arc::new(MemStore::default());
        let multi = MultiStore::new(
            vec![
                Partition {
                    name: "a".into(),
                    storage: a.clone() as Arc<dyn Storage>,
                },
                Partition {
                    name: "b".into(),
                    storage: b.clone() as Arc<dyn Storage>,
                },
            ],
            "a",
        )
        .unwrap();
        (multi, a, b)
    }

    const R: &str = "x/y";

    #[tokio::test]
    async fn writes_go_to_primary_only() {
        let (multi, a, b) = make_multi();
        let data = b"hello";
        let d = digest_of(data);
        multi.blob_write(R, &d, data).await.unwrap();
        assert!(
            a.blob_exists(R, &d).await.unwrap(),
            "primary should hold blob"
        );
        assert!(!b.blob_exists(R, &d).await.unwrap(), "secondary should not");
    }

    #[tokio::test]
    async fn read_falls_through_to_secondary() {
        let (multi, a, b) = make_multi();
        let data = b"only-on-b";
        let d = digest_of(data);
        // Plant directly into secondary, bypassing the multi-store.
        b.blob_write(R, &d, data).await.unwrap();
        assert_eq!(multi.blob_read(R, &d).await.unwrap(), data);
        // Primary still doesn't have it (fallthrough is read-only).
        assert!(!a.blob_exists(R, &d).await.unwrap());
    }

    #[tokio::test]
    async fn read_prefers_primary_over_secondary() {
        let (multi, a, b) = make_multi();
        // Two stores hold the same digest with the same bytes (must be —
        // content-addressed). MultiStore should still short-circuit on
        // the primary without consulting the secondary.
        let data = b"shared";
        let d = digest_of(data);
        a.blob_write(R, &d, data).await.unwrap();
        b.blob_write(R, &d, data).await.unwrap();
        assert_eq!(multi.blob_read(R, &d).await.unwrap(), data);
    }

    #[tokio::test]
    async fn delete_removes_from_all_partitions() {
        let (multi, a, b) = make_multi();
        let data = b"to-be-deleted";
        let d = digest_of(data);
        a.blob_write(R, &d, data).await.unwrap();
        b.blob_write(R, &d, data).await.unwrap();
        multi.blob_delete(R, &d).await.unwrap();
        assert!(!a.blob_exists(R, &d).await.unwrap());
        assert!(!b.blob_exists(R, &d).await.unwrap());
    }

    #[tokio::test]
    async fn list_unions_across_partitions() {
        let (multi, a, b) = make_multi();
        a.manifest_put("alpha/foo", &Reference::Tag("v1".into()), b"{}")
            .await
            .unwrap();
        b.manifest_put("beta/bar", &Reference::Tag("v1".into()), b"{}")
            .await
            .unwrap();
        let repos = multi.list_repos().await.unwrap();
        assert_eq!(repos, vec!["alpha/foo", "beta/bar"]);
    }

    #[test]
    fn rejects_duplicate_partition_names() {
        let s = Arc::new(MemStore::default()) as Arc<dyn Storage>;
        let r = MultiStore::new(
            vec![
                Partition {
                    name: "a".into(),
                    storage: s.clone(),
                },
                Partition {
                    name: "a".into(),
                    storage: s.clone(),
                },
            ],
            "a",
        );
        assert!(matches!(r, Err(StorageError::Invalid(_))));
    }

    #[test]
    fn rejects_unknown_primary() {
        let s = Arc::new(MemStore::default()) as Arc<dyn Storage>;
        let r = MultiStore::new(
            vec![Partition {
                name: "a".into(),
                storage: s,
            }],
            "missing",
        );
        assert!(matches!(r, Err(StorageError::Invalid(_))));
    }
}
