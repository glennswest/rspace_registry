//! Mark-and-sweep garbage collection across a `Storage` backend.
//!
//! For each repo, walk every manifest digest, parse it, and accumulate the
//! set of referenced blob digests (plus the manifest digests themselves,
//! since manifests are also stored as blobs in some backend layouts). Any
//! blob digest in `list_all_blobs` but not in the reachable set is
//! unreferenced and gets deleted.
//!
//! ## Repo argument on sweep
//!
//! Per-repo routing means `blob_delete(repo, digest)` dispatches to a
//! repo's backing store. When sweeping a digest we don't have a single
//! authoritative repo — for a single-backend store that doesn't matter
//! (the backend ignores `repo`), but for a router we try each known repo
//! in turn until one of them owns the blob's backend. Inefficient but
//! correct; GC is rare enough that this is acceptable.

use std::collections::BTreeSet;

use crate::digest::Digest;
use crate::manifest::parse_manifest_refs;
use crate::storage::{Reference, Storage, StorageError};

#[derive(Debug, Default, Clone)]
pub struct GcReport {
    pub repos_scanned: usize,
    pub manifests_scanned: usize,
    pub reachable_blobs: usize,
    pub deleted_blobs: usize,
    pub deleted_bytes: u64,
}

pub async fn run<S: Storage + ?Sized>(storage: &S) -> Result<GcReport, StorageError> {
    let mut reachable: BTreeSet<Digest> = BTreeSet::new();
    let mut report = GcReport::default();

    let repos = storage.list_repos().await?;
    report.repos_scanned = repos.len();

    for repo in &repos {
        for digest in storage.list_manifest_digests(repo).await? {
            report.manifests_scanned += 1;
            // The manifest itself is reachable.
            reachable.insert(digest.clone());

            let bytes = match storage
                .manifest_get(repo, &Reference::Digest(digest.clone()))
                .await
            {
                Ok(b) => b,
                Err(StorageError::NotFound) => continue,
                Err(e) => return Err(e),
            };
            if let Ok(m) = parse_manifest_refs(&bytes) {
                for d in m.referenced_digests() {
                    reachable.insert(d);
                }
            }
        }
    }
    report.reachable_blobs = reachable.len();

    for blob in storage.list_all_blobs().await? {
        if reachable.contains(&blob) {
            continue;
        }
        // For single-backend Storage (FsStorage), `repo` is ignored, so
        // the first repo we find works. For a router, we walk repos to
        // find the one whose backend currently holds this blob.
        let owner = locate_owner(storage, &repos, &blob).await;
        let probe_repo: &str = owner.as_deref().unwrap_or("");
        let size = storage.blob_size(probe_repo, &blob).await.unwrap_or(0);
        match storage.blob_delete(probe_repo, &blob).await {
            Ok(()) => {
                report.deleted_blobs += 1;
                report.deleted_bytes += size;
            }
            Err(StorageError::NotFound) => {}
            Err(e) => return Err(e),
        }
    }

    Ok(report)
}

/// Find a repo whose backend currently holds `digest`. Returns `None` if
/// no known repo's backend has it (orphan blob in a router with no
/// matching rule) — caller can fall back to `""` for single-backend
/// stores where `repo` is ignored anyway.
async fn locate_owner<S: Storage + ?Sized>(
    storage: &S,
    repos: &[String],
    digest: &Digest,
) -> Option<String> {
    for repo in repos {
        if storage.blob_exists(repo, digest).await.unwrap_or(false) {
            return Some(repo.clone());
        }
    }
    None
}
