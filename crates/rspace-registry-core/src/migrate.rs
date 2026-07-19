//! Live repo/class migration between storage roots — zero-miss cutover.
//!
//! `--repo-root` places a class of repos (`system/*`, `microvm/*`,
//! `data/*`, …) on a volume. `repoint`/`upsert` change where *new* writes
//! land but leave existing bytes stranded on the old volume. This module
//! *moves* the bytes so a whole class can be relocated to a different
//! volume with no downtime **and no read-miss window**:
//!
//! 1. **Overlay cutover** — repoint the rule at a [`MultiStore`] whose
//!    primary is the new backend and whose fallback is the old one. New
//!    writes immediately land on the new volume; reads resolve new-first
//!    and fall back to old, so nothing is ever unreachable — even before a
//!    single byte is copied.
//! 2. **Backfill** — copy every matching repo's content (tags, all manifest
//!    digests, every reachable blob) old → new. Since no new writes reach
//!    the old volume after step 1, one idempotent pass suffices.
//! 3. **Collapse** — repoint the rule straight at the new backend, dropping
//!    the old volume from the read path.
//! 4. **Drain (optional)** — delete the migrated repos' manifests from the
//!    old backend, then GC it so the now-unreferenced blobs are swept and
//!    the old volume's capacity is reclaimed.
//!
//! Everything is content-addressed, so the backfill is idempotent and the
//! whole operation is restartable. If the backfill fails, the route is
//! left on the overlay — a correct serving state (the union of both
//! volumes) — so a failure never loses reachability.
//!
//! ## In-flight uploads
//!
//! An upload session started on the old volume before the overlay cutover
//! is backend-local; a later `PATCH`/`PUT` for it routes to the new
//! primary and won't find the session. Callers retry the upload, which
//! then lands on the new volume. Manifests and finished blobs are
//! unaffected.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::digest::{Algorithm, Digest};
use crate::gc;
use crate::manifest::parse_manifest_refs;
use crate::multi::{MultiStore, Partition};
use crate::replicate::glob_match;
use crate::repo_router::RepoRouter;
use crate::storage::{Reference, Storage, StorageError};

#[derive(Debug, Default, Clone)]
pub struct MigrateReport {
    /// Repos moved (matched the pattern and lived on the source backend).
    pub repos_migrated: usize,
    pub blobs_copied: usize,
    pub bytes_copied: u64,
    pub manifests_copied: usize,
    /// Blobs swept from the old backend when `drain` was set.
    pub blobs_purged: usize,
    pub bytes_purged: u64,
    pub duration_ms: u128,
    /// True once the route was repointed at the new backend.
    pub cutover: bool,
}

/// Migrate every repo matching `pattern` from its current volume onto
/// `new_backend`, with **zero-miss reads** throughout, then collapse the
/// route onto the new volume. When `drain` is set, reclaim the old volume
/// afterwards.
///
/// `pattern` must be the exact key of an existing route rule (e.g.
/// `"data/*"`). Returns `Invalid` if no such rule exists.
///
/// ## Ordering — why reads never miss
///
/// 1. **Overlay cutover** — repoint the rule at a [`MultiStore`] whose
///    *primary* is the new backend and whose *fallback* is the old one.
///    From this instant, new writes land on the new volume, while reads
///    resolve new-first and fall back to old — so nothing is ever
///    unreachable, even before a single byte has been copied.
/// 2. **Backfill** — copy every matching repo old → new. Because no new
///    writes reach the old volume after step 1, one pass is sufficient and
///    idempotent.
/// 3. **Collapse** — repoint the rule straight at the new backend, dropping
///    the old volume from the read path.
///
/// If the backfill fails, the route is left on the overlay — a fully
/// correct serving state (the union of both volumes). Re-running the
/// migration, or a manual `repo-root` collapse once the cause is fixed,
/// recovers it; nothing is lost.
pub async fn run(
    router: &RepoRouter,
    pattern: &str,
    new_backend: Arc<dyn Storage>,
    drain: bool,
) -> Result<MigrateReport, StorageError> {
    let started = std::time::Instant::now();
    let mut report = MigrateReport::default();

    let old = router
        .backend_for_pattern(pattern)
        .ok_or_else(|| StorageError::Invalid(format!("no route rule with pattern {pattern:?}")))?;

    // No-op if the rule already points at this backend.
    if Arc::ptr_eq(&old, &new_backend) {
        report.duration_ms = started.elapsed().as_millis();
        return Ok(report);
    }

    // Repos to move: those matching the pattern that actually resolve to
    // the old backend today (a more specific rule may claim some of them —
    // leave those alone).
    let repos: Vec<String> = old
        .list_repos()
        .await?
        .into_iter()
        .filter(|r| glob_match(pattern, r) && Arc::ptr_eq(&router.backend_for(r), &old))
        .collect();
    report.repos_migrated = repos.len();

    // 1. Overlay cutover — reads new-first, fall back to old; writes → new.
    let overlay = Arc::new(MultiStore::new(
        vec![
            Partition {
                name: "new".into(),
                storage: new_backend.clone(),
            },
            Partition {
                name: "old".into(),
                storage: old.clone(),
            },
        ],
        "new",
    )?) as Arc<dyn Storage>;
    router.upsert(pattern.to_string(), overlay);

    // 2. Backfill old → new. No new writes reach old past step 1, so a
    //    single idempotent pass suffices.
    for repo in &repos {
        copy_repo(old.as_ref(), new_backend.as_ref(), repo, &mut report).await?;
    }

    // 3. Collapse — drop the old volume from the read path.
    router.upsert(pattern.to_string(), new_backend.clone());
    report.cutover = true;

    // Drain — delete migrated repos' manifests from the old backend, then
    // GC it so the orphaned blobs (blobs are content-addressed per root)
    // are swept and the volume is reclaimed.
    if drain {
        for repo in &repos {
            purge_repo_manifests(old.as_ref(), repo).await?;
        }
        let gc = gc::run(old.as_ref()).await?;
        report.blobs_purged = gc.deleted_blobs;
        report.bytes_purged = gc.deleted_bytes;
    }

    report.duration_ms = started.elapsed().as_millis();
    Ok(report)
}

