//! OCI Distribution Spec v1.1 endpoint handlers.
//!
//! The catchall in `router.rs` parses an incoming `(method, path)` into a
//! single `Op` enum, then dispatches into one function per operation here.
//! This keeps each handler small and the routing decisions in one place.

use std::str::FromStr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use rspace_registry_core::{
    gc, migrate, parse_manifest_refs, replicate, Digest, MultiStore, Reference, ReplicateConfig,
    RepoRouter, Storage, StorageError, MANIFEST_MEDIA_TYPES, OCI_INDEX_MEDIA_TYPE,
    OCI_MANIFEST_MEDIA_TYPE,
};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::error::{OciCode, OciError};

pub type SharedStorage = Arc<dyn Storage>;

// ---------------------------------------------------------------------------
// Repo names
// ---------------------------------------------------------------------------

/// Validate an OCI repository name per the v1.1 spec.
pub fn validate_repo_name(name: &str) -> Result<(), OciError> {
    if name.is_empty() || name.len() > 255 {
        return Err(OciError::new(OciCode::NameInvalid, "name out of range"));
    }
    for component in name.split('/') {
        if component.is_empty() {
            return Err(OciError::new(OciCode::NameInvalid, "empty path component"));
        }
        let mut chars = component.chars();
        let first = chars.next().unwrap();
        if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
            return Err(OciError::new(
                OciCode::NameInvalid,
                "component must start with [a-z0-9]",
            ));
        }
        for c in chars {
            if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-') {
                return Err(OciError::new(
                    OciCode::NameInvalid,
                    "component contains invalid character",
                ));
            }
        }
    }
    Ok(())
}

fn parse_reference(s: &str) -> Reference {
    if let Ok(d) = Digest::from_str(s) {
        Reference::Digest(d)
    } else {
        Reference::Tag(s.to_string())
    }
}

// ---------------------------------------------------------------------------
// Version + base
// ---------------------------------------------------------------------------

pub async fn version_check() -> Response {
    let mut h = HeaderMap::new();
    h.insert(
        HeaderName::from_static("docker-distribution-api-version"),
        HeaderValue::from_static("registry/2.0"),
    );
    (StatusCode::OK, h, "{}").into_response()
}

// ---------------------------------------------------------------------------
// Catalog + tags
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct CatalogBody {
    repositories: Vec<String>,
}

#[derive(Serialize)]
struct TagsBody<'a> {
    name: &'a str,
    tags: Vec<String>,
}

pub async fn catalog(
    storage: SharedStorage,
    n: Option<usize>,
    last: Option<String>,
) -> Result<Response, OciError> {
    let mut repos = storage.list_repos().await?;
    if let Some(after) = last {
        repos.retain(|r| *r > after);
    }
    let truncated = if let Some(limit) = n {
        let was_more = repos.len() > limit;
        repos.truncate(limit);
        was_more
    } else {
        false
    };

    let mut h = HeaderMap::new();
    if truncated {
        if let Some(last) = repos.last() {
            let link = format!(
                "</v2/_catalog?n={}&last={}>; rel=\"next\"",
                n.unwrap_or(0),
                last
            );
            if let Ok(v) = HeaderValue::from_str(&link) {
                h.insert(header::LINK, v);
            }
        }
    }
    Ok((
        StatusCode::OK,
        h,
        axum::Json(CatalogBody {
            repositories: repos,
        }),
    )
        .into_response())
}

pub async fn tags_list(
    storage: SharedStorage,
    repo: &str,
    n: Option<usize>,
    last: Option<String>,
) -> Result<Response, OciError> {
    validate_repo_name(repo)?;
    let mut tags = storage.list_tags(repo).await?;
    if tags.is_empty() {
        // Per spec: a repo with no tags should still 404 the tags-list endpoint.
        let has_repo = storage.list_repos().await?.iter().any(|r| r == repo);
        if !has_repo {
            return Err(OciError::new(OciCode::NameUnknown, "repository not found"));
        }
    }
    if let Some(after) = last {
        tags.retain(|t| *t > after);
    }
    let truncated = if let Some(limit) = n {
        let was_more = tags.len() > limit;
        tags.truncate(limit);
        was_more
    } else {
        false
    };

    let mut h = HeaderMap::new();
    if truncated {
        if let Some(last) = tags.last() {
            let link = format!(
                "</v2/{}/tags/list?n={}&last={}>; rel=\"next\"",
                repo,
                n.unwrap_or(0),
                last
            );
            if let Ok(v) = HeaderValue::from_str(&link) {
                h.insert(header::LINK, v);
            }
        }
    }
    Ok((StatusCode::OK, h, axum::Json(TagsBody { name: repo, tags })).into_response())
}

