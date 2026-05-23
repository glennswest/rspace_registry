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
//!   uploads/
//!     <uuid>                 → in-progress upload, append-only
//! ```
//!
//! Atomic writes via write-to-temp-then-rename. No locking — multiple
//! writers for the same digest produce the same bytes (content-addressed),
//! and `rename` is atomic at the syscall level.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use rspace_registry_core::digest::Algorithm;
use rspace_registry_core::{Digest, Reference, Storage, StorageError, UploadStatus};
use sha2::{Digest as _, Sha256, Sha512};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

pub struct FsStorage {
    root: PathBuf,
}

impl FsStorage {
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(root.join("blobs"))?;
        std::fs::create_dir_all(root.join("manifests"))?;
        std::fs::create_dir_all(root.join("uploads"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn blob_path(&self, d: &Digest) -> PathBuf {
        let prefix = &d.hex[0..2];
        self.root
            .join("blobs")
            .join(alg_name(d.algorithm))
            .join(prefix)
            .join(&d.hex)
    }

    fn manifest_tag_path(&self, repo: &str, tag: &str) -> PathBuf {
        self.root.join("manifests").join(repo).join("tags").join(tag)
    }

    fn manifest_digest_path(&self, repo: &str, d: &Digest) -> PathBuf {
        self.root
            .join("manifests")
            .join(repo)
            .join("digests")
            .join(d.to_string())
    }

    fn upload_path(&self, id: Uuid) -> PathBuf {
        self.root.join("uploads").join(id.to_string())
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

fn alg_name(a: Algorithm) -> &'static str {
    match a {
        Algorithm::Sha256 => "sha256",
        Algorithm::Sha512 => "sha512",
    }
}

fn hash_content(alg: Algorithm, content: &[u8]) -> Digest {
    match alg {
        Algorithm::Sha256 => Digest {
            algorithm: Algorithm::Sha256,
            hex: hex::encode(Sha256::digest(content)),
        },
        Algorithm::Sha512 => Digest {
            algorithm: Algorithm::Sha512,
            hex: hex::encode(Sha512::digest(content)),
        },
    }
}

fn io_to_storage(e: std::io::Error) -> StorageError {
    match e.kind() {
        std::io::ErrorKind::NotFound => StorageError::NotFound,
        _ => StorageError::Io(e),
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
            .map_err(io_to_storage)?;
        Ok(m.len())
    }

    async fn blob_read(&self, digest: &Digest) -> Result<Vec<u8>, StorageError> {
        fs::read(self.blob_path(digest)).await.map_err(io_to_storage)
    }

    async fn blob_write(&self, expected: &Digest, content: &[u8]) -> Result<(), StorageError> {
        let actual = hash_content(expected.algorithm, content);
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
            .map_err(io_to_storage)
    }

    // ---- Upload sessions ------------------------------------------------

    async fn upload_create(&self) -> Result<UploadStatus, StorageError> {
        let id = Uuid::new_v4();
        let path = self.upload_path(id);
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).await?;
        }
        fs::File::create(&path).await?;
        Ok(UploadStatus { id, offset: 0 })
    }

    async fn upload_status(&self, id: Uuid) -> Result<UploadStatus, StorageError> {
        let m = fs::metadata(self.upload_path(id))
            .await
            .map_err(io_to_storage)?;
        Ok(UploadStatus { id, offset: m.len() })
    }

    async fn upload_append(&self, id: Uuid, chunk: &[u8]) -> Result<UploadStatus, StorageError> {
        let path = self.upload_path(id);
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .map_err(io_to_storage)?;
        f.write_all(chunk).await?;
        f.flush().await?;
        let offset = f.metadata().await?.len();
        Ok(UploadStatus { id, offset })
    }

    async fn upload_finalize(&self, id: Uuid, expected: &Digest) -> Result<(), StorageError> {
        let upload_path = self.upload_path(id);
        let bytes = fs::read(&upload_path).await.map_err(io_to_storage)?;
        let actual = hash_content(expected.algorithm, &bytes);
        if actual != *expected {
            return Err(StorageError::DigestMismatch {
                expected: expected.clone(),
                got: actual,
            });
        }
        let dest = self.blob_path(expected);
        if let Some(p) = dest.parent() {
            fs::create_dir_all(p).await?;
        }
        // Try rename first (same-fs fast path); fall back to copy+remove
        // when uploads/ and blobs/ straddle filesystems.
        if fs::rename(&upload_path, &dest).await.is_err() {
            fs::write(&dest, &bytes).await?;
            let _ = fs::remove_file(&upload_path).await;
        }
        Ok(())
    }

    async fn upload_cancel(&self, id: Uuid) -> Result<(), StorageError> {
        fs::remove_file(self.upload_path(id))
            .await
            .map_err(io_to_storage)
    }

    // ---- Manifests ------------------------------------------------------

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
                    .map_err(io_to_storage)?;
                s.trim()
                    .parse()
                    .map_err(|e| StorageError::Internal(format!("bad stored tag: {e}")))?
            }
        };
        fs::read(self.manifest_digest_path(repo, &digest))
            .await
            .map_err(io_to_storage)
    }

    async fn manifest_put(
        &self,
        repo: &str,
        reference: &Reference,
        content: &[u8],
    ) -> Result<Digest, StorageError> {
        let digest = hash_content(Algorithm::Sha256, content);
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

    async fn manifest_delete(
        &self,
        repo: &str,
        reference: &Reference,
    ) -> Result<(), StorageError> {
        let path = match reference {
            Reference::Digest(d) => self.manifest_digest_path(repo, d),
            Reference::Tag(t) => self.manifest_tag_path(repo, t),
        };
        fs::remove_file(path).await.map_err(io_to_storage)
    }

    // ---- Listing --------------------------------------------------------

    async fn list_repos(&self) -> Result<Vec<String>, StorageError> {
        let root = self.root.join("manifests");
        let mut out = Vec::new();
        walk_repos(&root, &root, &mut out).await?;
        out.sort();
        out.dedup();
        Ok(out)
    }

    async fn list_tags(&self, repo: &str) -> Result<Vec<String>, StorageError> {
        let dir = self.root.join("manifests").join(repo).join("tags");
        let mut tags = Vec::new();
        let mut rd = match fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(tags),
            Err(e) => return Err(StorageError::Io(e)),
        };
        while let Some(entry) = rd.next_entry().await? {
            if !entry.file_type().await?.is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
                continue;
            };
            if name.ends_with(".tmp") {
                continue;
            }
            tags.push(name);
        }
        tags.sort();
        Ok(tags)
    }

    async fn list_manifest_digests(&self, repo: &str) -> Result<Vec<Digest>, StorageError> {
        let dir = self.root.join("manifests").join(repo).join("digests");
        let mut out = Vec::new();
        let mut rd = match fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(StorageError::Io(e)),
        };
        while let Some(entry) = rd.next_entry().await? {
            if !entry.file_type().await?.is_file() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
                continue;
            };
            if name.ends_with(".tmp") {
                continue;
            }
            if let Ok(d) = name.parse::<Digest>() {
                out.push(d);
            }
        }
        out.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
        Ok(out)
    }

    async fn list_all_blobs(&self) -> Result<Vec<Digest>, StorageError> {
        let mut out = Vec::new();
        for alg in [Algorithm::Sha256, Algorithm::Sha512] {
            let alg_dir = self.root.join("blobs").join(alg_name(alg));
            let mut rd = match fs::read_dir(&alg_dir).await {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(StorageError::Io(e)),
            };
            while let Some(prefix_entry) = rd.next_entry().await? {
                if !prefix_entry.file_type().await?.is_dir() {
                    continue;
                }
                let mut inner = fs::read_dir(prefix_entry.path()).await?;
                while let Some(entry) = inner.next_entry().await? {
                    if !entry.file_type().await?.is_file() {
                        continue;
                    }
                    let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
                        continue;
                    };
                    if name.ends_with(".tmp") {
                        continue;
                    }
                    if let Ok(d) = format!("{}:{}", alg_name(alg), name).parse::<Digest>() {
                        out.push(d);
                    }
                }
            }
        }
        out.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
        Ok(out)
    }
}

