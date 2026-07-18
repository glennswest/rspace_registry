//! Live class migration (drain + cutover) between storage roots.
//!
//! Seeds a `data/*` volume with an image (config + layer + tagged
//! manifest), migrates the class onto a fresh volume, and asserts the
//! bytes moved, the route cut over, and — with `drain` — the old volume
//! was reclaimed.

use std::str::FromStr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use rspace_registry::{build_router, AppState};
use rspace_registry_core::{migrate, Digest, Reference, RepoRouter, RouteRule, Storage};
use rspace_registry_fs::FsStorage;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tower::ServiceExt;

fn sha256_digest(b: &[u8]) -> Digest {
    Digest::from_str(&format!("sha256:{}", hex::encode(Sha256::digest(b)))).unwrap()
}

/// Seed `repo` on `backend` with a minimal but real OCI image: a config
/// blob, one layer blob, and a `latest` tag pointing at the manifest.
/// Returns the two blob digests.
async fn seed_image(backend: &dyn Storage, repo: &str) -> (Digest, Digest) {
    let config = br#"{"architecture":"amd64","os":"linux"}"#;
    let layer = b"a fake layer's worth of bytes";
    let config_d = sha256_digest(config);
    let layer_d = sha256_digest(layer);

    backend.blob_write(repo, &config_d, config).await.unwrap();
    backend.blob_write(repo, &layer_d, layer).await.unwrap();

    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{config_d}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{layer_d}","size":{}}}]}}"#,
        config.len(),
        layer.len()
    );
    backend
        .manifest_put(repo, &Reference::Tag("latest".into()), manifest.as_bytes())
        .await
        .unwrap();

    (config_d, layer_d)
}

fn router_with(data: Arc<dyn Storage>, default: Arc<dyn Storage>) -> RepoRouter {
    RepoRouter::new(vec![
        RouteRule {
            pattern: "data/*".into(),
            backend: data,
        },
        RouteRule {
            pattern: "*".into(),
            backend: default,
        },
    ])
    .unwrap()
}

#[tokio::test]
async fn migrate_copies_class_and_cuts_over() {
    let old_dir = tempfile::tempdir().unwrap();
    let new_dir = tempfile::tempdir().unwrap();
    let def_dir = tempfile::tempdir().unwrap();

    let old = Arc::new(FsStorage::new(old_dir.path()).unwrap()) as Arc<dyn Storage>;
    let new = Arc::new(FsStorage::new(new_dir.path()).unwrap()) as Arc<dyn Storage>;
    let default = Arc::new(FsStorage::new(def_dir.path()).unwrap()) as Arc<dyn Storage>;

    let (config_d, layer_d) = seed_image(old.as_ref(), "data/vol1").await;
    let router = router_with(old.clone(), default.clone());

    let report = migrate::run(&router, "data/*", new.clone(), false)
        .await
        .unwrap();

    assert!(report.cutover);
    assert_eq!(report.repos_migrated, 1);
    assert_eq!(report.blobs_copied, 2);
    assert!(report.manifests_copied >= 1);

    // Bytes are now on the new volume.
    assert!(new.blob_exists("data/vol1", &config_d).await.unwrap());
    assert!(new.blob_exists("data/vol1", &layer_d).await.unwrap());
    assert!(new
        .manifest_get("data/vol1", &Reference::Tag("latest".into()))
        .await
        .is_ok());

    // The route cut over — future ops for data/* land on new.
    assert!(Arc::ptr_eq(&router.backend_for("data/vol1"), &new));

    // Without drain, the old volume still holds its copy.
    assert!(old.blob_exists("data/vol1", &config_d).await.unwrap());
}

#[tokio::test]
async fn migrate_is_idempotent() {
    let old_dir = tempfile::tempdir().unwrap();
    let new_dir = tempfile::tempdir().unwrap();
    let def_dir = tempfile::tempdir().unwrap();

    let old = Arc::new(FsStorage::new(old_dir.path()).unwrap()) as Arc<dyn Storage>;
    let new = Arc::new(FsStorage::new(new_dir.path()).unwrap()) as Arc<dyn Storage>;
    let default = Arc::new(FsStorage::new(def_dir.path()).unwrap()) as Arc<dyn Storage>;

    seed_image(old.as_ref(), "data/vol1").await;
    let router = router_with(old.clone(), default.clone());

    migrate::run(&router, "data/*", new.clone(), false)
        .await
        .unwrap();
    // Re-run against the already-migrated new backend (now the active one):
    // pattern already points at `new`, so it's a no-op copy.
    let again = migrate::run(&router, "data/*", new.clone(), false)
        .await
        .unwrap();
    assert_eq!(again.blobs_copied, 0);
    assert_eq!(again.manifests_copied, 0);
}

