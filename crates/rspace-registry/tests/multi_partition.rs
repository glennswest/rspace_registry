//! End-to-end multi-partition flow:
//!   1. Run the registry over a MultiStore of two FS partitions (A primary).
//!   2. Push a tagged image — only A receives it.
//!   3. `POST /admin/replicate` — B catches up.
//!   4. `GET /admin/partitions` reports both with matching blob counts.
//!   5. Push another image; confirm `replicate?tag_glob=prod-*` only
//!      copies the prod-tagged one.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, StatusCode};
use http_body_util::BodyExt;
use rspace_registry::{build_router, AppState};
use rspace_registry_core::{MultiStore, Partition, Storage};
use rspace_registry_fs::FsStorage;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

struct Harness {
    app: axum::Router,
    multi: Arc<MultiStore>,
    _a_dir: tempfile::TempDir,
    _b_dir: tempfile::TempDir,
}

fn harness() -> Harness {
    let a_dir = tempfile::tempdir().expect("a tmp");
    let b_dir = tempfile::tempdir().expect("b tmp");
    let a = Arc::new(FsStorage::new(a_dir.path()).expect("a fs"));
    let b = Arc::new(FsStorage::new(b_dir.path()).expect("b fs"));
    let multi = Arc::new(
        MultiStore::new(
            vec![
                Partition { name: "a".into(), storage: a as Arc<dyn Storage> },
                Partition { name: "b".into(), storage: b as Arc<dyn Storage> },
            ],
            "a",
        )
        .unwrap(),
    );
    let storage: Arc<dyn Storage> = multi.clone();
    let state = AppState::new(storage).with_multi(multi.clone());
    Harness {
        app: build_router(state),
        multi,
        _a_dir: a_dir,
        _b_dir: b_dir,
    }
}

async fn send(
    app: &axum::Router,
    method: Method,
    path: &str,
    body: Option<Vec<u8>>,
    content_type: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(ct) = content_type {
        builder = builder.header("content-type", HeaderValue::from_str(ct).unwrap());
    }
    let req = builder
        .body(body.map(Body::from).unwrap_or_else(Body::empty))
        .unwrap();
    let resp = app.clone().oneshot(req).await.expect("request");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect body")
        .to_bytes()
        .to_vec();
    (status, headers, bytes)
}

