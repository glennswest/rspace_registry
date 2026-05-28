//! Replication reconciler.
//!
//! Periodic catch-up scan that copies content from the primary partition
//! to each secondary. Idempotent because everything is content-addressed
//! — running it twice with no changes in between is a no-op.
//!
//! ## Tag-glob filtering
//!
//! When a glob is set, only tags matching it are considered for copy.
//! Their manifests and all transitively-referenced blobs / child
//! manifests are pulled across. Without a glob, everything in the
//! primary is copied.

use std::collections::BTreeSet;
use std::time::Duration;

use crate::digest::Digest;
use crate::manifest::parse_manifest_refs;
use crate::multi::{MultiStore, Partition};
use crate::storage::{Reference, Storage, StorageError};

#[derive(Debug, Default, Clone)]
pub struct ReplicateReport {
    pub partitions_scanned: usize,
    pub blobs_copied: usize,
    pub bytes_copied: u64,
    pub manifests_copied: usize,
    pub duration_ms: u128,
}

#[derive(Debug, Clone, Default)]
pub struct ReplicateConfig {
    /// Optional shell-style glob restricting which tag names get
    /// replicated (e.g. `"prod-*"`). `None` ⇒ replicate everything.
    pub tag_glob: Option<String>,
}

/// Run a single reconciliation pass over a [`MultiStore`].
pub async fn run(
    multi: &MultiStore,
    cfg: &ReplicateConfig,
) -> Result<ReplicateReport, StorageError> {
    let started = std::time::Instant::now();
    let mut report = ReplicateReport::default();

    let primary = multi.primary();
    let glob = cfg.tag_glob.as_deref();

    // Collect the manifest-digest set we want present on each secondary.
    let mut wanted_manifests: BTreeSet<(String, Digest)> = BTreeSet::new();
    // And the blob refs paired with the repo they were discovered in —
    // a router places them on the repo's backend, single-backend stores
    // ignore the repo.
    let mut wanted_blobs: BTreeSet<(String, Digest)> = BTreeSet::new();
    // Track tag → digest so we can write the right tag pointer on secondaries.
    let mut wanted_tags: Vec<(String, String, Digest)> = Vec::new();

    let repos = primary.storage.list_repos().await?;
    for repo in &repos {
        // Tag-driven walk.
        let tags = primary.storage.list_tags(repo).await?;
        for tag in tags {
            if let Some(g) = glob {
                if !glob_match(g, &tag) {
                    continue;
                }
            }
            let bytes = primary
                .storage
                .manifest_get(repo, &Reference::Tag(tag.clone()))
                .await?;
            let digest = sha256_digest(&bytes);
            wanted_tags.push((repo.clone(), tag.clone(), digest.clone()));
            walk_manifest(
                primary.storage.as_ref(),
                repo,
                digest,
                bytes,
                &mut wanted_manifests,
                &mut wanted_blobs,
            )
            .await?;
        }

        // If no glob is set, also pull every manifest digest in the
        // primary that wasn't reached via a tag — these are referrers
        // and detached manifests that should still replicate.
        if glob.is_none() {
            for d in primary.storage.list_manifest_digests(repo).await? {
                if wanted_manifests.contains(&(repo.clone(), d.clone())) {
                    continue;
                }
                let bytes = match primary
                    .storage
                    .manifest_get(repo, &Reference::Digest(d.clone()))
                    .await
                {
                    Ok(b) => b,
                    Err(StorageError::NotFound) => continue,
                    Err(e) => return Err(e),
                };
                walk_manifest(
                    primary.storage.as_ref(),
                    repo,
                    d,
                    bytes,
                    &mut wanted_manifests,
                    &mut wanted_blobs,
                )
                .await?;
            }
        }
    }

    // Fan out to each secondary.
    for secondary in multi.secondaries() {
        report.partitions_scanned += 1;
        copy_into(
            primary,
            secondary,
            &wanted_manifests,
            &wanted_blobs,
            &wanted_tags,
            &mut report,
        )
        .await?;
    }

    report.duration_ms = started.elapsed().as_millis();
    Ok(report)
}

async fn walk_manifest(
    storage: &dyn Storage,
    repo: &str,
    digest: Digest,
    bytes: Vec<u8>,
    wanted_manifests: &mut BTreeSet<(String, Digest)>,
    wanted_blobs: &mut BTreeSet<(String, Digest)>,
) -> Result<(), StorageError> {
    if !wanted_manifests.insert((repo.to_string(), digest.clone())) {
        return Ok(());
    }
    let parsed = match parse_manifest_refs(&bytes) {
        Ok(m) => m,
        Err(_) => return Ok(()), // best-effort; not a valid manifest, skip
    };

    // Child manifests (image-index entries) — recurse via Box<Pin<Fut>>
    // because async-recursion is finicky for trait-object args.
    let child_digests: Vec<Digest> = parsed.manifests.iter().map(|d| d.digest.clone()).collect();
    for child in child_digests {
        if wanted_manifests.contains(&(repo.to_string(), child.clone())) {
            continue;
        }
        let child_bytes = match storage
            .manifest_get(repo, &Reference::Digest(child.clone()))
            .await
        {
            Ok(b) => b,
            Err(StorageError::NotFound) => continue,
            Err(e) => return Err(e),
        };
        Box::pin(walk_manifest(
            storage,
            repo,
            child,
            child_bytes,
            wanted_manifests,
            wanted_blobs,
        ))
        .await?;
    }

    // Blob refs (config + layers + subject if present) — tagged with the
    // repo they were discovered in so the per-repo replication can place
    // them on the right backend.
    if let Some(c) = parsed.config {
        wanted_blobs.insert((repo.to_string(), c.digest));
    }
    for l in parsed.layers {
        wanted_blobs.insert((repo.to_string(), l.digest));
    }
    if let Some(s) = parsed.subject {
        wanted_blobs.insert((repo.to_string(), s.digest));
    }
    Ok(())
}