// ---------------------------------------------------------------------------
// Manifests
// ---------------------------------------------------------------------------

fn manifest_media_type(bytes: &[u8], hinted: Option<&str>) -> String {
    if let Some(h) = hinted {
        if MANIFEST_MEDIA_TYPES.contains(&h) {
            return h.to_string();
        }
    }
    // The manifest's own `mediaType` field is authoritative when present —
    // serving a Docker v2s2 manifest under an OCI Content-Type makes
    // clients reject it as a "mixed OCI image".
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) {
        if let Some(mt) = v.get("mediaType").and_then(|m| m.as_str()) {
            if MANIFEST_MEDIA_TYPES.contains(&mt) {
                return mt.to_string();
            }
        }
    }
    // Sniff: an image index has a `manifests` array; an image manifest has
    // `layers`. We default to OCI image manifest for everything else.
    match parse_manifest_refs(bytes) {
        Ok(m) if !m.manifests.is_empty() => OCI_INDEX_MEDIA_TYPE.to_string(),
        _ => OCI_MANIFEST_MEDIA_TYPE.to_string(),
    }
}

pub async fn manifest_get(
    storage: SharedStorage,
    repo: &str,
    reference: &str,
    head_only: bool,
) -> Result<Response, OciError> {
    validate_repo_name(repo)?;
    let r = parse_reference(reference);
    let bytes = match storage.manifest_get(repo, &r).await {
        Ok(b) => b,
        Err(StorageError::NotFound) => {
            return Err(OciError::new(OciCode::ManifestUnknown, "manifest unknown"))
        }
        Err(e) => return Err(e.into()),
    };

    let digest = Digest {
        algorithm: rspace_registry_core::digest::Algorithm::Sha256,
        hex: hex::encode(Sha256::digest(&bytes)),
    };
    let media = manifest_media_type(&bytes, None);

    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&media).expect("media type is ascii"),
    );
    h.insert(
        HeaderName::from_static("docker-content-digest"),
        HeaderValue::from_str(&digest.to_string()).unwrap(),
    );
    h.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&bytes.len().to_string()).unwrap(),
    );

    if head_only {
        Ok((StatusCode::OK, h).into_response())
    } else {
        Ok((StatusCode::OK, h, bytes).into_response())
    }
}

pub async fn manifest_put(
    storage: SharedStorage,
    repo: &str,
    reference: &str,
    content_type: Option<String>,
    body: Bytes,
) -> Result<Response, OciError> {
    validate_repo_name(repo)?;
    // Reject obviously non-JSON payloads early.
    if serde_json::from_slice::<serde_json::Value>(&body).is_err() {
        return Err(OciError::new(OciCode::ManifestInvalid, "not valid JSON"));
    }
    let r = parse_reference(reference);
    let digest = storage.manifest_put(repo, &r, &body).await?;

    // If the client referenced this manifest by digest, the digest in the
    // URL must match what we computed.
    if let Reference::Digest(want) = &r {
        if want != &digest {
            return Err(OciError::new(
                OciCode::DigestInvalid,
                format!("path digest {want} does not match computed {digest}"),
            ));
        }
    }

    let media = manifest_media_type(&body, content_type.as_deref());

    let mut h = HeaderMap::new();
    let location = format!("/v2/{repo}/manifests/{digest}");
    h.insert(header::LOCATION, HeaderValue::from_str(&location).unwrap());
    h.insert(
        HeaderName::from_static("docker-content-digest"),
        HeaderValue::from_str(&digest.to_string()).unwrap(),
    );
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(&media).unwrap());
    Ok((StatusCode::CREATED, h).into_response())
}

