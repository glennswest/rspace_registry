//! Per-class storage quota enforcement.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use rspace_registry::{build_router, AppState};
use rspace_registry_core::{
    Digest, Quota, QuotaStorage, RepoRouter, RouteRule, Storage, StorageError,
};
use rspace_registry_fs::FsStorage;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tower::ServiceExt;

fn dg(b: &[u8]) -> Digest {
    Digest::from_str(&format!("sha256:{}", hex::encode(Sha256::digest(b)))).unwrap()
}

/// Router: `data/*` → `data` volume with a quota, everything else → `other`
/// volume with no quota.
fn quota_storage(max_bytes: u64) -> (Arc<QuotaStorage>, Vec<tempfile::TempDir>) {
    let data_dir = tempfile::tempdir().unwrap();
    let other_dir = tempfile::tempdir().unwrap();
    let data = Arc::new(FsStorage::new(data_dir.path()).unwrap()) as Arc<dyn Storage>;
    let other = Arc::new(FsStorage::new(other_dir.path()).unwrap()) as Arc<dyn Storage>;
    let router = Arc::new(
        RepoRouter::new(vec![
            RouteRule {
                pattern: "data/*".into(),
                backend: data,
            },
            RouteRule {
                pattern: "*".into(),
                backend: other,
            },
        ])
        .unwrap(),
    );
    let qs = Arc::new(QuotaStorage::new(
        router,
        vec![Quota {
            pattern: "data/*".into(),
            max_bytes,
        }],
        Duration::from_secs(30),
    ));
    (qs, vec![data_dir, other_dir])
}

fn blob(n: usize, fill: u8) -> Vec<u8> {
    vec![fill; n]
}

#[tokio::test]
async fn write_under_quota_succeeds_over_quota_rejected() {
    let (qs, _dirs) = quota_storage(100);

    // 60 bytes under the 100-byte cap — accepted.
    let b1 = blob(60, 1);
    qs.blob_write("data/vol1", &dg(&b1), &b1).await.unwrap();

    // A second distinct 60-byte blob would total 120 > 100 — rejected.
    let b2 = blob(60, 2);
    let err = qs.blob_write("data/vol2", &dg(&b2), &b2).await.unwrap_err();
    assert!(matches!(err, StorageError::QuotaExceeded(_)), "got {err:?}");
}

#[tokio::test]
async fn idempotent_repush_of_existing_blob_not_rejected() {
    let (qs, _dirs) = quota_storage(100);
    let b = blob(90, 7);
    qs.blob_write("data/vol1", &dg(&b), &b).await.unwrap();
    // Re-pushing the SAME blob adds no bytes — must not trip the quota even
    // though a fresh 90-byte write would (90 + 90 > 100).
    qs.blob_write("data/vol1", &dg(&b), &b).await.unwrap();
}

#[tokio::test]
async fn unmatched_repo_is_unlimited() {
    let (qs, _dirs) = quota_storage(100);
    // `other/*` matches no quota — a 500-byte blob is fine.
    let b = blob(500, 3);
    qs.blob_write("other/thing", &dg(&b), &b).await.unwrap();
}

#[tokio::test]
async fn report_shows_usage_against_limit() {
    let (qs, _dirs) = quota_storage(1000);
    let b = blob(250, 9);
    qs.blob_write("data/vol1", &dg(&b), &b).await.unwrap();

    let report = qs.report().await;
    assert_eq!(report.len(), 1);
    assert_eq!(report[0].pattern, "data/*");
    assert_eq!(report[0].max_bytes, 1000);
    assert_eq!(report[0].used_bytes, Some(250));
}

#[tokio::test]
async fn http_push_over_quota_returns_413_and_admin_reports() {
    let (qs, _dirs) = quota_storage(100);
    // The QuotaStorage is the top-level storage; the admin quota endpoint is
    // backed by the same handle.
    let storage: Arc<dyn Storage> = qs.clone();
    let state = AppState::new(storage).with_quota(qs.clone());
    let app = build_router(state);

    // Monolithic upload of a 200-byte blob to data/* — exceeds the 100 cap.
    let body = blob(200, 5);
    let digest = dg(&body);
    let (status, _) = send(
        &app,
        Method::POST,
        &format!("/v2/data/vol1/blobs/uploads/?digest={digest}"),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

    // A small blob under the cap goes through.
    let small = blob(50, 6);
    let sd = dg(&small);
    let (status, _) = send(
        &app,
        Method::POST,
        &format!("/v2/data/vol1/blobs/uploads/?digest={sd}"),
        Some(small),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Admin endpoint reports usage.
    let (status, bytes) = send(&app, Method::GET, "/admin/quotas", None).await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let q = &v["quotas"][0];
    assert_eq!(q["pattern"], "data/*");
    assert_eq!(q["max_bytes"], 100);
    assert_eq!(q["used_bytes"], 50);
}

async fn send(
    app: &axum::Router,
    method: Method,
    path: &str,
    body: Option<Vec<u8>>,
) -> (StatusCode, Vec<u8>) {
    let req = Request::builder()
        .method(method)
        .uri(path)
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