async fn copy_into(
    primary: &Partition,
    target: &Partition,
    wanted_manifests: &BTreeSet<(String, Digest)>,
    wanted_blobs: &BTreeSet<(String, Digest)>,
    wanted_tags: &[(String, String, Digest)],
    report: &mut ReplicateReport,
) -> Result<(), StorageError> {
    // Blobs first — manifests reference them.
    for (repo, d) in wanted_blobs {
        if target.storage.blob_exists(repo, d).await? {
            continue;
        }
        let bytes = match primary.storage.blob_read(repo, d).await {
            Ok(b) => b,
            Err(StorageError::NotFound) => continue,
            Err(e) => return Err(e),
        };
        let len = bytes.len() as u64;
        target.storage.blob_write(repo, d, &bytes).await?;
        report.blobs_copied += 1;
        report.bytes_copied += len;
    }

    // Manifests — write each as a digest reference. Tag pointers come
    // next.
    for (repo, digest) in wanted_manifests {
        let needs = match target
            .storage
            .manifest_get(repo, &Reference::Digest(digest.clone()))
            .await
        {
            Ok(_) => false,
            Err(StorageError::NotFound) => true,
            Err(e) => return Err(e),
        };
        if !needs {
            continue;
        }
        let bytes = match primary
            .storage
            .manifest_get(repo, &Reference::Digest(digest.clone()))
            .await
        {
            Ok(b) => b,
            Err(StorageError::NotFound) => continue,
            Err(e) => return Err(e),
        };
        target
            .storage
            .manifest_put(repo, &Reference::Digest(digest.clone()), &bytes)
            .await?;
        report.manifests_copied += 1;
    }

    // Tag pointers — always re-write to whatever the primary says,
    // even if the secondary already has a (possibly stale) pointer.
    for (repo, tag, digest) in wanted_tags {
        let bytes = match primary
            .storage
            .manifest_get(repo, &Reference::Digest(digest.clone()))
            .await
        {
            Ok(b) => b,
            Err(StorageError::NotFound) => continue,
            Err(e) => return Err(e),
        };
        target
            .storage
            .manifest_put(repo, &Reference::Tag(tag.clone()), &bytes)
            .await?;
    }
    Ok(())
}

fn sha256_digest(bytes: &[u8]) -> Digest {
    Digest {
        algorithm: crate::digest::Algorithm::Sha256,
        hex: hex::encode(<sha2::Sha256 as sha2::Digest>::digest(bytes)),
    }
}

/// Minimal shell-style glob: supports `*` (any run, including empty) and
/// `?` (one char). Anchored both ends. Good enough for tag selectors;
/// pulling in `globset` would be overkill.
pub fn glob_match(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = s.chars().collect();
    glob_helper(&p, 0, &t, 0)
}

fn glob_helper(p: &[char], pi: usize, t: &[char], ti: usize) -> bool {
    if pi == p.len() {
        return ti == t.len();
    }
    match p[pi] {
        '*' => {
            for k in ti..=t.len() {
                if glob_helper(p, pi + 1, t, k) {
                    return true;
                }
            }
            false
        }
        '?' => ti < t.len() && glob_helper(p, pi + 1, t, ti + 1),
        c => ti < t.len() && t[ti] == c && glob_helper(p, pi + 1, t, ti + 1),
    }
}

/// Convenience wrapper that runs `run` in a loop until cancelled by
/// dropping the returned join handle's sender.
pub fn spawn_loop(
    multi: std::sync::Arc<MultiStore>,
    cfg: ReplicateConfig,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        // First tick fires immediately; skip it so startup isn't
        // surprised by a sudden burst.
        tick.tick().await;
        loop {
            tick.tick().await;
            match run(&multi, &cfg).await {
                Ok(report) => {
                    if report.blobs_copied > 0 || report.manifests_copied > 0 {
                        tracing::info!(
                            partitions = report.partitions_scanned,
                            blobs = report.blobs_copied,
                            bytes = report.bytes_copied,
                            manifests = report.manifests_copied,
                            duration_ms = report.duration_ms,
                            "replicate: catch-up pass"
                        );
                    } else {
                        tracing::debug!(
                            partitions = report.partitions_scanned,
                            duration_ms = report.duration_ms,
                            "replicate: caught up"
                        );
                    }
                }
                Err(e) => tracing::warn!(error = %e, "replicate: pass failed"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_basics() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("prod-*", "prod-v1"));
        assert!(!glob_match("prod-*", "dev-v1"));
        assert!(glob_match("v?.0", "v1.0"));
        assert!(!glob_match("v?.0", "v10.0"));
        assert!(glob_match("v*-rc?", "v1-rc2"));
        assert!(!glob_match("v*-rc?", "v1-rc"));
    }
}