pub async fn manifest_delete(
    storage: SharedStorage,
    repo: &str,
    reference: &str,
) -> Result<Response, OciError> {
    validate_repo_name(repo)?;
    let r = parse_reference(reference);
    match storage.manifest_delete(repo, &r).await {
        Ok(()) => Ok((StatusCode::ACCEPTED, HeaderMap::new()).into_response()),
        Err(StorageError::NotFound) => {
            Err(OciError::new(OciCode::ManifestUnknown, "manifest unknown"))
        }
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// Blobs
// ---------------------------------------------------------------------------

pub async fn blob_get(
    storage: SharedStorage,
    repo: &str,
    digest_str: &str,
    head_only: bool,
) -> Result<Response, OciError> {
    validate_repo_name(repo)?;
    let digest = parse_digest(digest_str)?;
    let bytes = match storage.blob_read(repo, &digest).await {
        Ok(b) => b,
        Err(StorageError::NotFound) => {
            return Err(OciError::new(OciCode::BlobUnknown, "blob unknown"))
        }
        Err(e) => return Err(e.into()),
    };

    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h.insert(
        HeaderName::from_static("docker-content-digest"),
        HeaderValue::from_str(&digest.to_string()).unwrap(),
    );
    h.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&bytes.len().to_string()).unwrap(),
    );

    if head_only {
        Ok((StatusCode::OK, h).into_response())
    } else {
        Ok((StatusCode::OK, h, bytes).into_response())
    }
}

pub async fn blob_delete(
    storage: SharedStorage,
    repo: &str,
    digest_str: &str,
) -> Result<Response, OciError> {
    validate_repo_name(repo)?;
    let digest = parse_digest(digest_str)?;
    match storage.blob_delete(repo, &digest).await {
        Ok(()) => Ok((StatusCode::ACCEPTED, HeaderMap::new()).into_response()),
        Err(StorageError::NotFound) => Err(OciError::new(OciCode::BlobUnknown, "blob unknown")),
        Err(e) => Err(e.into()),
    }
}

fn parse_digest(s: &str) -> Result<Digest, OciError> {
    Digest::from_str(s)
        .map_err(|e| OciError::new(OciCode::DigestInvalid, format!("invalid digest: {e}")))
}

// ---------------------------------------------------------------------------
// Blob uploads (POST init, PATCH chunk, PUT finalise, GET status, DELETE cancel)
// ---------------------------------------------------------------------------

pub async fn upload_start(
    storage: SharedStorage,
    repo: &str,
    digest_q: Option<String>,
    mount_q: Option<String>,
    from_q: Option<String>,
    body: Bytes,
) -> Result<Response, OciError> {
    validate_repo_name(repo)?;

    // Cross-repo mount: make the blob visible at /v2/<target>/blobs/<d>.
    //
    // With per-repo storage routing, source and target may live on
    // different backends. The OCI spec only requires the blob to be
    // visible after the mount; if source and target route to the same
    // backend, that's already true. If they route to different backends,
    // we copy the bytes from source to target.
    if let Some(digest_s) = mount_q {
        if let Ok(digest) = Digest::from_str(&digest_s) {
            if storage.blob_exists(repo, &digest).await? {
                return Ok(mount_response(repo, &digest));
            }
            if let Some(from) = from_q.as_deref() {
                if validate_repo_name(from).is_ok() {
                    if let Ok(bytes) = storage.blob_read(from, &digest).await {
                        storage.blob_write(repo, &digest, &bytes).await?;
                        return Ok(mount_response(repo, &digest));
                    }
                }
            }
        }
        // Source doesn't exist anywhere reachable → fall through to a
        // normal upload session.
    }

    // Monolithic upload: POST + ?digest=<d> with body.
    if let Some(digest_s) = digest_q {
        let digest = parse_digest(&digest_s)?;
        storage.blob_write(repo, &digest, &body).await?;
        let mut h = HeaderMap::new();
        let loc = format!("/v2/{repo}/blobs/{digest}");
        h.insert(header::LOCATION, HeaderValue::from_str(&loc).unwrap());
        h.insert(
            HeaderName::from_static("docker-content-digest"),
            HeaderValue::from_str(&digest.to_string()).unwrap(),
        );
        return Ok((StatusCode::CREATED, h).into_response());
    }

    // Normal: start a new upload session.
    let session = storage.upload_create(repo).await?;
    let mut h = HeaderMap::new();
    let loc = format!("/v2/{repo}/blobs/uploads/{}", session.id);
    h.insert(header::LOCATION, HeaderValue::from_str(&loc).unwrap());
    h.insert(
        HeaderName::from_static("docker-upload-uuid"),
        HeaderValue::from_str(&session.id.to_string()).unwrap(),
    );
    h.insert(header::RANGE, HeaderValue::from_str("0-0").unwrap());
    Ok((StatusCode::ACCEPTED, h).into_response())
}

