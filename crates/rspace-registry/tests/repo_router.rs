//! End-to-end repo-router integration tests against the HTTP service.
//!
//! Exercises:
//!   - push to `4.18.2/kernel` lands under the exact-match mount; push
//!     to `4.18.2/system` falls to the `4.18.2/*` group rule
//!   - `GET /admin/repo-roots` reports the ruleset
//!   - `POST /admin/repo-root` adds a new rule and subsequent writes
//!     dispatch to it
//!   - `POST /admin/repo-root` repointing an existing pattern swaps
//!     where subsequent writes land (no restart)

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, StatusCode};
use http_body_util::BodyExt;
use rspace_registry::{build_router, AppState};
use rspace_registry_core::{RepoRouter, RouteRule, Storage};
use rspace_registry_fs::FsStorage;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

struct Harness {
    app: axum::Router,
    fast: Arc<FsStorage>,
    slow: Arc<FsStorage>,
    default: Arc<FsStorage>,
    _dirs: Vec<tempfile::TempDir>,
}

fn harness() -> Harness {
    let fast_dir = tempfile::tempdir().unwrap();
    let slow_dir = tempfile::tempdir().unwrap();
    let default_dir = tempfile::tempdir().unwrap();
    let fast = Arc::new(FsStorage::new(fast_dir.path()).unwrap());
    let slow = Arc::new(FsStorage::new(slow_dir.path()).unwrap());
    let default = Arc::new(FsStorage::new(default_dir.path()).unwrap());

    let router = Arc::new(
        RepoRouter::new(vec![
            RouteRule {
                pattern: "4.18.2/kernel".into(),
                backend: fast.clone() as Arc<dyn Storage>,
            },
            RouteRule {
                pattern: "4.18.2/*".into(),
                backend: slow.clone() as Arc<dyn Storage>,
            },
            RouteRule {
                pattern: "*".into(),
                backend: default.clone() as Arc<dyn Storage>,
            },
        ])
        .unwrap(),
    );
    let storage: Arc<dyn Storage> = router.clone();
    let state = AppState::new(storage).with_router(router);
    Harness {
        app: build_router(state),
        fast,
        slow,
        default,
        _dirs: vec![fast_dir, slow_dir, default_dir],
    }
}

async fn send(
    app: &axum::Router,
    method: Method,
    path: &str,
    body: Option<Vec<u8>>,
    content_type: Option<&str>,
) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(ct) = content_type {
        builder = builder.header("content-type", HeaderValue::from_str(ct).unwrap());
    }
    let req = builder
        .body(body.map(Body::from).unwrap_or_else(Body::empty))
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

fn sha256_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

