//! Manual path-pattern routing for OCI Distribution Spec v1.1.
//!
//! Repo names may contain `/` (e.g. `library/alpine`, `tenant/team/repo`),
//! so we cannot rely on axum's static `:param` extractors. Instead we
//! accept anything under `/v2/` via a fallback and parse the path with a
//! small state machine.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use tower_http::trace::TraceLayer;

use rspace_registry_core::{MultiStore, RepoRouter};

use crate::auth::{self, Htpasswd};
use crate::error::{OciCode, OciError};
use crate::handlers::{self, SharedStorage};
use crate::k8s::{self, K8sAuth};

/// How the registry authenticates requests, when auth is enabled at all.
#[derive(Clone)]
pub enum Auth {
    /// HTTP Basic against an htpasswd file.
    Htpasswd(Arc<Htpasswd>),
    /// Cluster-delegated: TokenReview + SubjectAccessReview (`--auth k8s`).
    K8s(Arc<K8sAuth>),
}

#[derive(Clone)]
pub struct AppState {
    pub storage: SharedStorage,
    /// When `storage` is a `MultiStore`, expose admin endpoints
    /// (`/admin/partitions`, `/admin/replicate`) backed by it.
    pub multi: Option<Arc<MultiStore>>,
    /// When `storage` is a `RepoRouter`, expose admin endpoints
    /// (`GET /admin/repo-roots`, `POST /admin/repo-root`) backed by it.
    pub router: Option<Arc<RepoRouter>>,
    pub auth: Option<Auth>,
    pub realm: String,
    /// When set, exposes `POST /admin/gc` as an authenticated trigger.
    pub admin_enabled: bool,
}

impl AppState {
    pub fn new(storage: SharedStorage) -> Self {
        Self {
            storage,
            multi: None,
            router: None,
            auth: None,
            realm: "rspace-registry".to_string(),
            admin_enabled: true,
        }
    }

    pub fn with_multi(mut self, multi: Arc<MultiStore>) -> Self {
        self.multi = Some(multi);
        self
    }

    pub fn with_router(mut self, router: Arc<RepoRouter>) -> Self {
        self.router = Some(router);
        self
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .fallback(dispatch)
        .layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn require_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let Some(auth) = state.auth.as_ref() else {
        return next.run(req).await;
    };
    // Always allow GET /v2/ (the version check is unauthenticated by spec —
    // some clients use it to probe what auth method to use, then retry).
    if req.method() == Method::GET && (req.uri().path() == "/v2/" || req.uri().path() == "/v2") {
        return next.run(req).await;
    }
    match auth {
        Auth::Htpasswd(htpasswd) => match auth::parse_basic(req.headers()) {
            Some((u, p)) if htpasswd.verify(&u, &p) => next.run(req).await,
            _ => basic_challenge(&state.realm),
        },
        Auth::K8s(k8s_auth) => {
            let op = k8s::classify(req.method(), req.uri().path());
            let peer = req
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip());
            match k8s_auth.check(&op, req.headers(), peer).await {
                Ok(()) => next.run(req).await,
                Err(reject) => reject.into_response(k8s_auth),
            }
        }
    }
}

fn basic_challenge(realm: &str) -> Response {
    let err = OciError::new(OciCode::Unauthorized, "authentication required");
    let mut resp = err.into_response();
    let challenge = auth::challenge_headers(realm);
    for (k, v) in challenge {
        if let Some(name) = k {
            resp.headers_mut().insert(name, v);
        }
    }
    *resp.status_mut() = auth::UNAUTH_STATUS;
    resp
}