fn parse_uuid(s: &str) -> Result<Uuid, OciError> {
    Uuid::parse_str(s).map_err(|_| OciError::new(OciCode::BlobUploadInvalid, "invalid upload uuid"))
}

fn mount_response(repo: &str, digest: &Digest) -> Response {
    let mut h = HeaderMap::new();
    let loc = format!("/v2/{repo}/blobs/{digest}");
    h.insert(header::LOCATION, HeaderValue::from_str(&loc).unwrap());
    h.insert(
        HeaderName::from_static("docker-content-digest"),
        HeaderValue::from_str(&digest.to_string()).unwrap(),
    );
    (StatusCode::CREATED, h).into_response()
}

pub async fn upload_status(
    storage: SharedStorage,
    repo: &str,
    uuid: &str,
) -> Result<Response, OciError> {
    validate_repo_name(repo)?;
    let id = parse_uuid(uuid)?;
    let status = match storage.upload_status(repo, id).await {
        Ok(s) => s,
        Err(StorageError::NotFound) => {
            return Err(OciError::new(OciCode::BlobUploadUnknown, "no such upload"))
        }
        Err(e) => return Err(e.into()),
    };
    let mut h = HeaderMap::new();
    let loc = format!("/v2/{repo}/blobs/uploads/{id}");
    h.insert(header::LOCATION, HeaderValue::from_str(&loc).unwrap());
    h.insert(
        HeaderName::from_static("docker-upload-uuid"),
        HeaderValue::from_str(&id.to_string()).unwrap(),
    );
    let range = if status.offset == 0 {
        "0-0".to_string()
    } else {
        format!("0-{}", status.offset.saturating_sub(1))
    };
    h.insert(header::RANGE, HeaderValue::from_str(&range).unwrap());
    Ok((StatusCode::NO_CONTENT, h).into_response())
}

pub async fn upload_chunk(
    storage: SharedStorage,
    repo: &str,
    uuid: &str,
    body: Bytes,
) -> Result<Response, OciError> {
    validate_repo_name(repo)?;
    let id = parse_uuid(uuid)?;
    let status = match storage.upload_append(repo, id, &body).await {
        Ok(s) => s,
        Err(StorageError::NotFound) => {
            return Err(OciError::new(OciCode::BlobUploadUnknown, "no such upload"))
        }
        Err(e) => return Err(e.into()),
    };

    let mut h = HeaderMap::new();
    let loc = format!("/v2/{repo}/blobs/uploads/{id}");
    h.insert(header::LOCATION, HeaderValue::from_str(&loc).unwrap());
    h.insert(
        HeaderName::from_static("docker-upload-uuid"),
        HeaderValue::from_str(&id.to_string()).unwrap(),
    );
    let range = format!("0-{}", status.offset.saturating_sub(1));
    h.insert(header::RANGE, HeaderValue::from_str(&range).unwrap());
    Ok((StatusCode::ACCEPTED, h).into_response())
}

pub async fn upload_finish(
    storage: SharedStorage,
    repo: &str,
    uuid: &str,
    digest_q: Option<String>,
    body: Bytes,
) -> Result<Response, OciError> {
    validate_repo_name(repo)?;
    let id = parse_uuid(uuid)?;
    let digest_s = digest_q
        .ok_or_else(|| OciError::new(OciCode::DigestInvalid, "missing ?digest= on finalise"))?;
    let digest = parse_digest(&digest_s)?;
    if !body.is_empty() {
        match storage.upload_append(repo, id, &body).await {
            Ok(_) => {}
            Err(StorageError::NotFound) => {
                return Err(OciError::new(OciCode::BlobUploadUnknown, "no such upload"))
            }
            Err(e) => return Err(e.into()),
        }
    }
    match storage.upload_finalize(repo, id, &digest).await {
        Ok(()) => {}
        Err(StorageError::NotFound) => {
            return Err(OciError::new(OciCode::BlobUploadUnknown, "no such upload"))
        }
        Err(StorageError::DigestMismatch { expected, got }) => {
            return Err(OciError::new(
                OciCode::DigestInvalid,
                format!("digest mismatch: expected {expected}, got {got}"),
            ))
        }
        Err(e) => return Err(e.into()),
    }

    let mut h = HeaderMap::new();
    let loc = format!("/v2/{repo}/blobs/{digest}");
    h.insert(header::LOCATION, HeaderValue::from_str(&loc).unwrap());
    h.insert(
        HeaderName::from_static("docker-content-digest"),
        HeaderValue::from_str(&digest.to_string()).unwrap(),
    );
    Ok((StatusCode::CREATED, h).into_response())
}

