//! Cluster-delegated auth (`--auth k8s`).
//!
//! The registry holds **no credentials of its own**. It answers the Docker
//! distribution bearer-token challenge, validates the presented Kubernetes
//! token against the API server via **TokenReview**, and authorizes each
//! operation with a **SubjectAccessReview** in the namespace that matches the
//! repository path (`<namespace>/<name>[/...]`). "Who can pull from `x/*`" is a
//! RoleBinding, not registry config — mirroring the built-in OpenShift
//! registry.
//!
//! ## Boot-order constraint
//!
//! On stormcos nodes the registry serves preloaded images to CRI-O on loopback
//! **before** the API server exists. `--auth-allow-loopback` makes requests
//! from `127.0.0.1`/`::1` skip auth entirely so the node can bootstrap with
//! zero cluster dependency; everything else still goes through TokenReview/SAR.
//!
//! ## Token-exchange endpoint
//!
//! The `Bearer` challenge points at a `realm` URL — this registry's own
//! `GET /token` ([`token_endpoint`]). A client that follows the flow
//! (rather than presenting the token directly) authenticates there with the
//! k8s token as the Basic password and receives it back as the bearer token
//! to use against `/v2/`. We do not mint a scoped token of our own — the
//! k8s token is the identity and SAR enforces authorization per request.
//!
//! ## Testability
//!
//! All API-server interaction sits behind the [`Reviewer`] trait. The real
//! [`ApiReviewer`] talks to the API server over HTTPS; tests inject a fake.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::http::{header, HeaderMap, Method, StatusCode};

use crate::error::{OciCode, OciError};

// ---------------------------------------------------------------------------
// Reviewer trait — the seam over the Kubernetes API server.
// ---------------------------------------------------------------------------

/// Outcome of a TokenReview: whether the token authenticates and, if so, the
/// identity the SAR is performed against.
#[derive(Debug, Clone, Default)]
pub struct TokenVerdict {
    pub authenticated: bool,
    pub username: String,
    pub uid: String,
    pub groups: Vec<String>,
    /// Populated on `authenticated == false` so we can log why.
    pub error: Option<String>,
}

/// A SubjectAccessReview request: "may `user` (in `groups`) perform `verb` on
/// `group/resource` in `namespace`?"
#[derive(Debug, Clone)]
pub struct AccessReview {
    pub user: String,
    pub uid: String,
    pub groups: Vec<String>,
    pub namespace: String,
    pub verb: String,
    pub group: String,
    pub resource: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    #[error("api server unreachable: {0}")]
    Unreachable(String),
    #[error("api server rejected review: {0}")]
    ApiError(String),
}

/// The seam over the API server. Implemented for real by [`ApiReviewer`] and
/// with an in-memory fake in tests.
#[async_trait]
pub trait Reviewer: Send + Sync {
    async fn review_token(&self, token: &str) -> Result<TokenVerdict, ReviewError>;
    async fn review_access(&self, req: &AccessReview) -> Result<bool, ReviewError>;
}

// ---------------------------------------------------------------------------
// Operation classification — map (method, path) to an authz target.
// ---------------------------------------------------------------------------

/// The kind of registry operation a request represents, which fixes the SAR
/// verb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `GET`/`HEAD` blob or manifest → verb `get`.
    Pull,
    /// upload `POST`/`PATCH`/`PUT`, manifest `PUT` → verb `update`.
    Push,
    /// `DELETE` manifest/blob → verb `delete`.
    Delete,
    /// `tags/list`, `_catalog` → verb `list`.
    List,
    /// `/admin/*` → verb `update` on the cluster-scoped admin resource.
    Admin,
    /// `GET /v2/` version probe — always unauthenticated by spec.
    Version,
}

impl Action {
    pub fn verb(self) -> &'static str {
        match self {
            Action::Pull => "get",
            Action::Push => "update",
            Action::Delete => "delete",
            Action::List => "list",
            Action::Admin => "update",
            Action::Version => "get",
        }
    }
}

/// A classified request: what is being done, and to which repo (if any).
#[derive(Debug, Clone)]
pub struct Operation {
    pub action: Action,
    /// Repository path, e.g. `team-a/nginx`. `None` for `_catalog`, `/admin/*`
    /// and the version check.
    pub repo: Option<String>,
}

