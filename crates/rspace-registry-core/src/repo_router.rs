//! Repository → backend router.
//!
//! Place different repos on different filesystem mounts (per-repo
//! storage roots). The router holds an ordered list of glob rules; for
//! each operation it resolves the incoming `repo` against the ruleset
//! and dispatches to the matching backend.
//!
//! ## Resolution
//!
//! Rules are evaluated in declared order, **longest pattern wins** on
//! ties — so you can stack progressively-broader fallbacks:
//!
//! ```text
//! 4.18.2/kernel  → /mnt/fast/418-kernel    (exact)
//! 4.18.2/system  → /mnt/fast/418-system    (exact)
//! 4.18.2/*       → /mnt/slow/418           (group default)
//! *              → /mnt/fast/default       (global default)
//! ```
//!
//! Globs support `*` (any run, including empty) and `?` (one char), same
//! as `replicate.rs`.
//!
//! ## Runtime repoint
//!
//! `repoint(pattern, new_backend)` swaps the backend a rule resolves to,
//! atomically. This is what `rspacefs-pvc` triggers after a local mount
//! pivot so the registry reflects the change without restart. New ops
//! pick up the new backend immediately; ops already in-flight against
//! the old backend complete on it (typical `Arc<dyn Storage>` lifetime).

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::RwLock;
use uuid::Uuid;

use crate::digest::Digest;
use crate::replicate::glob_match;
use crate::storage::{Reference, Storage, StorageError, UploadStatus};

#[derive(Clone)]
pub struct RouteRule {
    pub pattern: String,
    pub backend: Arc<dyn Storage>,
}

impl std::fmt::Debug for RouteRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteRule")
            .field("pattern", &self.pattern)
            .finish()
    }
}

pub struct RepoRouter {
    rules: RwLock<Vec<RouteRule>>,
}

impl RepoRouter {
    /// Build a router. At least one rule must match every conceivable
    /// repo — pass a trailing `"*" → default_backend` rule.
    pub fn new(rules: Vec<RouteRule>) -> Result<Self, StorageError> {
        if rules.is_empty() {
            return Err(StorageError::Invalid(
                "RepoRouter needs at least one rule".into(),
            ));
        }
        Ok(Self {
            rules: RwLock::new(rules),
        })
    }

    /// Build a router with a single catchall rule (`*` → backend).
    /// Equivalent to single-backend operation behind the trait.
    pub fn single(backend: Arc<dyn Storage>) -> Self {
        Self {
            rules: RwLock::new(vec![RouteRule {
                pattern: "*".into(),
                backend,
            }]),
        }
    }

    /// Snapshot the current ruleset (cheap — clones the Arc backends).
    pub fn rules(&self) -> Vec<RouteRule> {
        self.rules.read().clone()
    }

    /// Resolve a repo to its backend. Always returns one — the
    /// constructor guarantees a non-empty ruleset, and the longest-match
    /// algorithm degrades gracefully to the broadest rule if no
    /// specific one fits (typically the `*` catchall).
    fn resolve(&self, repo: &str) -> Arc<dyn Storage> {
        let rules = self.rules.read();
        // Pick the rule whose pattern matches and is the most specific
        // (longest). Among equal-length matches, earlier in declared
        // order wins — same convention nginx uses for location blocks.
        let mut best: Option<(usize, usize)> = None; // (pattern_len, idx)
        for (i, rule) in rules.iter().enumerate() {
            if glob_match(&rule.pattern, repo) {
                match best {
                    None => best = Some((rule.pattern.len(), i)),
                    Some((blen, _)) if rule.pattern.len() > blen => {
                        best = Some((rule.pattern.len(), i))
                    }
                    _ => {}
                }
            }
        }
        match best {
            Some((_, i)) => rules[i].backend.clone(),
            // No rule matched — should be impossible given a catchall;
            // fall back to the last rule's backend so callers don't see
            // panics in production. This path is also covered by tests.
            None => rules[rules.len() - 1].backend.clone(),
        }
    }

    /// Atomically replace the backend behind every rule whose `pattern`
    /// equals the given pattern (typically one). Returns the count of
    /// rules updated; 0 means no matching rule exists.
    ///
    /// New ops pick up the new backend on their next dispatch. Ops
    /// already holding an Arc<dyn Storage> via `resolve()` complete on
    /// the old backend.
    pub fn repoint(&self, pattern: &str, backend: Arc<dyn Storage>) -> usize {
        let mut rules = self.rules.write();
        let mut updated = 0;
        for rule in rules.iter_mut() {
            if rule.pattern == pattern {
                rule.backend = backend.clone();
                updated += 1;
            }
        }
        updated
    }

