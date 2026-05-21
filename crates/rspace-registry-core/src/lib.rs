//! OCI types, `Storage` trait, GC engine. Used by `rspace-registry` (binary)
//! and any storage backend crate (`rspace-registry-fs`, future
//! `rspace-registry-rspacefs`).

pub mod digest;
pub mod storage;

pub use digest::Digest;
pub use storage::{Reference, Storage, StorageError};
