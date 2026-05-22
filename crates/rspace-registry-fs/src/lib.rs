//! Filesystem-backed `Storage` impl. Directory layout:
//!
//! ```text
//! <data>/
//!   blobs/
//!     sha256/<first-two-hex>/<digest>
//!     sha512/<first-two-hex>/<digest>
//!   manifests/
//!     <repo>/
//!       tags/<tag>           → file holding the canonical digest as a string
//!       digests/<digest>     → file holding the manifest bytes
//! ```
//!
//! Atomic writes via write-to-temp-then-rename. No locking — multiple
//! writers for the same digest produce the same bytes (content-addressed),
//! and `rename` is atomic at the syscall level.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use rspace_registry_core::{Digest, Reference, Storage, StorageError};
use sha2::{Digest as _, Sha256, Sha512};
use tokio::fs;
use tokio::io::AsyncWriteExt;

pub struct FsStorage {
    root: PathBuf,
}

impl FsStorage {
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn blob_path(&self, d: &Digest) -> PathBuf {
        let alg = match d.algorithm {
            rspace_registry_core::digest::Algorithm::Sha256 => "sha256",
            rspace_registry_core::digest::Algorithm::Sha512 => "sha512",
        };
        let prefix = &d.hex[0..2];
        self.root.join("blobs").join(alg).join(prefix).join(&d.hex)
    }

    fn manifest_tag_path(&self, repo: &str, tag: &str) -> PathBuf {
        self.root
            .join("manifests")
            .join(repo)
            .join("tags")
            .join(tag)
    }

    fn manifest_digest_path(&self, repo: &str, d: &Digest) -> PathBuf {
        self.root
            .join("manifests")
            .join(repo)
            .join("digests")
            .join(d.to_string())
    }

    async fn atomic_write(path: &Path, content: &[u8]) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let tmp = path.with_extension("tmp");
        let mut f = fs::File::create(&tmp).await?;
        f.write_all(content).await?;
        f.flush().await?;
        f.sync_all().await?;
        drop(f);
        fs::rename(&tmp, path).await?;
        Ok(())
    }
}

#[async_trait]
impl Storage for FsStorage {
    async fn blob_exists(&self, digest: &Digest) -> Result<bool, StorageError> {
        Ok(fs::try_exists(self.blob_path(digest)).await?)
    }