async fn dispatch(State(state): State<AppState>, req: Request) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query: HashMap<String, String> = req
        .uri()
        .query()
        .map(|q| {
            url_form_pairs(q)
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let headers = req.headers().clone();
    let body = match read_body(req).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };

    let storage = state.storage.clone();

    // ---------------- /v2/ root -----------------------------------------
    if path == "/v2" || path == "/v2/" {
        return match method {
            Method::GET | Method::HEAD => handlers::version_check().await,
            _ => method_not_allowed(),
        };
    }

    // Strip /v2/ prefix; everything below this is `<repo>/<endpoint>...`.
    let Some(rest) = path.strip_prefix("/v2/") else {
        // ----- Admin (non-OCI) endpoints ----------------------------------
        if state.admin_enabled {
            if path == "/admin/gc" && method == Method::POST {
                return into_response(handlers::gc_run(storage).await);
            }
            if let Some(multi) = state.multi.as_ref() {
                if path == "/admin/partitions" && (method == Method::GET || method == Method::HEAD)
                {
                    return into_response(handlers::partitions_list(multi.clone()).await);
                }
                if path == "/admin/replicate" && method == Method::POST {
                    let req: handlers::ReplicateRequest = if body.is_empty() {
                        Default::default()
                    } else {
                        match serde_json::from_slice(&body) {
                            Ok(r) => r,
                            Err(e) => {
                                return OciError::new(
                                    OciCode::BlobUploadInvalid,
                                    format!("bad replicate body: {e}"),
                                )
                                .with_status(StatusCode::BAD_REQUEST)
                                .into_response();
                            }
                        }
                    };
                    return into_response(handlers::replicate_run(multi.clone(), req).await);
                }
            }
            if let Some(router) = state.router.as_ref() {
                if path == "/admin/repo-roots" && (method == Method::GET || method == Method::HEAD)
                {
                    return into_response(handlers::repo_roots_list(router.clone()).await);
                }
                if path == "/admin/repo-root" && method == Method::POST {
                    let req: handlers::RepoRootRequest = match serde_json::from_slice(&body) {
                        Ok(r) => r,
                        Err(e) => {
                            return OciError::new(
                                OciCode::BlobUploadInvalid,
                                format!("bad repo-root body: {e}"),
                            )
                            .with_status(StatusCode::BAD_REQUEST)
                            .into_response();
                        }
                    };
                    return into_response(handlers::repo_root_upsert(router.clone(), req).await);
                }
                if path == "/admin/repo-migrate" && method == Method::POST {
                    let req: handlers::RepoMigrateRequest = match serde_json::from_slice(&body) {
                        Ok(r) => r,
                        Err(e) => {
                            return OciError::new(
                                OciCode::BlobUploadInvalid,
                                format!("bad repo-migrate body: {e}"),
                            )
                            .with_status(StatusCode::BAD_REQUEST)
                            .into_response();
                        }
                    };
                    return into_response(handlers::repo_migrate(router.clone(), req).await);
                }
            }
        }
        return not_found();
    };

    // ---------------- /v2/_catalog --------------------------------------
    if rest == "_catalog" {
        return match method {
            Method::GET | Method::HEAD => {
                let n = query.get("n").and_then(|s| s.parse().ok());
                let last = query.get("last").cloned();
                into_response(handlers::catalog(storage, n, last).await)
            }
            _ => method_not_allowed(),
        };
    }

    // For the remaining endpoints we recognise suffix patterns; the part
    // before the matching suffix is the repository name.
    if let Some(repo) = strip_suffix(rest, "/tags/list") {
        return match method {
            Method::GET | Method::HEAD => {
                let n = query.get("n").and_then(|s| s.parse().ok());
                let last = query.get("last").cloned();
                into_response(handlers::tags_list(storage, repo, n, last).await)
            }
            _ => method_not_allowed(),
        };
    }

    // /v2/<name>/blobs/uploads        (POST start; spec canonical form has a
    // trailing slash, which podman/docker send — accept both)
    // /v2/<name>/blobs/uploads/<uuid> (GET/PATCH/PUT/DELETE session)
    let upload_start = rest.strip_suffix('/').unwrap_or(rest);
    if let Some(repo) = strip_suffix(upload_start, "/blobs/uploads") {
        return match method {
            Method::POST => {
                let digest = query.get("digest").cloned();
                let mount = query.get("mount").cloned();
                let from = query.get("from").cloned();
                into_response(
                    handlers::upload_start(storage, repo, digest, mount, from, body).await,
                )
            }
            _ => method_not_allowed(),
        };
    }
    if let Some((repo, uuid)) = strip_two_suffix(rest, "/blobs/uploads/") {
        return match method {
            Method::GET => into_response(handlers::upload_status(storage, repo, uuid).await),
            Method::PATCH => into_response(handlers::upload_chunk(storage, repo, uuid, body).await),
            Method::PUT => {
                let digest = query.get("digest").cloned();
                into_response(handlers::upload_finish(storage, repo, uuid, digest, body).await)
            }
            Method::DELETE => into_response(handlers::upload_cancel(storage, repo, uuid).await),
            _ => method_not_allowed(),
        };
    }

    if let Some((repo, reference)) = strip_two_suffix(rest, "/manifests/") {
        return match method {
            Method::GET => {
                into_response(handlers::manifest_get(storage, repo, reference, false).await)
            }
            Method::HEAD => {
                into_response(handlers::manifest_get(storage, repo, reference, true).await)
            }
            Method::PUT => {
                let ct = headers
                    .get(axum::http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                into_response(handlers::manifest_put(storage, repo, reference, ct, body).await)
            }
            Method::DELETE => {
                into_response(handlers::manifest_delete(storage, repo, reference).await)
            }
            _ => method_not_allowed(),
        };
    }

    if let Some((repo, digest)) = strip_two_suffix(rest, "/blobs/") {
        return match method {
            Method::GET => into_response(handlers::blob_get(storage, repo, digest, false).await),
            Method::HEAD => into_response(handlers::blob_get(storage, repo, digest, true).await),
            Method::DELETE => into_response(handlers::blob_delete(storage, repo, digest).await),
            _ => method_not_allowed(),
        };
    }

    if let Some((repo, digest)) = strip_two_suffix(rest, "/referrers/") {
        return match method {
            Method::GET => {
                let at = query.get("artifactType").cloned();
                into_response(handlers::referrers(storage, repo, digest, at).await)
            }
            _ => method_not_allowed(),
        };
    }

    not_found()
}

fn into_response(r: Result<Response, OciError>) -> Response {
    match r {
        Ok(resp) => resp,
        Err(e) => e.into_response(),
    }
}

fn method_not_allowed() -> Response {
    OciError::new(OciCode::Unsupported, "method not allowed")
        .with_status(StatusCode::METHOD_NOT_ALLOWED)
        .into_response()
}

fn not_found() -> Response {
    OciError::new(OciCode::NameUnknown, "not found").into_response()
}

/// Strip exactly `suffix` from `s`, returning the prefix (the repo name)
/// if the suffix matched and the prefix is non-empty.
fn strip_suffix<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    let prefix = s.strip_suffix(suffix)?;
    if prefix.is_empty() {
        None
    } else {
        Some(prefix)
    }
}