    /// Append or replace a rule by pattern. If a rule with this exact
    /// pattern exists, its backend is swapped (same as `repoint`).
    /// Otherwise the rule is appended. Returns `true` if a new rule was
    /// added.
    pub fn upsert(&self, pattern: String, backend: Arc<dyn Storage>) -> bool {
        let mut rules = self.rules.write();
        for rule in rules.iter_mut() {
            if rule.pattern == pattern {
                rule.backend = backend;
                return false;
            }
        }
        rules.push(RouteRule { pattern, backend });
        true
    }

    /// Walk every distinct backend (deduped by Arc pointer identity).
    /// Used by listing operations that need to union across the whole
    /// router rather than per-rule.
    fn distinct_backends(&self) -> Vec<Arc<dyn Storage>> {
        let rules = self.rules.read();
        let mut out: Vec<Arc<dyn Storage>> = Vec::new();
        for r in rules.iter() {
            let raw = Arc::as_ptr(&r.backend) as *const ();
            if !out
                .iter()
                .any(|b| Arc::as_ptr(b) as *const () == raw)
            {
                out.push(r.backend.clone());
            }
        }
        out
    }
}

#[async_trait]
impl Storage for RepoRouter {
    // ---- Blobs ----------------------------------------------------------

    async fn blob_exists(&self, repo: &str, digest: &Digest) -> Result<bool, StorageError> {
        self.resolve(repo).blob_exists(repo, digest).await
    }

    async fn blob_size(&self, repo: &str, digest: &Digest) -> Result<u64, StorageError> {
        self.resolve(repo).blob_size(repo, digest).await
    }

    async fn blob_read(&self, repo: &str, digest: &Digest) -> Result<Vec<u8>, StorageError> {
        self.resolve(repo).blob_read(repo, digest).await
    }

    async fn blob_write(
        &self,
        repo: &str,
        expected: &Digest,
        content: &[u8],
    ) -> Result<(), StorageError> {
        self.resolve(repo).blob_write(repo, expected, content).await
    }

    async fn blob_delete(&self, repo: &str, digest: &Digest) -> Result<(), StorageError> {
        self.resolve(repo).blob_delete(repo, digest).await
    }

    // ---- Upload sessions ------------------------------------------------

    async fn upload_create(&self, repo: &str) -> Result<UploadStatus, StorageError> {
        self.resolve(repo).upload_create(repo).await
    }

    async fn upload_status(&self, repo: &str, id: Uuid) -> Result<UploadStatus, StorageError> {
        self.resolve(repo).upload_status(repo, id).await
    }

    async fn upload_append(
        &self,
        repo: &str,
        id: Uuid,
        chunk: &[u8],
    ) -> Result<UploadStatus, StorageError> {
        self.resolve(repo).upload_append(repo, id, chunk).await
    }

    async fn upload_finalize(
        &self,
        repo: &str,
        id: Uuid,
        expected: &Digest,
    ) -> Result<(), StorageError> {
        self.resolve(repo).upload_finalize(repo, id, expected).await
    }

    async fn upload_cancel(&self, repo: &str, id: Uuid) -> Result<(), StorageError> {
        self.resolve(repo).upload_cancel(repo, id).await
    }

    // ---- Manifests ------------------------------------------------------

    async fn manifest_get(
        &self,
        repo: &str,
        reference: &Reference,
    ) -> Result<Vec<u8>, StorageError> {
        self.resolve(repo).manifest_get(repo, reference).await
    }

    async fn manifest_put(
        &self,
        repo: &str,
        reference: &Reference,
        content: &[u8],
    ) -> Result<Digest, StorageError> {
        self.resolve(repo).manifest_put(repo, reference, content).await
    }

    async fn manifest_delete(
        &self,
        repo: &str,
        reference: &Reference,
    ) -> Result<(), StorageError> {
        self.resolve(repo).manifest_delete(repo, reference).await
    }

    // ---- Listing --------------------------------------------------------
    //
    // List ops union across every distinct backend. We dedupe by Arc
    // identity, not by name — two rules sharing the same `Arc<dyn
    // Storage>` would otherwise double-count.

    async fn list_repos(&self) -> Result<Vec<String>, StorageError> {
        let mut set: BTreeSet<String> = BTreeSet::new();
        for backend in self.distinct_backends() {
            for r in backend.list_repos().await? {
                set.insert(r);
            }
        }
        Ok(set.into_iter().collect())
    }

    async fn list_tags(&self, repo: &str) -> Result<Vec<String>, StorageError> {
        self.resolve(repo).list_tags(repo).await
    }