pub async fn upload_cancel(
    storage: SharedStorage,
    repo: &str,
    uuid: &str,
) -> Result<Response, OciError> {
    validate_repo_name(repo)?;
    let id = parse_uuid(uuid)?;
    match storage.upload_cancel(repo, id).await {
        Ok(()) => Ok((StatusCode::NO_CONTENT, HeaderMap::new()).into_response()),
        Err(StorageError::NotFound) => {
            Err(OciError::new(OciCode::BlobUploadUnknown, "no such upload"))
        }
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// Referrers (OCI v1.1)
// ---------------------------------------------------------------------------

pub async fn referrers(
    storage: SharedStorage,
    repo: &str,
    digest_str: &str,
    artifact_type_filter: Option<String>,
) -> Result<Response, OciError> {
    validate_repo_name(repo)?;
    let subject = parse_digest(digest_str)?;

    let mut matched = Vec::new();
    let mut filtered = false;
    for d in storage.list_manifest_digests(repo).await? {
        let bytes = match storage
            .manifest_get(repo, &Reference::Digest(d.clone()))
            .await
        {
            Ok(b) => b,
            Err(StorageError::NotFound) => continue,
            Err(e) => return Err(e.into()),
        };
        let Ok(m) = parse_manifest_refs(&bytes) else {
            continue;
        };
        let Some(sub) = m.subject.as_ref() else {
            continue;
        };
        if sub.digest != subject {
            continue;
        }
        if let Some(filter) = artifact_type_filter.as_deref() {
            if m.artifact_type.as_deref() != Some(filter) {
                filtered = true;
                continue;
            }
        }

        // Build the descriptor entry for the index.
        let media = manifest_media_type(&bytes, None);
        let mut entry = serde_json::Map::new();
        entry.insert("mediaType".into(), json!(media));
        entry.insert("digest".into(), json!(d.to_string()));
        entry.insert("size".into(), json!(bytes.len()));
        if let Some(at) = &m.artifact_type {
            entry.insert("artifactType".into(), json!(at));
        }
        if let Some(annot) = &m.annotations {
            entry.insert("annotations".into(), json!(annot));
        }
        matched.push(serde_json::Value::Object(entry));
    }

    let body = json!({
        "schemaVersion": 2,
        "mediaType": OCI_INDEX_MEDIA_TYPE,
        "manifests": matched,
    });

    let mut h = HeaderMap::new();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(OCI_INDEX_MEDIA_TYPE),
    );
    if filtered {
        h.insert(
            HeaderName::from_static("oci-filters-applied"),
            HeaderValue::from_static("artifactType"),
        );
    }
    Ok((StatusCode::OK, h, axum::Json(body)).into_response())
}

// ---------------------------------------------------------------------------
// GC trigger (admin-only; not part of the OCI spec but useful)
// ---------------------------------------------------------------------------

pub async fn gc_run(storage: SharedStorage) -> Result<Response, OciError> {
    let report = gc::run(&*storage).await?;
    Ok(axum::Json(json!({
        "repos_scanned": report.repos_scanned,
        "manifests_scanned": report.manifests_scanned,
        "reachable_blobs": report.reachable_blobs,
        "deleted_blobs": report.deleted_blobs,
        "deleted_bytes": report.deleted_bytes,
    }))
    .into_response())
}

// ---------------------------------------------------------------------------
// Multi-partition admin (only meaningful when MultiStore is in use)
// ---------------------------------------------------------------------------

/// Snapshot of one partition. Counts are point-in-time and may differ
/// between calls if the registry is taking writes concurrently.
pub async fn partitions_list(multi: Arc<MultiStore>) -> Result<Response, OciError> {
    let mut out = Vec::with_capacity(multi.partitions().len());
    for (i, p) in multi.partitions().iter().enumerate() {
        let blobs = p.storage.list_all_blobs().await?;
        let repos = p.storage.list_repos().await?;
        let mut manifest_count = 0usize;
        for repo in &repos {
            manifest_count += p.storage.list_manifest_digests(repo).await?.len();
        }
        let is_primary = std::ptr::eq(p, multi.primary());
        let _ = i;
        out.push(json!({
            "name": p.name,
            "primary": is_primary,
            "blob_count": blobs.len(),
            "manifest_count": manifest_count,
            "repo_count": repos.len(),
        }));
    }
    Ok(axum::Json(json!({ "partitions": out })).into_response())
}

#[derive(serde::Deserialize, Default)]
pub struct ReplicateRequest {
    /// Optional shell-style glob — `"prod-*"`, `"v?.0"`, `"*"`.
    pub tag_glob: Option<String>,
}

pub async fn replicate_run(
    multi: Arc<MultiStore>,
    body: ReplicateRequest,
) -> Result<Response, OciError> {
    let cfg = ReplicateConfig {
        tag_glob: body.tag_glob,
    };
    let report = replicate::run(&multi, &cfg).await?;
    Ok(axum::Json(json!({
        "partitions_scanned": report.partitions_scanned,
        "blobs_copied": report.blobs_copied,
        "bytes_copied": report.bytes_copied,
        "manifests_copied": report.manifests_copied,
        "duration_ms": report.duration_ms,
    }))
    .into_response())
}

// ---------------------------------------------------------------------------
// Repo-router admin (only meaningful when RepoRouter is in use)
// ---------------------------------------------------------------------------

/// `GET /admin/repo-roots` — list the current routing ruleset.
pub async fn repo_roots_list(router: Arc<RepoRouter>) -> Result<Response, OciError> {
    let rules = router.rules();
    let out: Vec<_> = rules
        .iter()
        .map(|r| {
            json!({
                "pattern": r.pattern,
                // We can't usefully show the backend's path without
                // leaking implementation detail (it's behind a trait
                // object). For now just confirm it's bound.
                "bound": true,
            })
        })
        .collect();
    Ok(axum::Json(json!({ "rules": out })).into_response())
}

/// `POST /admin/repo-root` body: `{ "pattern": "...", "root": "/path" }`.
///
/// If a rule with `pattern` exists, its backend is repointed to the new
/// root; otherwise the rule is appended. This is what `rspacefs-pvc`
/// fires after a local mount pivot so the registry follows along without
/// a restart.
#[derive(serde::Deserialize)]
pub struct RepoRootRequest {
    pub pattern: String,
    pub root: String,
}

pub async fn repo_root_upsert(
    router: Arc<RepoRouter>,
    body: RepoRootRequest,
) -> Result<Response, OciError> {
    if body.pattern.is_empty() {
        return Err(
            OciError::new(OciCode::BlobUploadInvalid, "pattern must be non-empty")
                .with_status(StatusCode::BAD_REQUEST),
        );
    }
    let storage = rspace_registry_fs::FsStorage::new(&body.root).map_err(|e| {
        OciError::new(
            OciCode::Unsupported,
            format!("opening root {}: {e}", body.root),
        )
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
    })?;
    let backend = Arc::new(storage) as Arc<dyn Storage>;
    let added = router.upsert(body.pattern.clone(), backend);
    Ok(axum::Json(json!({
        "pattern": body.pattern,
        "root": body.root,
        "added": added,
        "rule_count": router.rules().len(),
    }))
    .into_response())
}

/// `POST /admin/repo-migrate` body:
/// `{ "pattern": "data/*" | "class": "data", "to": "/mnt/bulk2",
///    "drain": false, "async": false }`.
///
/// Zero-miss live migration: overlay-cutover the rule (new primary + old
/// fallback), backfill old → new, then collapse onto the new volume. With
/// `drain: true`, the old volume's content is deleted and GC'd afterwards
/// so its capacity is reclaimed. With `async: true`, the migration runs in
/// the background and the response returns a job id to poll at
/// `GET /admin/jobs/<id>`.
#[derive(serde::Deserialize)]
pub struct RepoMigrateRequest {
    /// Exact route pattern to migrate (e.g. `data/*`). Mutually exclusive
    /// with `class`.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Named class to migrate; expands to the `<class>/*` pattern.
    #[serde(default)]
    pub class: Option<String>,
    pub to: String,
    #[serde(default)]
    pub drain: bool,
    #[serde(default, rename = "async")]
    pub background: bool,
}

fn migrate_report_json(
    pattern: &str,
    to: &str,
    drain: bool,
    report: &rspace_registry_core::MigrateReport,
) -> serde_json::Value {
    json!({
        "pattern": pattern,
        "to": to,
        "drain": drain,
        "cutover": report.cutover,
        "repos_migrated": report.repos_migrated,
        "blobs_copied": report.blobs_copied,
        "bytes_copied": report.bytes_copied,
        "manifests_copied": report.manifests_copied,
        "blobs_purged": report.blobs_purged,
        "bytes_purged": report.bytes_purged,
        "duration_ms": report.duration_ms,
    })
}

pub async fn repo_migrate(
    router: Arc<RepoRouter>,
    jobs: crate::jobs::Jobs,
    body: RepoMigrateRequest,
) -> Result<Response, OciError> {
    let pattern = match (body.pattern.as_deref(), body.class.as_deref()) {
        (Some(p), _) if !p.is_empty() => p.to_string(),
        (_, Some(c)) if !c.is_empty() => format!("{c}/*"),
        _ => {
            return Err(OciError::new(
                OciCode::BlobUploadInvalid,
                "one of non-empty `pattern` or `class` is required",
            )
            .with_status(StatusCode::BAD_REQUEST))
        }
    };
    // Build the destination backend synchronously so a bad path fails the
    // request up front rather than a background job.
    let storage = rspace_registry_fs::FsStorage::new(&body.to).map_err(|e| {
        OciError::new(
            OciCode::Unsupported,
            format!("opening root {}: {e}", body.to),
        )
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
    })?;
    let new_backend = Arc::new(storage) as Arc<dyn Storage>;

    if body.background {
        let params = json!({ "pattern": pattern, "to": body.to, "drain": body.drain });
        let id = jobs.start("repo-migrate", params);
        let (jobs2, router2, pattern2, to2, drain) = (
            jobs.clone(),
            router.clone(),
            pattern.clone(),
            body.to.clone(),
            body.drain,
        );
        let job_id = id.clone();
        tokio::spawn(async move {
            match migrate::run(&router2, &pattern2, new_backend, drain).await {
                Ok(report) => jobs2.finish(
                    &job_id,
                    migrate_report_json(&pattern2, &to2, drain, &report),
                ),
                Err(e) => jobs2.fail(&job_id, e.to_string()),
            }
        });
        return Ok((
            StatusCode::ACCEPTED,
            axum::Json(json!({ "job_id": id, "state": "running", "pattern": pattern })),
        )
            .into_response());
    }

    let report = migrate::run(&router, &pattern, new_backend, body.drain).await?;
    Ok(axum::Json(migrate_report_json(&pattern, &body.to, body.drain, &report)).into_response())
}

// ---------------------------------------------------------------------------
// Async job status
// ---------------------------------------------------------------------------

/// `GET /admin/jobs` — list all background admin jobs.
pub async fn jobs_list(jobs: crate::jobs::Jobs) -> Result<Response, OciError> {
    Ok(axum::Json(json!({ "jobs": jobs.list() })).into_response())
}

/// `GET /admin/jobs/<id>` — one job, 404 if unknown.
pub async fn job_get(jobs: crate::jobs::Jobs, id: &str) -> Result<Response, OciError> {
    match jobs.get(id) {
        Some(rec) => Ok(axum::Json(rec).into_response()),
        None => Err(OciError::new(OciCode::NameUnknown, "job not found")),
    }
}

// ---------------------------------------------------------------------------
// Named repo classes
// ---------------------------------------------------------------------------

/// `GET /admin/repo-classes` — list declared classes and their volumes.
pub async fn repo_classes_list(
    classes: Vec<crate::router::RepoClass>,
) -> Result<Response, OciError> {
    Ok(axum::Json(json!({ "classes": classes })).into_response())
}

// ---------------------------------------------------------------------------
// Storage quotas
// ---------------------------------------------------------------------------

/// `GET /admin/quotas` — per-class limit + current usage.
pub async fn quotas_list(
    quota: Arc<rspace_registry_core::QuotaStorage>,
) -> Result<Response, OciError> {
    let report = quota.report().await;
    let out: Vec<_> = report
        .iter()
        .map(|q| {
            let pct = q
                .used_bytes
                .filter(|_| q.max_bytes > 0)
                .map(|u| (u as f64 / q.max_bytes as f64 * 100.0).round() as u64);
            json!({
                "pattern": q.pattern,
                "max_bytes": q.max_bytes,
                "used_bytes": q.used_bytes,
                "used_pct": pct,
            })
        })
        .collect();
    Ok(axum::Json(json!({ "quotas": out })).into_response())
}