async fn push_blob(app: &axum::Router, repo: &str, bytes: &[u8]) -> String {
    let digest = format!("sha256:{}", sha256_hex(bytes));
    let (status, _) = send(
        app,
        Method::POST,
        &format!("/v2/{repo}/blobs/uploads?digest={digest}"),
        Some(bytes.to_vec()),
        Some("application/octet-stream"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    digest
}

#[tokio::test]
async fn push_dispatches_by_longest_prefix() {
    let h = harness();
    let kernel_blob = b"kernel-bytes";
    let system_blob = b"system-bytes";
    let unrelated_blob = b"unrelated-bytes";

    push_blob(&h.app, "4.18.2/kernel", kernel_blob).await;
    push_blob(&h.app, "4.18.2/system", system_blob).await;
    push_blob(&h.app, "other/repo", unrelated_blob).await;

    let kd = rspace_registry_core::Digest::try_from(format!("sha256:{}", sha256_hex(kernel_blob)))
        .unwrap();
    let sd = rspace_registry_core::Digest::try_from(format!("sha256:{}", sha256_hex(system_blob)))
        .unwrap();
    let ud =
        rspace_registry_core::Digest::try_from(format!("sha256:{}", sha256_hex(unrelated_blob)))
            .unwrap();

    assert!(h.fast.blob_exists("4.18.2/kernel", &kd).await.unwrap());
    assert!(!h.fast.blob_exists("4.18.2/system", &sd).await.unwrap());
    assert!(h.slow.blob_exists("4.18.2/system", &sd).await.unwrap());
    assert!(!h.slow.blob_exists("4.18.2/kernel", &kd).await.unwrap());
    assert!(h.default.blob_exists("other/repo", &ud).await.unwrap());
}

#[tokio::test]
async fn repo_roots_list_reports_three_rules() {
    let h = harness();
    let (status, body) = send(&h.app, Method::GET, "/admin/repo-roots", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let parsed: Value = serde_json::from_slice(&body).unwrap();
    let rules = parsed["rules"].as_array().unwrap();
    assert_eq!(rules.len(), 3);
    assert_eq!(rules[0]["pattern"], "4.18.2/kernel");
    assert_eq!(rules[1]["pattern"], "4.18.2/*");
    assert_eq!(rules[2]["pattern"], "*");
}

#[tokio::test]
async fn admin_repo_root_adds_new_rule_and_dispatches_to_it() {
    let h = harness();
    let new_root = tempfile::tempdir().unwrap();

    let req = serde_json::json!({
        "pattern": "tenant/secret/*",
        "root": new_root.path().to_str().unwrap(),
    });
    let (status, body) = send(
        &h.app,
        Method::POST,
        "/admin/repo-root",
        Some(req.to_string().into_bytes()),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parsed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["added"], true);
    assert_eq!(parsed["rule_count"], 4);

    // A push to a matching repo now lands on the new root, not on the
    // default catchall.
    push_blob(&h.app, "tenant/secret/svc", b"secret-bytes").await;
    let sd =
        rspace_registry_core::Digest::try_from(format!("sha256:{}", sha256_hex(b"secret-bytes")))
            .unwrap();
    // It should NOT have landed on the default (catchall) backend.
    assert!(!h
        .default
        .blob_exists("tenant/secret/svc", &sd)
        .await
        .unwrap());
}

#[tokio::test]
async fn admin_repo_root_repoint_redirects_subsequent_writes() {
    let h = harness();
    // Start: push lands on the default catchall.
    push_blob(&h.app, "x/y", b"before").await;
    let bd = rspace_registry_core::Digest::try_from(format!("sha256:{}", sha256_hex(b"before")))
        .unwrap();
    assert!(h.default.blob_exists("x/y", &bd).await.unwrap());

    // Repoint the `*` rule to a fresh root (simulating rspacefs-pvc pivot).
    let new_root = tempfile::tempdir().unwrap();
    let req = serde_json::json!({
        "pattern": "*",
        "root": new_root.path().to_str().unwrap(),
    });
    let (status, body) = send(
        &h.app,
        Method::POST,
        "/admin/repo-root",
        Some(req.to_string().into_bytes()),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parsed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        parsed["added"], false,
        "repoint of an existing pattern must not add"
    );

    // After repoint, a new write should land on the new root.
    push_blob(&h.app, "x/y", b"after").await;
    let ad =
        rspace_registry_core::Digest::try_from(format!("sha256:{}", sha256_hex(b"after"))).unwrap();
    let post = Arc::new(FsStorage::new(new_root.path()).unwrap());
    assert!(post.blob_exists("x/y", &ad).await.unwrap());
    // And the old default is unchanged (still holds the `before` blob).
    assert!(h.default.blob_exists("x/y", &bd).await.unwrap());
    assert!(!h.default.blob_exists("x/y", &ad).await.unwrap());
}

#[tokio::test]
async fn admin_repo_root_endpoints_404_without_router() {
    // Single-backend AppState — admin/repo-root endpoints shouldn't
    // be routable.
    let tmp = tempfile::tempdir().unwrap();
    let storage = Arc::new(FsStorage::new(tmp.path()).unwrap());
    let state = AppState::new(storage);
    let app = build_router(state);

    let (status, _) = send(&app, Method::GET, "/admin/repo-roots", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let req = serde_json::json!({"pattern": "*", "root": "/tmp"});
    let (status, _) = send(
        &app,
        Method::POST,
        "/admin/repo-root",
        Some(req.to_string().into_bytes()),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