    async fn list_manifest_digests(&self, repo: &str) -> Result<Vec<Digest>, StorageError> {
        self.resolve(repo).list_manifest_digests(repo).await
    }

    async fn list_all_blobs(&self) -> Result<Vec<Digest>, StorageError> {
        let mut set: BTreeSet<Digest> = BTreeSet::new();
        for backend in self.distinct_backends() {
            for d in backend.list_all_blobs().await? {
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

    /// Bare-minimum Storage stub that records which repos it's seen
    /// writes for. We use it to assert the router dispatched to the
    /// right backend.
    #[derive(Default)]
    struct Recorder {
        name: String,
        writes: tokio::sync::Mutex<Vec<String>>,
    }

    impl Recorder {
        fn new(name: &str) -> Arc<Self> {
            Arc::new(Self {
                name: name.into(),
                writes: Default::default(),
            })
        }
        async fn writes(&self) -> Vec<String> {
            self.writes.lock().await.clone()
        }
    }

    #[async_trait]
    impl Storage for Recorder {
        async fn blob_exists(&self, _r: &str, _d: &Digest) -> Result<bool, StorageError> {
            Ok(false)
        }
        async fn blob_size(&self, _r: &str, _d: &Digest) -> Result<u64, StorageError> {
            Err(StorageError::NotFound)
        }
        async fn blob_read(&self, _r: &str, _d: &Digest) -> Result<Vec<u8>, StorageError> {
            Err(StorageError::NotFound)
        }
        async fn blob_write(
            &self,
            r: &str,
            _d: &Digest,
            _c: &[u8],
        ) -> Result<(), StorageError> {
            self.writes.lock().await.push(format!("{}:{}", self.name, r));
            Ok(())
        }
        async fn blob_delete(&self, _r: &str, _d: &Digest) -> Result<(), StorageError> {
            Ok(())
        }
        async fn upload_create(&self, _r: &str) -> Result<UploadStatus, StorageError> {
            Ok(UploadStatus {
                id: Uuid::nil(),
                offset: 0,
            })
        }
        async fn upload_status(&self, _r: &str, _id: Uuid) -> Result<UploadStatus, StorageError> {
            Err(StorageError::NotFound)
        }
        async fn upload_append(
            &self,
            _r: &str,
            id: Uuid,
            _c: &[u8],
        ) -> Result<UploadStatus, StorageError> {
            Ok(UploadStatus { id, offset: 0 })
        }
        async fn upload_finalize(
            &self,
            _r: &str,
            _id: Uuid,
            _d: &Digest,
        ) -> Result<(), StorageError> {
            Ok(())
        }
        async fn upload_cancel(&self, _r: &str, _id: Uuid) -> Result<(), StorageError> {
            Ok(())
        }
        async fn manifest_get(
            &self,
            _r: &str,
            _ref: &Reference,
        ) -> Result<Vec<u8>, StorageError> {
            Err(StorageError::NotFound)
        }
        async fn manifest_put(
            &self,
            r: &str,
            _ref: &Reference,
            _c: &[u8],
        ) -> Result<Digest, StorageError> {
            self.writes.lock().await.push(format!("{}:{}", self.name, r));
            Ok(Digest {
                algorithm: Algorithm::Sha256,
                hex: "0".repeat(64),
            })
        }
        async fn manifest_delete(&self, _r: &str, _ref: &Reference) -> Result<(), StorageError> {
            Ok(())
        }
        async fn list_repos(&self) -> Result<Vec<String>, StorageError> {
            Ok(vec![self.name.clone()])
        }
        async fn list_tags(&self, _r: &str) -> Result<Vec<String>, StorageError> {
            Ok(vec![])
        }
        async fn list_manifest_digests(&self, _r: &str) -> Result<Vec<Digest>, StorageError> {
            Ok(vec![])
        }
        async fn list_all_blobs(&self) -> Result<Vec<Digest>, StorageError> {
            Ok(vec![])
        }
    }

    fn dummy_digest() -> Digest {
        Digest {
            algorithm: Algorithm::Sha256,
            hex: "0".repeat(64),
        }
    }

    #[tokio::test]
    async fn exact_pattern_wins_over_wildcard() {
        let fast = Recorder::new("fast");
        let slow = Recorder::new("slow");
        let router = RepoRouter::new(vec![
            RouteRule {
                pattern: "4.18.2/kernel".into(),
                backend: fast.clone() as Arc<dyn Storage>,
            },
            RouteRule {
                pattern: "*".into(),
                backend: slow.clone() as Arc<dyn Storage>,
            },
        ])
        .unwrap();

        router
            .blob_write("4.18.2/kernel", &dummy_digest(), b"x")
            .await
            .unwrap();
        router
            .blob_write("4.18.2/system", &dummy_digest(), b"x")
            .await
            .unwrap();

        assert_eq!(fast.writes().await, vec!["fast:4.18.2/kernel"]);
        assert_eq!(slow.writes().await, vec!["slow:4.18.2/system"]);
    }

    #[tokio::test]
    async fn longest_prefix_wins() {
        let kernel = Recorder::new("kernel");
        let group = Recorder::new("group");
        let global = Recorder::new("global");
        let router = RepoRouter::new(vec![
            RouteRule {
                pattern: "4.18.2/kernel".into(),
                backend: kernel.clone() as Arc<dyn Storage>,
            },
            RouteRule {
                pattern: "4.18.2/*".into(),
                backend: group.clone() as Arc<dyn Storage>,
            },
            RouteRule {
                pattern: "*".into(),
                backend: global.clone() as Arc<dyn Storage>,
            },
        ])
        .unwrap();

        router
            .blob_write("4.18.2/kernel", &dummy_digest(), b"x")
            .await
            .unwrap();
        router
            .blob_write("4.18.2/system", &dummy_digest(), b"x")
            .await
            .unwrap();
        router
            .blob_write("other/repo", &dummy_digest(), b"x")
            .await
            .unwrap();

        assert_eq!(kernel.writes().await, vec!["kernel:4.18.2/kernel"]);
        assert_eq!(group.writes().await, vec!["group:4.18.2/system"]);
        assert_eq!(global.writes().await, vec!["global:other/repo"]);
    }

    #[tokio::test]
    async fn repoint_swaps_backend_for_existing_pattern() {
        let original = Recorder::new("original");
        let replacement = Recorder::new("replacement");
        let router = RepoRouter::new(vec![RouteRule {
            pattern: "*".into(),
            backend: original.clone() as Arc<dyn Storage>,
        }])
        .unwrap();

        router
            .blob_write("foo/bar", &dummy_digest(), b"x")
            .await
            .unwrap();
        assert_eq!(original.writes().await, vec!["original:foo/bar"]);

        let n = router.repoint("*", replacement.clone() as Arc<dyn Storage>);
        assert_eq!(n, 1);

        router
            .blob_write("foo/bar", &dummy_digest(), b"x")
            .await
            .unwrap();
        assert_eq!(replacement.writes().await, vec!["replacement:foo/bar"]);
        // Original sees no new writes after the swap.
        assert_eq!(original.writes().await, vec!["original:foo/bar"]);
    }

    #[tokio::test]
    async fn upsert_adds_new_rule_or_swaps_existing() {
        let a = Recorder::new("a");
        let b = Recorder::new("b");
        let router = RepoRouter::new(vec![RouteRule {
            pattern: "*".into(),
            backend: a.clone() as Arc<dyn Storage>,
        }])
        .unwrap();

        let added = router.upsert("prod/*".into(), b.clone() as Arc<dyn Storage>);
        assert!(added);

        router
            .blob_write("prod/svc", &dummy_digest(), b"x")
            .await
            .unwrap();
        router
            .blob_write("dev/svc", &dummy_digest(), b"x")
            .await
            .unwrap();

        assert_eq!(b.writes().await, vec!["b:prod/svc"]);
        assert_eq!(a.writes().await, vec!["a:dev/svc"]);

        // Upserting the same pattern is a swap, not a duplicate.
        let c = Recorder::new("c");
        let added = router.upsert("prod/*".into(), c.clone() as Arc<dyn Storage>);
        assert!(!added);
        assert_eq!(router.rules().len(), 2);
    }

    #[tokio::test]
    async fn list_repos_unions_across_backends() {
        let a = Recorder::new("alpha");
        let b = Recorder::new("beta");
        let router = RepoRouter::new(vec![
            RouteRule {
                pattern: "alpha/*".into(),
                backend: a.clone() as Arc<dyn Storage>,
            },
            RouteRule {
                pattern: "*".into(),
                backend: b.clone() as Arc<dyn Storage>,
            },
        ])
        .unwrap();

        let repos = router.list_repos().await.unwrap();
        // Recorder returns its own name as the only repo it knows.
        let mut expected = vec!["alpha".to_string(), "beta".to_string()];
        expected.sort();
        let mut got = repos;
        got.sort();
        assert_eq!(got, expected);
    }

    #[test]
    fn empty_rules_rejected() {
        let r = RepoRouter::new(vec![]);
        assert!(matches!(r, Err(StorageError::Invalid(_))));
    }
}
