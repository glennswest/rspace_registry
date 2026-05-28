//! OCI manifest parsing.
//!
//! We don't need to fully validate the manifest schema (a registry serves
//! arbitrary content-addressed bytes, that's the whole point). But we DO
//! need to extract referenced blob digests so GC can reach them, and the
//! `subject` field so referrers queries work.

use serde::{Deserialize, Serialize};

use crate::digest::Digest;

pub const OCI_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";
pub const DOCKER_MANIFEST_V2_MEDIA_TYPE: &str =
    "application/vnd.docker.distribution.manifest.v2+json";
pub const DOCKER_MANIFEST_LIST_V2_MEDIA_TYPE: &str =
    "application/vnd.docker.distribution.manifest.list.v2+json";

pub const MANIFEST_MEDIA_TYPES: &[&str] = &[
    OCI_MANIFEST_MEDIA_TYPE,
    OCI_INDEX_MEDIA_TYPE,
    DOCKER_MANIFEST_V2_MEDIA_TYPE,
    DOCKER_MANIFEST_LIST_V2_MEDIA_TYPE,
];

/// Single content descriptor. We only deserialise the fields the registry
/// actually inspects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Descriptor {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub digest: Digest,
    #[serde(default)]
    pub size: u64,
    #[serde(
        rename = "artifactType",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub artifact_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Loosely-typed manifest view used by the registry. Captures both
/// `image manifest` (config + layers) and `image index` (manifests) shapes.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    #[serde(default, rename = "mediaType")]
    pub media_type: Option<String>,
    #[serde(default, rename = "artifactType")]
    pub artifact_type: Option<String>,
    #[serde(default)]
    pub config: Option<Descriptor>,
    #[serde(default)]
    pub layers: Vec<Descriptor>,
    #[serde(default)]
    pub manifests: Vec<Descriptor>,
    #[serde(default)]
    pub subject: Option<Descriptor>,
    #[serde(default)]
    pub annotations: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Manifest {
    /// Digests this manifest references — config, layers, child manifests.
    /// Used by GC's mark phase.
    pub fn referenced_digests(&self) -> Vec<Digest> {
        let mut out = Vec::new();
        if let Some(c) = &self.config {
            out.push(c.digest.clone());
        }
        for l in &self.layers {
            out.push(l.digest.clone());
        }
        for m in &self.manifests {
            out.push(m.digest.clone());
        }
        if let Some(s) = &self.subject {
            out.push(s.digest.clone());
        }
        out
    }
}

/// Convenience wrapper around `serde_json::from_slice` for callers that
/// don't want to pull in `serde_json` directly.
pub fn parse_manifest_refs(bytes: &[u8]) -> Result<Manifest, serde_json::Error> {
    serde_json::from_slice(bytes)
}