/// Classify a request purely from its method and path. Kept in lock-step with
/// the routing table in `router.rs`; anything unrecognised is treated as a
/// `Pull` (the least-privileged verb) so an unknown path can never be a
/// backdoor to a write.
pub fn classify(method: &Method, path: &str) -> Operation {
    // Version probe.
    if path == "/v2" || path == "/v2/" {
        return Operation {
            action: Action::Version,
            repo: None,
        };
    }
    // Admin endpoints.
    if path.starts_with("/admin/") {
        return Operation {
            action: Action::Admin,
            repo: None,
        };
    }
    // Catalog is cross-repo; authorize as a `list`.
    if path == "/v2/_catalog" {
        return Operation {
            action: Action::List,
            repo: None,
        };
    }

    let rest = path.strip_prefix("/v2/").unwrap_or(path);

    // tags/list → list, repo is the prefix.
    if let Some(repo) = rest.strip_suffix("/tags/list") {
        return Operation {
            action: Action::List,
            repo: non_empty(repo),
        };
    }

    // Everything else carries a repo before a `/blobs/`, `/manifests/`,
    // `/referrers/` marker. Find the repo prefix from the first such marker.
    let repo = repo_from_rest(rest);
    let action = match *method {
        Method::GET | Method::HEAD => Action::Pull,
        Method::POST | Method::PATCH | Method::PUT => Action::Push,
        Method::DELETE => Action::Delete,
        _ => Action::Pull,
    };
    // referrers is a read even though it can be reached via GET only.
    Operation { action, repo }
}

fn repo_from_rest(rest: &str) -> Option<String> {
    for marker in ["/blobs/uploads", "/blobs/", "/manifests/", "/referrers/"] {
        if let Some(idx) = rest.find(marker) {
            return non_empty(&rest[..idx]);
        }
    }
    None
}

fn non_empty(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

// ---------------------------------------------------------------------------
// Config + the K8sAuth gate.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct K8sAuthConfig {
    /// API group of the virtual SAR resource, e.g. `rspace.io`.
    pub resource_group: String,
    /// SAR resource, e.g. `repositories`.
    pub resource: String,
    /// Namespace used for single-segment repos (no `<ns>/` prefix) and for the
    /// cross-repo catalog. `None` rejects such requests.
    pub default_namespace: Option<String>,
    /// TokenReview / SAR verdict cache TTL.
    pub cache_ttl: Duration,
    /// Skip auth for loopback clients (the boot-order fast path).
    pub allow_loopback: bool,
    /// Value advertised as `realm=` in the `Bearer` challenge.
    pub token_realm: String,
    /// Value advertised as `service=` in the `Bearer` challenge.
    pub service: String,
}

/// Why a request was refused. Carried out of [`K8sAuth::check`] so the caller
/// (the middleware) can render the right OCI response + challenge.
pub enum AuthReject {
    /// No/invalid token — `401` + `WWW-Authenticate: Bearer ...`.
    Unauthorized(String),
    /// Authenticated but not permitted — `403`.
    Denied(String),
    /// The API server could not be reached / errored — `503`.
    Unavailable(String),
}

struct Cached<T> {
    value: T,
    expires: Instant,
}

pub struct K8sAuth {
    cfg: K8sAuthConfig,
    reviewer: Box<dyn Reviewer>,
    token_cache: Mutex<HashMap<String, Cached<TokenVerdict>>>,
    sar_cache: Mutex<HashMap<String, Cached<bool>>>,
}

