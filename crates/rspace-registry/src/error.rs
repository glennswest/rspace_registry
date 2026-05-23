//! OCI Distribution Spec v1.1 error envelope.
//!
//! All non-success responses follow:
//!
//! ```json
//! {"errors":[{"code":"BLOB_UNKNOWN","message":"...","detail":...}]}
//! ```

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use rspace_registry_core::StorageError;
use serde::Serialize;

#[derive(Debug, Clone, Copy)]
pub enum OciCode {
    BlobUnknown,
    BlobUploadInvalid,
    BlobUploadUnknown,
    DigestInvalid,
    ManifestBlobUnknown,
    ManifestInvalid,
    ManifestUnknown,
    NameInvalid,
    NameUnknown,
    SizeInvalid,
    Unauthorized,
    Denied,
    Unsupported,
}

impl OciCode {
    pub fn as_str(self) -> &'static str {
        match self {
            OciCode::BlobUnknown => "BLOB_UNKNOWN",
            OciCode::BlobUploadInvalid => "BLOB_UPLOAD_INVALID",
            OciCode::BlobUploadUnknown => "BLOB_UPLOAD_UNKNOWN",
            OciCode::DigestInvalid => "DIGEST_INVALID",
            OciCode::ManifestBlobUnknown => "MANIFEST_BLOB_UNKNOWN",
            OciCode::ManifestInvalid => "MANIFEST_INVALID",
            OciCode::ManifestUnknown => "MANIFEST_UNKNOWN",
            OciCode::NameInvalid => "NAME_INVALID",
            OciCode::NameUnknown => "NAME_UNKNOWN",
            OciCode::SizeInvalid => "SIZE_INVALID",
            OciCode::Unauthorized => "UNAUTHORIZED",
            OciCode::Denied => "DENIED",
            OciCode::Unsupported => "UNSUPPORTED",
        }
    }

    pub fn default_status(self) -> StatusCode {
        match self {
            OciCode::Unauthorized => StatusCode::UNAUTHORIZED,
            OciCode::Denied => StatusCode::FORBIDDEN,
            OciCode::Unsupported => StatusCode::METHOD_NOT_ALLOWED,
            OciCode::BlobUnknown
            | OciCode::BlobUploadUnknown
            | OciCode::ManifestBlobUnknown
            | OciCode::ManifestUnknown
            | OciCode::NameUnknown => StatusCode::NOT_FOUND,
            OciCode::BlobUploadInvalid
            | OciCode::DigestInvalid
            | OciCode::ManifestInvalid
            | OciCode::NameInvalid
            | OciCode::SizeInvalid => StatusCode::BAD_REQUEST,
        }
    }
}

#[derive(Debug, Serialize)]
struct WireError {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct WireEnvelope {
    errors: Vec<WireError>,
}

#[derive(Debug)]
pub struct OciError {
    pub status: StatusCode,
    pub code: OciCode,
    pub message: String,
    pub detail: Option<serde_json::Value>,
}

impl OciError {
    pub fn new(code: OciCode, message: impl Into<String>) -> Self {
        Self {
            status: code.default_status(),
            code,
            message: message.into(),
            detail: None,
        }
    }

    pub fn with_status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }

    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }
}

impl IntoResponse for OciError {
    fn into_response(self) -> Response {
        let body = WireEnvelope {
            errors: vec![WireError {
                code: self.code.as_str(),
                message: self.message,
                detail: self.detail,
            }],
        };
        (self.status, Json(body)).into_response()
    }
}

impl From<StorageError> for OciError {
    fn from(e: StorageError) -> Self {
        match e {
            StorageError::NotFound => OciError::new(OciCode::BlobUnknown, "not found"),
            StorageError::DigestMismatch { expected, got } => OciError::new(
                OciCode::DigestInvalid,
                format!("digest mismatch: expected {expected}, got {got}"),
            ),
            StorageError::Invalid(m) => OciError::new(OciCode::BlobUploadInvalid, m),
            StorageError::Io(e) => OciError::new(OciCode::Unsupported, format!("io: {e}"))
                .with_status(StatusCode::INTERNAL_SERVER_ERROR),
            StorageError::Internal(m) => OciError::new(OciCode::Unsupported, m)
                .with_status(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }
}