    async fn blob_size(&self, digest: &Digest) -> Result<u64, StorageError> {
        let m = fs::metadata(self.blob_path(digest))
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => StorageError::NotFound,
                _ => StorageError::Io(e),
            })?;
        Ok(m.len())
    }

    async fn blob_read(&self, digest: &Digest) -> Result<Vec<u8>, StorageError> {
        fs::read(self.blob_path(digest))
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => StorageError::NotFound,
                _ => StorageError::Io(e),
            })
    }

    async fn blob_write(&self, expected: &Digest, content: &[u8]) -> Result<(), StorageError> {
        let actual = match expected.algorithm {
            rspace_registry_core::digest::Algorithm::Sha256 => {
                let h = Sha256::digest(content);
                Digest {
                    algorithm: rspace_registry_core::digest::Algorithm::Sha256,
                    hex: hex::encode(h),
                }
            }
            rspace_registry_core::digest::Algorithm::Sha512 => {
                let h = Sha512::digest(content);
                Digest {
                    algorithm: rspace_registry_core::digest::Algorithm::Sha512,
                    hex: hex::encode(h),
                }
            }
        };
        if actual != *expected {
            return Err(StorageError::DigestMismatch {
                expected: expected.clone(),
                got: actual,
            });
        }
        Self::atomic_write(&self.blob_path(expected), content).await?;
        Ok(())
    }

    async fn blob_delete(&self, digest: &Digest) -> Result<(), StorageError> {
        fs::remove_file(self.blob_path(digest))
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => StorageError::NotFound,
                _ => StorageError::Io(e),
            })
    }

    async fn manifest_get(
        &self,
        repo: &str,
        reference: &Reference,
    ) -> Result<Vec<u8>, StorageError> {
        let digest = match reference {
            Reference::Digest(d) => d.clone(),
            Reference::Tag(t) => {
                let s = fs::read_to_string(self.manifest_tag_path(repo, t))
                    .await
                    .map_err(|e| match e.kind() {
                        std::io::ErrorKind::NotFound => StorageError::NotFound,
                        _ => StorageError::Io(e),
                    })?;
                s.trim()
                    .parse()
                    .map_err(|e| StorageError::Internal(format!("bad stored tag: {e}")))?
            }
        };
        fs::read(self.manifest_digest_path(repo, &digest))
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => StorageError::NotFound,
                _ => StorageError::Io(e),
            })
    }

    async fn manifest_put(
        &self,
        repo: &str,
        reference: &Reference,
        content: &[u8],
    ) -> Result<Digest, StorageError> {
        // Manifest digest is always sha256 of the canonical bytes.
        let h = Sha256::digest(content);
        let digest = Digest {
            algorithm: rspace_registry_core::digest::Algorithm::Sha256,
            hex: hex::encode(h),
        };
        Self::atomic_write(&self.manifest_digest_path(repo, &digest), content).await?;
        if let Reference::Tag(tag) = reference {
            Self::atomic_write(
                &self.manifest_tag_path(repo, tag),
                digest.to_string().as_bytes(),
            )
            .await?;
        }
        Ok(digest)
    }

    async fn manifest_delete(&self, repo: &str, reference: &Reference) -> Result<(), StorageError> {
        let path = match reference {
            Reference::Digest(d) => self.manifest_digest_path(repo, d),
            Reference::Tag(t) => self.manifest_tag_path(repo, t),
        };
        fs::remove_file(path).await.map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => StorageError::NotFound,
            _ => StorageError::Io(e),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blob_roundtrip() {
        let tmp = tempdir();
        let fs = FsStorage::new(&tmp).unwrap();
        let bytes = b"hello";
        let h = Sha256::digest(bytes);
        let d = Digest {
            algorithm: rspace_registry_core::digest::Algorithm::Sha256,
            hex: hex::encode(h),
        };
        fs.blob_write(&d, bytes).await.unwrap();
        assert!(fs.blob_exists(&d).await.unwrap());
        assert_eq!(fs.blob_size(&d).await.unwrap(), bytes.len() as u64);
        assert_eq!(fs.blob_read(&d).await.unwrap(), bytes);
        fs.blob_delete(&d).await.unwrap();
        assert!(!fs.blob_exists(&d).await.unwrap());
    }

    #[tokio::test]
    async fn blob_write_rejects_wrong_digest() {
        let tmp = tempdir();
        let fs = FsStorage::new(&tmp).unwrap();
        let wrong = Digest {
            algorithm: rspace_registry_core::digest::Algorithm::Sha256,
            hex: "0".repeat(64),
        };
        let err = fs.blob_write(&wrong, b"hello").await.unwrap_err();
        assert!(matches!(err, StorageError::DigestMismatch { .. }));
    }

    #[tokio::test]
    async fn manifest_tag_then_digest() {
        let tmp = tempdir();
        let fs = FsStorage::new(&tmp).unwrap();
        let m = br#"{"schemaVersion":2}"#;
        let d = fs
            .manifest_put("library/foo", &Reference::Tag("v1".into()), m)
            .await
            .unwrap();
        // GET by tag returns the bytes.
        let by_tag = fs
            .manifest_get("library/foo", &Reference::Tag("v1".into()))
            .await
            .unwrap();
        assert_eq!(&*by_tag, m.as_ref());
        // GET by the returned digest also works.
        let by_digest = fs
            .manifest_get("library/foo", &Reference::Digest(d))
            .await
            .unwrap();
        assert_eq!(&*by_digest, m.as_ref());
    }

    fn tempdir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "rspace-registry-fs-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