impl K8sAuth {
    pub fn new(cfg: K8sAuthConfig, reviewer: Box<dyn Reviewer>) -> Self {
        Self {
            cfg,
            reviewer,
            token_cache: Mutex::new(HashMap::new()),
            sar_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Build the `WWW-Authenticate: Bearer ...` header value for a challenge.
    pub fn bearer_challenge(&self) -> String {
        format!(
            "Bearer realm=\"{}\",service=\"{}\"",
            self.cfg.token_realm, self.cfg.service
        )
    }

    /// Validate a token via TokenReview (cached). Public entry point for the
    /// token-exchange endpoint.
    pub async fn verify_token(&self, token: &str) -> Result<TokenVerdict, AuthReject> {
        self.token_review_cached(token).await
    }

    /// TokenReview / SAR cache TTL in seconds — advertised as `expires_in`
    /// by the token endpoint.
    pub fn cache_ttl_secs(&self) -> u64 {
        self.cfg.cache_ttl.as_secs()
    }

    /// Decide whether a request may proceed.
    ///
    /// `peer` is the client socket address (for the loopback fast path); `None`
    /// when connect-info is unavailable, in which case loopback is never
    /// assumed.
    pub async fn check(
        &self,
        op: &Operation,
        headers: &HeaderMap,
        peer: Option<IpAddr>,
    ) -> Result<(), AuthReject> {
        // The version probe is unauthenticated by spec.
        if op.action == Action::Version {
            return Ok(());
        }

        // Boot-order fast path: loopback clients skip auth so the node can
        // serve preloaded images before the API server exists.
        if self.cfg.allow_loopback && peer.map(is_loopback).unwrap_or(false) {
            return Ok(());
        }

        // ---- Authentication (TokenReview) ---------------------------------
        let Some(token) = extract_token(headers) else {
            return Err(AuthReject::Unauthorized("no bearer token presented".into()));
        };
        let verdict = self.token_review_cached(&token).await?;
        if !verdict.authenticated {
            return Err(AuthReject::Unauthorized(
                verdict
                    .error
                    .unwrap_or_else(|| "token failed authentication".into()),
            ));
        }

        // ---- Namespace resolution -----------------------------------------
        let namespace = match self.namespace_for(op) {
            Some(ns) => ns,
            None => {
                return Err(AuthReject::Denied(
                    "repository has no namespace and no --auth-k8s-default-ns is set".into(),
                ))
            }
        };

        // ---- Authorization (SubjectAccessReview) --------------------------
        let review = AccessReview {
            user: verdict.username.clone(),
            uid: verdict.uid.clone(),
            groups: verdict.groups.clone(),
            namespace,
            verb: op.action.verb().to_string(),
            group: self.cfg.resource_group.clone(),
            resource: self.cfg.resource.clone(),
        };
        if self.sar_cached(&review).await? {
            Ok(())
        } else {
            Err(AuthReject::Denied(format!(
                "user {:?} may not {} {}/{} in namespace {:?}",
                review.user, review.verb, review.group, review.resource, review.namespace
            )))
        }
    }

    /// Namespace for an operation: the repo's first path segment, or the
    /// configured default for single-segment repos, admin and catalog.
    fn namespace_for(&self, op: &Operation) -> Option<String> {
        match &op.repo {
            Some(repo) => match repo.split_once('/') {
                Some((ns, _)) if !ns.is_empty() => Some(ns.to_string()),
                _ => self.cfg.default_namespace.clone(),
            },
            None => self.cfg.default_namespace.clone(),
        }
    }

    async fn token_review_cached(&self, token: &str) -> Result<TokenVerdict, AuthReject> {
        let key = cache_key(token);
        if let Some(v) = self.get_cached(&self.token_cache, &key) {
            return Ok(v);
        }
        let verdict = self
            .reviewer
            .review_token(token)
            .await
            .map_err(|e| AuthReject::Unavailable(e.to_string()))?;
        // Cache both positive and negative verdicts briefly — a flood of bad
        // tokens must not become a flood of TokenReviews.
        self.put_cached(&self.token_cache, key, verdict.clone());
        Ok(verdict)
    }

    async fn sar_cached(&self, review: &AccessReview) -> Result<bool, AuthReject> {
        let key = format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            review.user, review.namespace, review.verb, review.group, review.resource
        );
        if let Some(v) = self.get_cached(&self.sar_cache, &key) {
            return Ok(v);
        }
        let allowed = self
            .reviewer
            .review_access(review)
            .await
            .map_err(|e| AuthReject::Unavailable(e.to_string()))?;
        self.put_cached(&self.sar_cache, key, allowed);
        Ok(allowed)
    }

    fn get_cached<T: Clone>(
        &self,
        cache: &Mutex<HashMap<String, Cached<T>>>,
        key: &str,
    ) -> Option<T> {
        let now = Instant::now();
        let mut map = cache.lock().unwrap();
        match map.get(key) {
            Some(c) if c.expires > now => Some(c.value.clone()),
            Some(_) => {
                map.remove(key);
                None
            }
            None => None,
        }
    }

    fn put_cached<T>(&self, cache: &Mutex<HashMap<String, Cached<T>>>, key: String, value: T) {
        let expires = Instant::now() + self.cfg.cache_ttl;
        cache.lock().unwrap().insert(key, Cached { value, expires });
    }
}

impl AuthReject {
    /// Render this rejection into an OCI error response with the right status
    /// and, for `Unauthorized`, the `Bearer` challenge header.
    pub fn into_response(self, auth: &K8sAuth) -> axum::response::Response {
        use axum::response::IntoResponse;
        let (code, status, message) = match self {
            AuthReject::Unauthorized(m) => (OciCode::Unauthorized, StatusCode::UNAUTHORIZED, m),
            AuthReject::Denied(m) => (OciCode::Denied, StatusCode::FORBIDDEN, m),
            AuthReject::Unavailable(m) => (
                OciCode::Unsupported,
                StatusCode::SERVICE_UNAVAILABLE,
                format!("auth backend unavailable: {m}"),
            ),
        };
        let mut resp = OciError::new(code, message)
            .with_status(status)
            .into_response();
        if status == StatusCode::UNAUTHORIZED {
            if let Ok(v) = header::HeaderValue::from_str(&auth.bearer_challenge()) {
                resp.headers_mut().insert(header::WWW_AUTHENTICATE, v);
            }
        }
        resp
    }
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6
                    .to_ipv4_mapped()
                    .map(|m| m.is_loopback())
                    .unwrap_or(false)
        }
    }
}