/// Recursive walk of the `manifests/` tree. A directory is a repo if it
/// contains either a `tags/` or `digests/` child. Repo names may be
/// slash-separated (e.g. `library/foo`).
fn walk_repos<'a>(
    base: &'a Path,
    dir: &'a Path,
    out: &'a mut Vec<String>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send + 'a>> {
    Box::pin(async move {
        let mut rd = match fs::read_dir(dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        let mut is_repo = false;
        let mut children: Vec<PathBuf> = Vec::new();
        while let Some(entry) = rd.next_entry().await? {
            if !entry.file_type().await?.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let name_s = name.to_string_lossy();
            if name_s == "tags" || name_s == "digests" {
                is_repo = true;
            } else {
                children.push(entry.path());
            }
        }
        if is_repo {
            if let Ok(rel) = dir.strip_prefix(base) {
                let s = rel.to_string_lossy().replace('\\', "/");
                if !s.is_empty() {
                    out.push(s);
                }
            }
        }
        for child in children {
            walk_repos(base, &child, out).await?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blob_roundtrip() {
        let tmp = tempdir();
        let fs = FsStorage::new(&tmp).unwrap();
        let bytes = b"hello";
        let d = Digest {
            algorithm: Algorithm::Sha256,
            hex: hex::encode(Sha256::digest(bytes)),
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
            algorithm: Algorithm::Sha256,
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
        let by_tag = fs
            .manifest_get("library/foo", &Reference::Tag("v1".into()))
            .await
            .unwrap();
        assert_eq!(&*by_tag, m.as_ref());
        let by_digest = fs
            .manifest_get("library/foo", &Reference::Digest(d))
            .await
            .unwrap();
        assert_eq!(&*by_digest, m.as_ref());
    }

    #[tokio::test]
    async fn upload_chunked_then_finalise() {
        let tmp = tempdir();
        let fs = FsStorage::new(&tmp).unwrap();
        let start = fs.upload_create().await.unwrap();
        assert_eq!(start.offset, 0);
        let s1 = fs.upload_append(start.id, b"hel").await.unwrap();
        assert_eq!(s1.offset, 3);
        let s2 = fs.upload_append(start.id, b"lo").await.unwrap();
        assert_eq!(s2.offset, 5);
        let digest = Digest {
            algorithm: Algorithm::Sha256,
            hex: hex::encode(Sha256::digest(b"hello")),
        };
        fs.upload_finalize(start.id, &digest).await.unwrap();
        assert_eq!(fs.blob_read(&digest).await.unwrap(), b"hello");
    }

    #[tokio::test]
    async fn upload_finalize_rejects_wrong_digest() {
        let tmp = tempdir();
        let fs = FsStorage::new(&tmp).unwrap();
        let s = fs.upload_create().await.unwrap();
        fs.upload_append(s.id, b"hello").await.unwrap();
        let wrong = Digest {
            algorithm: Algorithm::Sha256,
            hex: "0".repeat(64),
        };
        let err = fs.upload_finalize(s.id, &wrong).await.unwrap_err();
        assert!(matches!(err, StorageError::DigestMismatch { .. }));
    }

    #[tokio::test]
    async fn list_repos_tags_blobs() {
        let tmp = tempdir();
        let fs = FsStorage::new(&tmp).unwrap();
        let m = br#"{"schemaVersion":2}"#;
        fs.manifest_put("library/alpine", &Reference::Tag("v1".into()), m)
            .await
            .unwrap();
        fs.manifest_put("library/alpine", &Reference::Tag("v2".into()), m)
            .await
            .unwrap();
        fs.manifest_put("test/busybox", &Reference::Tag("latest".into()), m)
            .await
            .unwrap();

        let d = Digest {
            algorithm: Algorithm::Sha256,
            hex: hex::encode(Sha256::digest(b"x")),
        };
        fs.blob_write(&d, b"x").await.unwrap();

        let repos = fs.list_repos().await.unwrap();
        assert_eq!(repos, vec!["library/alpine", "test/busybox"]);

        let tags = fs.list_tags("library/alpine").await.unwrap();
        assert_eq!(tags, vec!["v1", "v2"]);

        let blobs = fs.list_all_blobs().await.unwrap();
        assert_eq!(blobs, vec![d]);
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
