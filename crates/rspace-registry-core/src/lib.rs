//! OCI types, `Storage` trait, GC engine. Used by `rspace-registry` (binary)
//! and any storage backend crate (`rspace-registry-fs`, future
//! `rspace-registry-rspacefs`).

pub mod digest;
pub mod gc;
pub mod manifest;
pub mod migrate;
pub mod multi;
pub mod quota;
pub mod replicate;
pub mod repo_router;
pub mod storage;

pub use digest::Digest;
pub use manifest::{
    parse_manifest_refs, Descriptor, Manifest, MANIFEST_MEDIA_TYPES, OCI_INDEX_MEDIA_TYPE,
    OCI_MANIFEST_MEDIA_TYPE,
};
pub use migrate::MigrateReport;
pub use multi::{MultiStore, Partition};
pub use quota::{Quota, QuotaStatus, QuotaStorage};
pub use replicate::{ReplicateConfig, ReplicateReport};
pub use repo_router::{RepoRouter, RouteRule};
pub use storage::{Reference, Storage, StorageError, UploadStatus};