/// `GET /token` — the Docker distribution token-exchange endpoint the
/// `Bearer` challenge points at.
///
/// The presented credential (Basic password or `Bearer`) is a Kubernetes
/// token. We authenticate it via TokenReview and echo it back as the
/// bearer token the client will present to `/v2/`. We do **not** mint a
/// scoped token of our own: the k8s token is the identity, and
/// authorization is enforced per-request by SubjectAccessReview — so the
/// granted-vs-requested scope distinction collapses to "does this identity
/// pass the SAR for the op it actually attempts". The requested `scope` is
/// parsed for logging only.
pub async fn token_endpoint(
    auth: &K8sAuth,
    headers: &HeaderMap,
    scope: Option<&str>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let Some(token) = extract_token(headers) else {
        return AuthReject::Unauthorized("no credentials presented to token endpoint".into())
            .into_response(auth);
    };

    if let Some(s) = scope {
        if let Some(parsed) = parse_scope(s) {
            tracing::debug!(
                resource = %parsed.0,
                name = %parsed.1,
                actions = %parsed.2,
                "token endpoint: scope requested (informational; SAR enforces per-request)"
            );
        }
    }

    match auth.verify_token(&token).await {
        Ok(v) if v.authenticated => {
            let body = serde_json::json!({
                "token": token,
                "access_token": token,
                "expires_in": auth.cache_ttl_secs(),
            });
            (StatusCode::OK, axum::Json(body)).into_response()
        }
        Ok(v) => AuthReject::Unauthorized(
            v.error
                .unwrap_or_else(|| "token failed authentication".into()),
        )
        .into_response(auth),
        Err(reject) => reject.into_response(auth),
    }
}

/// Parse a distribution token `scope` value of the form
/// `repository:<name>:<action[,action...]>` into
/// `(resourcetype, name, actions)`. Returns `None` if malformed.
fn parse_scope(scope: &str) -> Option<(String, String, String)> {
    // name itself may contain ':' only via the port in a pull-through
    // scenario, which we don't support — split into exactly 3 from the
    // left and right ends: resourcetype, name, actions.
    let first = scope.find(':')?;
    let last = scope.rfind(':')?;
    if first == last {
        return None;
    }
    let resourcetype = &scope[..first];
    let name = &scope[first + 1..last];
    let actions = &scope[last + 1..];
    if resourcetype.is_empty() || name.is_empty() || actions.is_empty() {
        return None;
    }
    Some((
        resourcetype.to_string(),
        name.to_string(),
        actions.to_string(),
    ))
}

