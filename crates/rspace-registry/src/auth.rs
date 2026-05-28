//! HTTP Basic auth backed by an htpasswd file.
//!
//! Supported entry formats:
//!
//! - `$2y$...` / `$2a$...` / `$2b$...` — bcrypt (the modern default)
//! - plain `password` — plaintext (allowed for tests only; warned at load)
//!
//! Apache MD5 (`$apr1$`) and SHA1 (`{SHA}`) entries are intentionally
//! refused — a typo'd line locks out the affected user rather than
//! silently granting access.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use base64::Engine;

#[derive(Debug, Clone)]
enum Hash {
    Bcrypt(String),
    Plain(String),
}

#[derive(Debug, Clone, Default)]
pub struct Htpasswd {
    users: HashMap<String, Hash>,
}

impl Htpasswd {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut users = HashMap::new();
        let mut plain_warned = false;
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((user, hash)) = line.split_once(':') else {
                tracing::warn!(line = i + 1, "htpasswd: skipping line without ':'");
                continue;
            };
            let h =
                if hash.starts_with("$2y$") || hash.starts_with("$2a$") || hash.starts_with("$2b$")
                {
                    Hash::Bcrypt(hash.to_string())
                } else if hash.starts_with('$') || hash.starts_with('{') {
                    tracing::warn!(
                        user = user,
                        "htpasswd: skipping user with unsupported hash scheme — use bcrypt"
                    );
                    continue;
                } else {
                    if !plain_warned {
                        tracing::warn!(
                            "htpasswd: plaintext entries detected — use bcrypt in production"
                        );
                        plain_warned = true;
                    }
                    Hash::Plain(hash.to_string())
                };
            users.insert(user.to_string(), h);
        }
        if users.is_empty() {
            return Err(anyhow!("htpasswd file has no usable entries"));
        }
        Ok(Self { users })
    }

    /// Insert a plaintext entry; for tests only.
    #[doc(hidden)]
    pub fn insert_plain(&mut self, user: &str, pw: &str) {
        self.users
            .insert(user.to_string(), Hash::Plain(pw.to_string()));
    }

    pub fn verify(&self, user: &str, password: &str) -> bool {
        let Some(stored) = self.users.get(user) else {
            return false;
        };
        match stored {
            Hash::Plain(p) => constant_eq(p.as_bytes(), password.as_bytes()),
            Hash::Bcrypt(h) => bcrypt::verify(password, h).unwrap_or(false),
        }
    }
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Decode an `Authorization: Basic ...` header into `(user, password)`.
pub fn parse_basic(headers: &HeaderMap) -> Option<(String, String)> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = raw
        .strip_prefix("Basic ")
        .or_else(|| raw.strip_prefix("basic "))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(token.trim())
        .ok()?;
    let s = String::from_utf8(decoded).ok()?;
    let (u, p) = s.split_once(':')?;
    Some((u.to_string(), p.to_string()))
}

/// Build a `WWW-Authenticate: Basic realm="..."` response for a 401.
pub fn challenge_headers(realm: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    if let Ok(v) = HeaderValue::from_str(&format!("Basic realm=\"{realm}\"")) {
        h.insert(header::WWW_AUTHENTICATE, v);
    }
    h
}

/// Status returned by the auth middleware when no/invalid credentials are
/// supplied — `401 Unauthorized` per OCI Distribution Spec.
pub const UNAUTH_STATUS: StatusCode = StatusCode::UNAUTHORIZED;