/// For patterns like `/manifests/<ref>` where `<ref>` is one final segment.
/// Splits `s` at the LAST occurrence of `marker` and returns
/// `(prefix-before-marker, suffix-after-marker)` if and only if:
///   - the prefix is non-empty (we need a repo name)
///   - the suffix is non-empty and contains no further `/`
fn strip_two_suffix<'a>(s: &'a str, marker: &str) -> Option<(&'a str, &'a str)> {
    let idx = s.rfind(marker)?;
    let prefix = &s[..idx];
    let suffix = &s[idx + marker.len()..];
    if prefix.is_empty() || suffix.is_empty() || suffix.contains('/') {
        None
    } else {
        Some((prefix, suffix))
    }
}

async fn read_body(req: Request) -> Result<Bytes, OciError> {
    let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .map_err(|e| {
            OciError::new(OciCode::BlobUploadInvalid, format!("body read: {e}"))
                .with_status(StatusCode::BAD_REQUEST)
        })?;
    Ok(bytes)
}

/// Minimal `application/x-www-form-urlencoded` parser — no extra crate
/// required. Decodes `+` to space and `%XX` to bytes.
fn url_form_pairs(q: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        out.push((url_decode(k), url_decode(v)));
    }
    out
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_nibble(bytes[i + 1]);
                let lo = hex_nibble(bytes[i + 2]);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h << 4) | l);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[allow(dead_code)]
fn _headers_unused(_: HeaderMap) {}
