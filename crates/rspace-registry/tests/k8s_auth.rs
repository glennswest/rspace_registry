//! HTTP-level tests for `--auth k8s`: the router wired with a `K8sAuth` over
//! a fake `Reviewer`, driven through the real middleware. Covers the bearer
//! challenge, TokenReview authn, SubjectAccessReview authz per verb, and the
//! loopback boot-order fast path.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderValue, Method, Request, StatusCode};
use rspace_registry::k8s::{
    AccessReview, K8sAuth, K8sAuthConfig, ReviewError, Reviewer, TokenVerdict,
};
use rspace_registry::{build_router, AppState, Auth};
use rspace_registry_fs::FsStorage;
use tower::ServiceExt;

/// Fixed-token / allow-list fake standing in for the API server.
struct FakeReviewer {
    tokens: HashMap<String, TokenVerdict>,
    allowed: Vec<(String, String, String)>, // (user, namespace, verb)
}

#[async_trait]
impl Reviewer for FakeReviewer {
    async fn review_token(&self, token: &str) -> Result<TokenVerdict, ReviewError> {
        Ok(self.tokens.get(token).cloned().unwrap_or(TokenVerdict {
            authenticated: false,
            error: Some("unknown token".into()),
            ..Default::default()
        }))
    }
    async fn review_access(&self, req: &AccessReview) -> Result<bool, ReviewError> {
        Ok(self
            .allowed
            .iter()
            .any(|(u, ns, v)| u == &req.user && ns == &req.namespace && v == &req.verb))
    }
}

fn router(allow_loopback: bool) -> (axum::Router, tempfile::TempDir) {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let storage = Arc::new(FsStorage::new(tmp.path()).expect("fs storage"));

    let mut tokens = HashMap::new();
    tokens.insert(
        "alice-token".to_string(),
        TokenVerdict {
            authenticated: true,
            username: "alice".into(),
            uid: "u1".into(),
            groups: vec!["system:authenticated".into()],
            error: None,
        },
    );
    let reviewer = FakeReviewer {
        tokens,
        // alice may pull and push in team-a, but not delete.
        allowed: vec![
            ("alice".into(), "team-a".into(), "get".into()),
            ("alice".into(), "team-a".into(), "update".into()),
        ],
    };
    let cfg = K8sAuthConfig {
        resource_group: "rspace.io".into(),
        resource: "repositories".into(),
        default_namespace: None,
        cache_ttl: Duration::from_secs(120),
        allow_loopback,
        token_realm: "rspace-registry/token".into(),
        service: "rspace-registry".into(),
    };
    let k8s = Arc::new(K8sAuth::new(cfg, Box::new(reviewer)));

    let mut state = AppState::new(storage);
    state.auth = Some(Auth::K8s(k8s));
    (build_router(state), tmp)
}

async fn send(
    app: &axum::Router,
    method: Method,
    path: &str,
    auth: Option<&str>,
    peer: Option<SocketAddr>,
) -> (StatusCode, axum::http::HeaderMap) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(a) = auth {
        builder = builder.header("authorization", HeaderValue::from_str(a).unwrap());
    }
    let mut req = builder.body(Body::empty()).unwrap();
    if let Some(p) = peer {
        req.extensions_mut().insert(ConnectInfo(p));
    }
    let resp = app.clone().oneshot(req).await.expect("request");
    (resp.status(), resp.headers().clone())
}

#[tokio::test]
async fn version_check_is_unauthenticated() {
    let (app, _t) = router(false);
    let (status, _h) = send(&app, Method::GET, "/v2/", None, None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn no_token_gets_bearer_challenge() {
    let (app, _t) = router(false);
    let (status, headers) = send(&app, Method::GET, "/v2/team-a/nginx/tags/list", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let challenge = headers.get("www-authenticate").unwrap().to_str().unwrap();
    assert!(challenge.starts_with("Bearer "), "got: {challenge}");
    assert!(challenge.contains("realm=\"rspace-registry/token\""));
    assert!(challenge.contains("service=\"rspace-registry\""));
}

#[tokio::test]
async fn bad_token_is_unauthorized() {
    let (app, _t) = router(false);
    let (status, _h) = send(
        &app,
        Method::GET,
        "/v2/team-a/nginx/tags/list",
        Some("Bearer wrong"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn permitted_pull_reaches_handler() {
    let (app, _t) = router(false);
    // A manifest GET is verb `get`, which alice holds in team-a. 404 (manifest
    // unknown) proves we got *past* auth into the handler — not 401/403.
    let (status, _h) = send(
        &app,
        Method::GET,
        "/v2/team-a/nginx/manifests/latest",
        Some("Bearer alice-token"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn pull_via_basic_password_token_works() {
    let (app, _t) = router(false);
    use base64::Engine;
    let creds = base64::engine::general_purpose::STANDARD.encode("anyuser:alice-token");
    let (status, _h) = send(
        &app,
        Method::GET,
        "/v2/team-a/nginx/manifests/latest",
        Some(&format!("Basic {creds}")),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_denied_for_user_without_verb() {
    let (app, _t) = router(false);
    // alice has get+update in team-a but NOT delete → 403.
    let (status, _h) = send(
        &app,
        Method::DELETE,
        "/v2/team-a/nginx/manifests/sha256:0000000000000000000000000000000000000000000000000000000000000000",
        Some("Bearer alice-token"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn pull_from_unpermitted_namespace_is_forbidden() {
    let (app, _t) = router(false);
    // alice has no rights in team-b.
    let (status, _h) = send(
        &app,
        Method::GET,
        "/v2/team-b/nginx/tags/list",
        Some("Bearer alice-token"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn loopback_skips_auth_when_enabled() {
    let (app, _t) = router(true);
    let peer: SocketAddr = "127.0.0.1:54321".parse().unwrap();
    // No credentials at all, yet the request reaches the handler (404).
    let (status, _h) = send(
        &app,
        Method::GET,
        "/v2/team-a/nginx/tags/list",
        None,
        Some(peer),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn non_loopback_peer_still_requires_auth() {
    let (app, _t) = router(true);
    let peer: SocketAddr = "10.0.0.5:54321".parse().unwrap();
    let (status, _h) = send(
        &app,
        Method::GET,
        "/v2/team-a/nginx/tags/list",
        None,
        Some(peer),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