/// Pull the Kubernetes token from the request. Accepts either
/// `Authorization: Bearer <token>` (the token flow) or
/// `Authorization: Basic <base64(user:token)>` — the username is ignored, so
/// `podman login -u anything -p $TOKEN` works.
fn extract_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    if let Some(rest) = strip_ci(raw, "Bearer ") {
        let t = rest.trim();
        return if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        };
    }
    if let Some(rest) = strip_ci(raw, "Basic ") {
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(rest.trim())
            .ok()?;
        let s = String::from_utf8(decoded).ok()?;
        let (_user, token) = s.split_once(':')?;
        return if token.is_empty() {
            None
        } else {
            Some(token.to_string())
        };
    }
    None
}

fn strip_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// Never key the cache on the raw token. A SHA-256 hex digest is enough to
/// separate distinct tokens without keeping the secret material in a map.
fn cache_key(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    hex::encode(h.finalize())
}

// ---------------------------------------------------------------------------
// Real reviewer — talks to the Kubernetes API server over HTTPS.
// ---------------------------------------------------------------------------

/// In-cluster (or explicitly configured) connection to the API server used to
/// run TokenReview and SubjectAccessReview.
pub struct ApiReviewer {
    client: reqwest::Client,
    base: String,
    /// The registry's OWN ServiceAccount token, used to authenticate the
    /// review calls themselves to the API server.
    sa_token: String,
}

const SA_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

impl ApiReviewer {
    /// Build a reviewer from the in-cluster ServiceAccount mount, optionally
    /// overriding the API server URL (`--auth-k8s-api`).
    pub fn in_cluster(api_override: Option<&str>) -> anyhow::Result<Self> {
        use anyhow::Context;

        let base = match api_override {
            Some(u) => u.trim_end_matches('/').to_string(),
            None => {
                let host = std::env::var("KUBERNETES_SERVICE_HOST")
                    .context("KUBERNETES_SERVICE_HOST unset and no --auth-k8s-api given")?;
                let port =
                    std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".into());
                if host.contains(':') {
                    format!("https://[{host}]:{port}")
                } else {
                    format!("https://{host}:{port}")
                }
            }
        };

        let sa_token = std::fs::read_to_string(format!("{SA_DIR}/token"))
            .with_context(|| format!("reading {SA_DIR}/token (registry ServiceAccount token)"))?
            .trim()
            .to_string();

        let mut builder = reqwest::Client::builder();
        // Trust the cluster CA when the mount is present; fall back to the
        // system roots otherwise (e.g. an external kube-apiserver behind a
        // publicly-trusted cert).
        if let Ok(ca) = std::fs::read(format!("{SA_DIR}/ca.crt")) {
            let cert = reqwest::Certificate::from_pem(&ca).context("parsing cluster CA cert")?;
            builder = builder.add_root_certificate(cert);
        }
        let client = builder
            .timeout(Duration::from_secs(10))
            .build()
            .context("building API server HTTP client")?;

        Ok(Self {
            client,
            base,
            sa_token,
        })
    }