/// Copy one repo's entire content from `src` to `dst`. Idempotent —
/// already-present blobs/manifests are skipped, so it doubles as the
/// catch-up pass.
pub async fn copy_repo(
    src: &dyn Storage,
    dst: &dyn Storage,
    repo: &str,
    report: &mut MigrateReport,
) -> Result<(), StorageError> {
    let mut manifests: BTreeSet<Digest> = BTreeSet::new();
    let mut blobs: BTreeSet<Digest> = BTreeSet::new();
    let mut tags: Vec<(String, Digest)> = Vec::new();

    // Tag-reachable walk.
    for tag in src.list_tags(repo).await? {
        let bytes = src.manifest_get(repo, &Reference::Tag(tag.clone())).await?;
        let d = sha256_digest(&bytes);
        tags.push((tag, d.clone()));
        walk(src, repo, d, bytes, &mut manifests, &mut blobs).await?;
    }
    // Detached manifests + referrers not reachable via a tag.
    for d in src.list_manifest_digests(repo).await? {
        if manifests.contains(&d) {
            continue;
        }
        match src.manifest_get(repo, &Reference::Digest(d.clone())).await {
            Ok(bytes) => walk(src, repo, d, bytes, &mut manifests, &mut blobs).await?,
            Err(StorageError::NotFound) => {}
            Err(e) => return Err(e),
        }
    }

    // Blobs first — manifests reference them.
    for d in &blobs {
        if dst.blob_exists(repo, d).await? {
            continue;
        }
        let bytes = match src.blob_read(repo, d).await {
            Ok(b) => b,
            Err(StorageError::NotFound) => continue,
            Err(e) => return Err(e),
        };
        let len = bytes.len() as u64;
        dst.blob_write(repo, d, &bytes).await?;
        report.blobs_copied += 1;
        report.bytes_copied += len;
    }

    // Manifests by digest.
    for d in &manifests {
        if dst
            .manifest_get(repo, &Reference::Digest(d.clone()))
            .await
            .is_ok()
        {
            continue;
        }
        let bytes = match src.manifest_get(repo, &Reference::Digest(d.clone())).await {
            Ok(b) => b,
            Err(StorageError::NotFound) => continue,
            Err(e) => return Err(e),
        };
        dst.manifest_put(repo, &Reference::Digest(d.clone()), &bytes)
            .await?;
        report.manifests_copied += 1;
    }

    // Tag pointers — always (re)write to match the source.
    for (tag, d) in &tags {
        let bytes = match src.manifest_get(repo, &Reference::Digest(d.clone())).await {
            Ok(b) => b,
            Err(StorageError::NotFound) => continue,
            Err(e) => return Err(e),
        };
        dst.manifest_put(repo, &Reference::Tag(tag.clone()), &bytes)
            .await?;
    }
    Ok(())
}

/// Walk a manifest, collecting its digest, child-manifest digests, and
/// referenced blob digests. Single-repo (migration never crosses repos),
/// so the visited sets are keyed by digest alone.
async fn walk(
    storage: &dyn Storage,
    repo: &str,
    digest: Digest,
    bytes: Vec<u8>,
    manifests: &mut BTreeSet<Digest>,
    blobs: &mut BTreeSet<Digest>,
) -> Result<(), StorageError> {
    if !manifests.insert(digest) {
        return Ok(());
    }
    let parsed = match parse_manifest_refs(&bytes) {
        Ok(m) => m,
        Err(_) => return Ok(()), // best-effort; not a manifest we understand
    };

    let children: Vec<Digest> = parsed.manifests.iter().map(|d| d.digest.clone()).collect();
    for child in children {
        if manifests.contains(&child) {
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
        Box::pin(walk(storage, repo, child, child_bytes, manifests, blobs)).await?;
    }

    if let Some(c) = parsed.config {
        blobs.insert(c.digest);
    }
    for l in parsed.layers {
        blobs.insert(l.digest);
    }
    if let Some(s) = parsed.subject {
        blobs.insert(s.digest);
    }
    Ok(())
}

/// Delete every tag and manifest-digest pointer for `repo` on a backend.
/// Blobs are left for `gc::run` to reclaim (they may still be shared by
/// another repo on the same root).
async fn purge_repo_manifests(storage: &dyn Storage, repo: &str) -> Result<(), StorageError> {
    for tag in storage.list_tags(repo).await? {
        match storage.manifest_delete(repo, &Reference::Tag(tag)).await {
            Ok(()) | Err(StorageError::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    for d in storage.list_manifest_digests(repo).await? {
        match storage.manifest_delete(repo, &Reference::Digest(d)).await {
            Ok(()) | Err(StorageError::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn sha256_digest(bytes: &[u8]) -> Digest {
    Digest {
        algorithm: Algorithm::Sha256,
        hex: hex::encode(<sha2::Sha256 as sha2::Digest>::digest(bytes)),
    }
}