fn sha256_hex(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

async fn push_image(app: &axum::Router, repo: &str, tag: &str, layer: &[u8]) -> String {
    let layer_digest = format!("sha256:{}", sha256_hex(layer));
    let config = format!(r#"{{"id":"{tag}"}}"#);
    let config_digest = format!("sha256:{}", sha256_hex(config.as_bytes()));

    // Monolithic blob upload x2.
    for (bytes, digest) in [(layer, &layer_digest), (config.as_bytes(), &config_digest)] {
        let (status, _h, _) = send(
            app,
            Method::POST,
            &format!("/v2/{repo}/blobs/uploads?digest={digest}"),
            Some(bytes.to_vec()),
            Some("application/octet-stream"),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest,
            "size": config.len(),
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": layer_digest,
            "size": layer.len(),
        }],
    });
    let m_bytes = serde_json::to_vec(&manifest).unwrap();
    let (status, _h, _) = send(
        app,
        Method::PUT,
        &format!("/v2/{repo}/manifests/{tag}"),
        Some(m_bytes.clone()),
        Some("application/vnd.oci.image.manifest.v1+json"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    format!("sha256:{}", sha256_hex(&m_bytes))
}

#[tokio::test]
async fn write_lands_on_primary_only() {
    let h = harness();
    let _ = push_image(&h.app, "app/one", "v1", b"layer-bytes-A").await;
    let a = &h.multi.partitions()[0].storage;
    let b = &h.multi.partitions()[1].storage;
    assert_eq!(a.list_repos().await.unwrap(), vec!["app/one"]);
    assert!(b.list_repos().await.unwrap().is_empty(), "secondary untouched");
}

#[tokio::test]
async fn replicate_all_then_partitions_report_matching_counts() {
    let h = harness();
    let _ = push_image(&h.app, "app/one", "v1", b"layer-bytes-1").await;
    let _ = push_image(&h.app, "app/two", "v2", b"layer-bytes-2-larger").await;

    // Trigger replication.
    let (status, _h, body) =
        send(&h.app, Method::POST, "/admin/replicate", Some(b"{}".to_vec()), Some("application/json"))
            .await;
    assert_eq!(status, StatusCode::OK);
    let report: Value = serde_json::from_slice(&body).unwrap();
    // Each push emits two distinct blobs (layer differs by content,
    // config differs by the tag string baked into it). Two pushes →
    // four blobs and two manifests copy across.
    assert_eq!(report["blobs_copied"], 4, "report: {report}");
    assert_eq!(report["manifests_copied"], 2);

    // /admin/partitions should now show identical counts.
    let (status, _h, body) = send(&h.app, Method::GET, "/admin/partitions", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let parsed: Value = serde_json::from_slice(&body).unwrap();
    let parts = parsed["partitions"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    let a = &parts[0];
    let b = &parts[1];
    assert_eq!(a["name"], "a");
    assert_eq!(a["primary"], true);
    assert_eq!(b["primary"], false);
    assert_eq!(a["blob_count"], b["blob_count"]);
    assert_eq!(a["manifest_count"], b["manifest_count"]);

    // A second replicate pass should be a no-op (idempotent).
    let (_s, _h, body) =
        send(&h.app, Method::POST, "/admin/replicate", Some(b"{}".to_vec()), Some("application/json"))
            .await;
    let report: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(report["blobs_copied"], 0, "second pass should copy nothing");
    assert_eq!(report["manifests_copied"], 0);
}

#[tokio::test]
async fn replicate_tag_glob_filters() {
    let h = harness();
    let _ = push_image(&h.app, "app/x", "prod-1", b"prod-layer-bytes").await;
    let _ = push_image(&h.app, "app/x", "dev-1", b"dev-layer-bytes").await;

    let body = serde_json::json!({"tag_glob": "prod-*"}).to_string();
    let (status, _h, body) = send(
        &h.app,
        Method::POST,
        "/admin/replicate",
        Some(body.into_bytes()),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let report: Value = serde_json::from_slice(&body).unwrap();
    // Two blobs (layer + config) and one manifest — only the prod image.
    assert_eq!(report["blobs_copied"], 2, "report: {report}");
    assert_eq!(report["manifests_copied"], 1);

    // Confirm the dev tag is NOT on B, but the prod tag is.
    let b = &h.multi.partitions()[1].storage;
    let tags = b.list_tags("app/x").await.unwrap();
    assert_eq!(tags, vec!["prod-1"]);
}

#[tokio::test]
async fn read_falls_through_to_secondary_via_http() {
    // If A goes empty (simulating a wipe) but B holds the blob from a
    // previous replicate, the registry should still serve it.
    let h = harness();
    let layer = b"shared-via-fallthrough";
    let _ = push_image(&h.app, "app/y", "v1", layer).await;

    // Replicate to B.
    let (_s, _h, _) =
        send(&h.app, Method::POST, "/admin/replicate", Some(b"{}".to_vec()), Some("application/json"))
            .await;

    // Wipe A's blob store by deleting the only blob via the HTTP API,
    // which goes to A first (and B since blob_delete is multi-fan-out
    // by design). To genuinely test fallthrough we have to bypass the
    // public API and reach into the partition directly.
    let layer_digest = format!("sha256:{}", sha256_hex(layer));
    let parsed: rspace_registry_core::Digest = layer_digest.parse().unwrap();
    let a = &h.multi.partitions()[0].storage;
    a.blob_delete(&parsed).await.unwrap();

    // Now GET via the registry should fall through to B and succeed.
    let (status, _h, body) = send(
        &h.app,
        Method::GET,
        &format!("/v2/app/y/blobs/{layer_digest}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fallthrough must serve from B");
    assert_eq!(body, layer);
}

#[tokio::test]
async fn replicate_without_multi_returns_404() {
    // Single-partition AppState — no MultiStore wired in. The
    // /admin/replicate endpoint should not be routable.
    let tmp = tempfile::tempdir().unwrap();
    let storage = Arc::new(FsStorage::new(tmp.path()).unwrap());
    let state = AppState::new(storage);
    let app = build_router(state);
    let (status, _h, _) = send(
        &app,
        Method::POST,
        "/admin/replicate",
        Some(b"{}".to_vec()),
        Some("application/json"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