#[tokio::test]
async fn migrate_with_drain_reclaims_old_volume() {
    let old_dir = tempfile::tempdir().unwrap();
    let new_dir = tempfile::tempdir().unwrap();
    let def_dir = tempfile::tempdir().unwrap();

    let old = Arc::new(FsStorage::new(old_dir.path()).unwrap()) as Arc<dyn Storage>;
    let new = Arc::new(FsStorage::new(new_dir.path()).unwrap()) as Arc<dyn Storage>;
    let default = Arc::new(FsStorage::new(def_dir.path()).unwrap()) as Arc<dyn Storage>;

    let (config_d, layer_d) = seed_image(old.as_ref(), "data/vol1").await;
    let router = router_with(old.clone(), default.clone());

    let report = migrate::run(&router, "data/*", new.clone(), true)
        .await
        .unwrap();

    assert!(report.cutover);
    assert_eq!(report.blobs_purged, 2, "both blobs swept from old");
    assert!(report.bytes_purged > 0);

    // New volume has the content; old volume is drained.
    assert!(new.blob_exists("data/vol1", &config_d).await.unwrap());
    assert!(!old.blob_exists("data/vol1", &config_d).await.unwrap());
    assert!(!old.blob_exists("data/vol1", &layer_d).await.unwrap());
    assert!(old
        .manifest_get("data/vol1", &Reference::Tag("latest".into()))
        .await
        .is_err());
}

#[tokio::test]
async fn migrate_unknown_pattern_errors() {
    let old_dir = tempfile::tempdir().unwrap();
    let new_dir = tempfile::tempdir().unwrap();
    let old = Arc::new(FsStorage::new(old_dir.path()).unwrap()) as Arc<dyn Storage>;
    let new = Arc::new(FsStorage::new(new_dir.path()).unwrap()) as Arc<dyn Storage>;
    let router = RepoRouter::single(old);

    // No rule keyed exactly "data/*" (only the catchall "*").
    let r = migrate::run(&router, "data/*", new, false).await;
    assert!(r.is_err());
}

#[tokio::test]
async fn migrate_leaves_more_specific_rules_alone() {
    // data/* on `old`, but data/keep pinned to `pinned` by a longer rule.
    // Migrating data/* must not move data/keep.
    let old_dir = tempfile::tempdir().unwrap();
    let new_dir = tempfile::tempdir().unwrap();
    let pin_dir = tempfile::tempdir().unwrap();

    let old = Arc::new(FsStorage::new(old_dir.path()).unwrap()) as Arc<dyn Storage>;
    let new = Arc::new(FsStorage::new(new_dir.path()).unwrap()) as Arc<dyn Storage>;
    let pinned = Arc::new(FsStorage::new(pin_dir.path()).unwrap()) as Arc<dyn Storage>;

    let (moved_cfg, _) = seed_image(old.as_ref(), "data/vol1").await;
    seed_image(pinned.as_ref(), "data/keep").await;

    let router = RepoRouter::new(vec![
        RouteRule {
            pattern: "data/keep".into(),
            backend: pinned.clone(),
        },
        RouteRule {
            pattern: "data/*".into(),
            backend: old.clone(),
        },
        RouteRule {
            pattern: "*".into(),
            backend: old.clone(),
        },
    ])
    .unwrap();

    let report = migrate::run(&router, "data/*", new.clone(), false)
        .await
        .unwrap();

    // Only data/vol1 moved; data/keep stayed on its pinned backend. Prove
    // it via manifests (per-repo) — blobs are content-addressed globally
    // within a root, so a shared-bytes blob is not a per-repo signal.
    assert_eq!(report.repos_migrated, 1);
    assert!(new.blob_exists("data/vol1", &moved_cfg).await.unwrap());
    assert!(
        new.manifest_get("data/vol1", &Reference::Tag("latest".into()))
            .await
            .is_ok(),
        "data/vol1 manifest migrated to new"
    );
    assert!(
        new.manifest_get("data/keep", &Reference::Tag("latest".into()))
            .await
            .is_err(),
        "data/keep manifest must NOT be on new"
    );
    assert!(Arc::ptr_eq(&router.backend_for("data/keep"), &pinned));
}

#[tokio::test]
async fn admin_repo_migrate_endpoint_moves_and_cuts_over() {
    let old_dir = tempfile::tempdir().unwrap();
    let def_dir = tempfile::tempdir().unwrap();
    let new_dir = tempfile::tempdir().unwrap();

    let old = Arc::new(FsStorage::new(old_dir.path()).unwrap());
    let default = Arc::new(FsStorage::new(def_dir.path()).unwrap());
    seed_image(old.as_ref(), "data/vol1").await;

    let router = Arc::new(
        RepoRouter::new(vec![
            RouteRule {
                pattern: "data/*".into(),
                backend: old.clone() as Arc<dyn Storage>,
            },
            RouteRule {
                pattern: "*".into(),
                backend: default.clone() as Arc<dyn Storage>,
            },
        ])
        .unwrap(),
    );
    let storage: Arc<dyn Storage> = router.clone();
    let state = AppState::new(storage).with_router(router.clone());
    let app = build_router(state);

    // Migrate data/* onto the new volume, draining the old.
    let body = serde_json::json!({
        "pattern": "data/*",
        "to": new_dir.path().to_str().unwrap(),
        "drain": true,
    });
    let req = Request::builder()
        .method(Method::POST)
        .uri("/admin/repo-migrate")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["cutover"], true);
    assert_eq!(v["repos_migrated"], 1);
    assert_eq!(v["blobs_copied"], 2);
    assert_eq!(v["blobs_purged"], 2);

    // The route now points at the new volume, so a pull of data/vol1
    // through the HTTP surface is served from there.
    let (status, _) = get(&app, "/v2/data/vol1/manifests/latest").await;
    assert_eq!(status, StatusCode::OK);
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method(Method::GET)
        .uri(path)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, bytes)
}