    async fn post(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, ReviewError> {
        let url = format!("{}{}", self.base, path);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.sa_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| ReviewError::Unreachable(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ReviewError::Unreachable(e.to_string()))?;
        if !status.is_success() {
            return Err(ReviewError::ApiError(format!("{status}: {text}")));
        }
        serde_json::from_str(&text).map_err(|e| ReviewError::ApiError(e.to_string()))
    }
}

#[async_trait]
impl Reviewer for ApiReviewer {
    async fn review_token(&self, token: &str) -> Result<TokenVerdict, ReviewError> {
        let body = serde_json::json!({
            "apiVersion": "authentication.k8s.io/v1",
            "kind": "TokenReview",
            "spec": { "token": token },
        });
        let v = self
            .post("/apis/authentication.k8s.io/v1/tokenreviews", body)
            .await?;
        let status = &v["status"];
        let authenticated = status["authenticated"].as_bool().unwrap_or(false);
        let user = &status["user"];
        Ok(TokenVerdict {
            authenticated,
            username: user["username"].as_str().unwrap_or("").to_string(),
            uid: user["uid"].as_str().unwrap_or("").to_string(),
            groups: user["groups"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|g| g.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            error: if authenticated {
                None
            } else {
                Some(
                    status["error"]
                        .as_str()
                        .unwrap_or("unauthenticated")
                        .to_string(),
                )
            },
        })
    }

    async fn review_access(&self, req: &AccessReview) -> Result<bool, ReviewError> {
        let body = serde_json::json!({
            "apiVersion": "authorization.k8s.io/v1",
            "kind": "SubjectAccessReview",
            "spec": {
                "user": req.user,
                "uid": req.uid,
                "groups": req.groups,
                "resourceAttributes": {
                    "namespace": req.namespace,
                    "verb": req.verb,
                    "group": req.group,
                    "resource": req.resource,
                },
            },
        });
        let v = self
            .post("/apis/authorization.k8s.io/v1/subjectaccessreviews", body)
            .await?;
        Ok(v["status"]["allowed"].as_bool().unwrap_or(false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    /// A fake reviewer: a fixed set of valid tokens → identity, and an
    /// allow-list of `(user, namespace, verb)` tuples.
    struct FakeReviewer {
        tokens: HashMap<String, TokenVerdict>,
        allowed: Vec<(String, String, String)>,
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
                .any(|(u, ns, verb)| u == &req.user && ns == &req.namespace && verb == &req.verb))
        }
    }

    fn auth_with(allow_loopback: bool, default_ns: Option<&str>) -> K8sAuth {
        let mut tokens = HashMap::new();
        tokens.insert(
            "good".to_string(),
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
            allowed: vec![
                ("alice".into(), "team-a".into(), "get".into()),
                ("alice".into(), "team-a".into(), "update".into()),
            ],
        };
        let cfg = K8sAuthConfig {
            resource_group: "rspace.io".into(),
            resource: "repositories".into(),
            default_namespace: default_ns.map(String::from),
            cache_ttl: Duration::from_secs(120),
            allow_loopback,
            token_realm: "https://reg.example/token".into(),
            service: "rspace-registry".into(),
        };
        K8sAuth::new(cfg, Box::new(reviewer))
    }

    fn bearer(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        h
    }

    fn op(action: Action, repo: &str) -> Operation {
        Operation {
            action,
            repo: Some(repo.to_string()),
        }
    }

    #[tokio::test]
    async fn pull_allowed_for_permitted_user() {
        let a = auth_with(false, None);
        let r = a
            .check(&op(Action::Pull, "team-a/nginx"), &bearer("good"), None)
            .await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn push_denied_when_not_permitted() {
        let a = auth_with(false, None);
        // team-b is not in the allow-list.
        let r = a
            .check(&op(Action::Push, "team-b/nginx"), &bearer("good"), None)
            .await;
        assert!(matches!(r, Err(AuthReject::Denied(_))));
    }

    #[tokio::test]
    async fn no_token_is_unauthorized() {
        let a = auth_with(false, None);
        let r = a
            .check(&op(Action::Pull, "team-a/nginx"), &HeaderMap::new(), None)
            .await;
        assert!(matches!(r, Err(AuthReject::Unauthorized(_))));
    }

    #[tokio::test]
    async fn bad_token_is_unauthorized() {
        let a = auth_with(false, None);
        let r = a
            .check(&op(Action::Pull, "team-a/nginx"), &bearer("nope"), None)
            .await;
        assert!(matches!(r, Err(AuthReject::Unauthorized(_))));
    }

    #[tokio::test]
    async fn loopback_skips_auth_when_enabled() {
        let a = auth_with(true, None);
        let peer = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
        // No credentials at all, yet allowed.
        let r = a
            .check(&op(Action::Pull, "team-a/nginx"), &HeaderMap::new(), peer)
            .await;
        assert!(r.is_ok());
        // v6 loopback too.
        let peer6 = Some(IpAddr::V6(Ipv6Addr::LOCALHOST));
        assert!(a
            .check(&op(Action::Push, "team-b/nginx"), &HeaderMap::new(), peer6)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn loopback_not_skipped_when_disabled() {
        let a = auth_with(false, None);
        let peer = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let r = a
            .check(&op(Action::Pull, "team-a/nginx"), &HeaderMap::new(), peer)
            .await;
        assert!(matches!(r, Err(AuthReject::Unauthorized(_))));
    }

    #[tokio::test]
    async fn single_segment_repo_needs_default_ns() {
        // No default ns → single-segment repo is denied.
        let a = auth_with(false, None);
        let r = a
            .check(&op(Action::Pull, "nginx"), &bearer("good"), None)
            .await;
        assert!(matches!(r, Err(AuthReject::Denied(_))));

        // With default ns = team-a, it authorizes there.
        let a = auth_with(false, Some("team-a"));
        let r = a
            .check(&op(Action::Pull, "nginx"), &bearer("good"), None)
            .await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn version_check_always_allowed() {
        let a = auth_with(false, None);
        let r = a
            .check(
                &Operation {
                    action: Action::Version,
                    repo: None,
                },
                &HeaderMap::new(),
                None,
            )
            .await;
        assert!(r.is_ok());
    }

    #[test]
    fn classify_maps_methods_to_verbs() {
        assert_eq!(classify(&Method::GET, "/v2/").action, Action::Version);
        assert_eq!(classify(&Method::GET, "/v2/_catalog").action, Action::List);

        let o = classify(&Method::GET, "/v2/team-a/nginx/tags/list");
        assert_eq!(o.action, Action::List);
        assert_eq!(o.repo.as_deref(), Some("team-a/nginx"));

        let o = classify(&Method::GET, "/v2/team-a/nginx/blobs/sha256:abc");
        assert_eq!(o.action, Action::Pull);
        assert_eq!(o.repo.as_deref(), Some("team-a/nginx"));

        let o = classify(&Method::PUT, "/v2/team-a/nginx/manifests/latest");
        assert_eq!(o.action, Action::Push);
        assert_eq!(o.repo.as_deref(), Some("team-a/nginx"));

        let o = classify(&Method::POST, "/v2/team-a/nginx/blobs/uploads/");
        assert_eq!(o.action, Action::Push);
        assert_eq!(o.repo.as_deref(), Some("team-a/nginx"));

        let o = classify(&Method::DELETE, "/v2/team-a/nginx/manifests/sha256:abc");
        assert_eq!(o.action, Action::Delete);
        assert_eq!(o.repo.as_deref(), Some("team-a/nginx"));

        assert_eq!(classify(&Method::POST, "/admin/gc").action, Action::Admin);
    }

    #[test]
    fn extract_token_handles_bearer_and_basic() {
        assert_eq!(extract_token(&bearer("tok")).as_deref(), Some("tok"));

        use base64::Engine;
        let creds = base64::engine::general_purpose::STANDARD.encode("anything:mytoken");
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Basic {creds}")).unwrap(),
        );
        assert_eq!(extract_token(&h).as_deref(), Some("mytoken"));

        assert_eq!(extract_token(&HeaderMap::new()), None);
    }

    #[test]
    fn parse_scope_splits_repository_scope() {
        assert_eq!(
            parse_scope("repository:team-a/nginx:pull,push"),
            Some((
                "repository".into(),
                "team-a/nginx".into(),
                "pull,push".into()
            ))
        );
        assert_eq!(
            parse_scope("repository:nginx:pull"),
            Some(("repository".into(), "nginx".into(), "pull".into()))
        );
        // Malformed — no actions segment.
        assert_eq!(parse_scope("repository:nginx"), None);
        assert_eq!(parse_scope("garbage"), None);
    }

    #[tokio::test]
    async fn token_endpoint_issues_token_for_valid_credentials() {
        use base64::Engine;
        let a = auth_with(false, None);
        let creds = base64::engine::general_purpose::STANDARD.encode("anyuser:good");
        let mut h = HeaderMap::new();
        h.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Basic {creds}")).unwrap(),
        );
        let resp = token_endpoint(&a, &h, Some("repository:team-a/nginx:pull")).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn token_endpoint_rejects_bad_and_missing_credentials() {
        let a = auth_with(false, None);
        // Bad token.
        let resp = token_endpoint(&a, &bearer("nope"), None).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // No credentials.
        let resp = token_endpoint(&a, &HeaderMap::new(), None).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
