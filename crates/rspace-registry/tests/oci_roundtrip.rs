//! End-to-end OCI Distribution Spec v1.1 round-trip against the in-process
//! HTTP service. Exercises every endpoint the acceptance criteria require:
//! upload a blob in chunks, push a manifest that references it, list tags,
//! list catalog, push a referrer with `subject`, query referrers, GC, then
//! delete + verify gone.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, StatusCode};
use http_body_util::BodyExt;
use rspace_registry::{build_router, AppState};
use rspace_registry_fs::FsStorage;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

fn router() -> (axum::Router, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let storage = Arc::new(FsStorage::new(tmp.path()).expect("fs storage"));
    let state = AppState::new(storage);
    (build_router(state), tmp)
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

#[tokio::test]
async fn version_check() {
    let (app, _tmp) = router();
    let (status, headers, body) = send(&app, Method::GET, "/v2/", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"{}");
    assert_eq!(
        headers
            .get("docker-distribution-api-version")
            .unwrap()
            .to_str()
            .unwrap(),
        "registry/2.0"
    );
}

#[tokio::test]
async fn full_push_pull_roundtrip() {
    let (app, _tmp) = router();
    let repo = "library/alpine";
    let layer = b"layer-data-hello-world";
    let layer_digest = format!("sha256:{}", sha256_hex(layer));
    let config =
        br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#;
    let config_digest = format!("sha256:{}", sha256_hex(config));

    // --- Upload the layer in chunks. ----------------------------------------
    let (status, headers, _) = send(
        &app,
        Method::POST,
        &format!("/v2/{repo}/blobs/uploads"),
        Some(vec![]),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // PATCH the first half.
    let mid = layer.len() / 2;
    let (status, _h, _) = send(
        &app,
        Method::PATCH,
        &location,
        Some(layer[..mid].to_vec()),
        Some("application/octet-stream"),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // PUT?digest= with the second half.
    let put_url = format!("{location}?digest={layer_digest}");
    let (status, headers, _) = send(
        &app,
        Method::PUT,
        &put_url,
        Some(layer[mid..].to_vec()),
        Some("application/octet-stream"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "finalise should 201");
    assert_eq!(
        headers
            .get("docker-content-digest")
            .unwrap()
            .to_str()
            .unwrap(),
        layer_digest
    );

    // --- Upload the config blob monolithically. ----------------------------
    let (status, _h, _) = send(
        &app,
        Method::POST,
        &format!("/v2/{repo}/blobs/uploads?digest={config_digest}"),
        Some(config.to_vec()),
        Some("application/octet-stream"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // --- HEAD both blobs. ---------------------------------------------------
    for d in [&layer_digest, &config_digest] {
        let (status, headers, _) = send(
            &app,
            Method::HEAD,
            &format!("/v2/{repo}/blobs/{d}"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "HEAD blob {d}");
        assert!(headers.contains_key("docker-content-digest"));
    }

    // --- PUT a manifest referencing them. -----------------------------------
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
    let manifest_digest = format!("sha256:{}", sha256_hex(&m_bytes));

    let (status, headers, _) = send(
        &app,
        Method::PUT,
        &format!("/v2/{repo}/manifests/v1"),
        Some(m_bytes.clone()),
        Some("application/vnd.oci.image.manifest.v1+json"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        headers
            .get("docker-content-digest")
            .unwrap()
            .to_str()
            .unwrap(),
        manifest_digest,
        "server-side digest must match client-computed digest"
    );

    // --- GET it back by tag and by digest. ----------------------------------
    let (status, _h, body) = send(
        &app,
        Method::GET,
        &format!("/v2/{repo}/manifests/v1"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, m_bytes);

    let (status, _h, body) = send(
        &app,
        Method::GET,
        &format!("/v2/{repo}/manifests/{manifest_digest}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, m_bytes);

    // --- Tags list contains "v1". -------------------------------------------
    let (status, _h, body) = send(
        &app,
        Method::GET,
        &format!("/v2/{repo}/tags/list"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parsed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["name"], repo);
    assert_eq!(parsed["tags"], serde_json::json!(["v1"]));

    // --- Catalog contains the repo. -----------------------------------------
    let (status, _h, body) = send(&app, Method::GET, "/v2/_catalog", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let parsed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["repositories"], serde_json::json!([repo]));

    // --- Referrers: push a signature manifest with subject=manifest_digest. -
    let referrer = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "artifactType": "application/vnd.example.signature.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.empty.v1+json",
            "digest": config_digest,
            "size": config.len(),
        },
        "layers": [{
            "mediaType": "application/vnd.example.signature.v1+json",
            "digest": layer_digest,
            "size": layer.len(),
        }],
        "subject": {
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": manifest_digest,
            "size": m_bytes.len(),
        },
    });
    let ref_bytes = serde_json::to_vec(&referrer).unwrap();
    let ref_digest = format!("sha256:{}", sha256_hex(&ref_bytes));
    let (status, _h, _) = send(
        &app,
        Method::PUT,
        &format!("/v2/{repo}/manifests/{ref_digest}"),
        Some(ref_bytes.clone()),
        Some("application/vnd.oci.image.manifest.v1+json"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _h, body) = send(
        &app,
        Method::GET,
        &format!("/v2/{repo}/referrers/{manifest_digest}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parsed: Value = serde_json::from_slice(&body).unwrap();
    let manifests = parsed["manifests"].as_array().expect("manifests array");
    assert_eq!(manifests.len(), 1);
    assert_eq!(manifests[0]["digest"], ref_digest);
    assert_eq!(
        manifests[0]["artifactType"],
        "application/vnd.example.signature.v1+json"
    );

    // Referrers with artifactType filter that matches → 1 entry, with
    // OCI-Filters-Applied unset.
    let (status, headers, body) = send(
        &app,
        Method::GET,
        &format!(
            "/v2/{repo}/referrers/{manifest_digest}?artifactType=application/vnd.example.signature.v1%2Bjson"
        ),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parsed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["manifests"].as_array().unwrap().len(), 1);
    assert!(headers.get("oci-filters-applied").is_none());

    // Filter that does NOT match → 0 entries, OCI-Filters-Applied=artifactType.
    let (status, headers, body) = send(
        &app,
        Method::GET,
        &format!(
            "/v2/{repo}/referrers/{manifest_digest}?artifactType=application/vnd.nope.v1%2Bjson"
        ),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let parsed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["manifests"].as_array().unwrap().len(), 0);
    assert_eq!(
        headers
            .get("oci-filters-applied")
            .unwrap()
            .to_str()
            .unwrap(),
        "artifactType"
    );

    // --- GC: nothing should be deleted (everything is reachable). ----------
    let (status, _h, body) = send(&app, Method::POST, "/admin/gc", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let gc: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(gc["deleted_blobs"], 0);
    assert!(gc["reachable_blobs"].as_u64().unwrap() >= 2);

    // --- Now delete the tag manifest + referrer; GC should sweep the blobs. -
    let (status, _h, _) = send(
        &app,
        Method::DELETE,
        &format!("/v2/{repo}/manifests/v1"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _h, _) = send(
        &app,
        Method::DELETE,
        &format!("/v2/{repo}/manifests/{manifest_digest}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let (status, _h, _) = send(
        &app,
        Method::DELETE,
        &format!("/v2/{repo}/manifests/{ref_digest}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, _h, body) = send(&app, Method::POST, "/admin/gc", None, None).await;
    assert_eq!(status, StatusCode::OK);
    let gc: Value = serde_json::from_slice(&body).unwrap();
    assert!(gc["deleted_blobs"].as_u64().unwrap() >= 2);
}

#[tokio::test]
async fn missing_blob_returns_404_oci_error() {
    let (app, _tmp) = router();
    let (status, _h, body) = send(
        &app,
        Method::GET,
        "/v2/x/blobs/sha256:0000000000000000000000000000000000000000000000000000000000000000",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let parsed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["errors"][0]["code"], "BLOB_UNKNOWN");
}

#[tokio::test]
async fn rejects_bad_repo_name() {
    let (app, _tmp) = router();
    let (status, _h, body) = send(&app, Method::GET, "/v2/UPPERCASE/tags/list", None, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let parsed: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["errors"][0]["code"], "NAME_INVALID");
}

#[tokio::test]
async fn cross_repo_mount_returns_201_immediately() {
    let (app, _tmp) = router();
    let blob = b"shared-blob";
    let digest = format!("sha256:{}", sha256_hex(blob));

    // Push blob to repo A.
    let (status, _h, _) = send(
        &app,
        Method::POST,
        &format!("/v2/a/blobs/uploads?digest={digest}"),
        Some(blob.to_vec()),
        Some("application/octet-stream"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Mount into repo B without re-uploading.
    let (status, headers, _) = send(
        &app,
        Method::POST,
        &format!("/v2/b/blobs/uploads?mount={digest}&from=a"),
        Some(vec![]),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "mount should 201");
    assert_eq!(
        headers
            .get("docker-content-digest")
            .unwrap()
            .to_str()
            .unwrap(),
        digest
    );
}
