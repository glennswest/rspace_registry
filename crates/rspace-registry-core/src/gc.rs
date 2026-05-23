//! Mark-and-sweep garbage collection across a `Storage` backend.
//!
//! For each repo, walk every manifest digest, parse it, and accumulate the
//! set of referenced blob digests (plus the manifest digests themselves,
//! since manifests are also stored as blobs in some backend layouts). Any
//! blob digest in `list_all_blobs` but not in the reachable set is
//! unreferenced and gets deleted.

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
        let size = storage.blob_size(&blob).await.unwrap_or(0);
        match storage.blob_delete(&blob).await {
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
