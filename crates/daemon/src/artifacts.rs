//! Content-addressed artifact store (STEP 1.4).
//!
//! Blobs are deduplicated by SHA-256 while every `put` records its own
//! per-occurrence metadata row. The two identifiers are deliberately
//! independent: identical bytes resolve to a single stored file (keyed by
//! `sha256` alone), but each occurrence is its own [`ArtifactRef`] with its own
//! [`ArtifactId`], classification, and [`Provenance`]. Classification checks
//! (model routing, export, display) always read the row of the specific ref in
//! hand — never a row looked up by hash — so the same bytes seen first as
//! `Internal` and later as `Secret` are two rows sharing one blob, and the
//! second never inherits the first's lower classification (RULE 1).
//!
//! Writes are atomic: bytes stream to `<root>/tmp/<uuid>` while being hashed,
//! then rename into place at `<root>/sha256/<xx>/<full-hex>`. A crash leaves
//! only `tmp/` garbage, which [`ArtifactStore::sweep_tmp`] removes on startup
//! (RULE 2).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use codypendent_protocol::{ArtifactId, ArtifactRef, DataClassification, RunId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

/// Where an artifact's bytes came from, recorded once per occurrence and stored
/// verbatim in the `provenance_json` column.
///
/// This is a daemon-local record (not a wire type). It round-trips through JSON
/// so it can be persisted and read back unchanged. The uniform `observed_at`
/// captures when the daemon first saw the bytes; `source` describes the origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    /// What produced the bytes.
    pub source: ProvenanceSource,
    /// When the daemon first observed the bytes.
    pub observed_at: DateTime<Utc>,
}

/// The origin of an artifact's bytes. Internally tagged so it can grow new
/// variants without breaking stored rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProvenanceSource {
    /// Output captured from a tool invocation during a run.
    ToolOutput {
        /// The tool that produced the bytes, e.g. `shell.run`.
        tool: String,
        /// The run the tool executed within.
        run_id: RunId,
    },
    /// Content supplied directly by a human (an upload or paste).
    UserUpload,
    /// Bytes produced by the daemon itself (chronicles, snapshots, recovery
    /// exports); `detail` names the producing subsystem.
    System {
        /// A short description of the producing subsystem.
        detail: String,
    },
}

impl Provenance {
    /// Provenance for output captured from a tool during a run, observed now.
    pub fn tool_output(tool: impl Into<String>, run_id: RunId) -> Self {
        Self {
            source: ProvenanceSource::ToolOutput {
                tool: tool.into(),
                run_id,
            },
            observed_at: Utc::now(),
        }
    }

    /// Provenance for content supplied directly by a human, observed now.
    pub fn user_upload() -> Self {
        Self {
            source: ProvenanceSource::UserUpload,
            observed_at: Utc::now(),
        }
    }

    /// Provenance for bytes produced by the daemon itself, observed now.
    pub fn system(detail: impl Into<String>) -> Self {
        Self {
            source: ProvenanceSource::System {
                detail: detail.into(),
            },
            observed_at: Utc::now(),
        }
    }
}

/// A content-addressed blob store rooted at `<data_dir>/artifacts`.
///
/// Layout under `root`:
/// - `sha256/<xx>/<full-hex>` — the deduplicated blobs (`<xx>` = first two hex
///   chars of the digest, to fan the directory out).
/// - `tmp/<uuid>` — in-flight writes awaiting the atomic rename into `sha256/`.
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    /// Create a store rooted at `root` (`<data_dir>/artifacts`). The `sha256/`
    /// and `tmp/` subdirectories are created lazily on the first [`put`].
    ///
    /// [`put`]: ArtifactStore::put
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// On-disk path for a blob with the given lowercase-hex SHA-256.
    fn blob_path(&self, sha256: &str) -> PathBuf {
        self.root.join("sha256").join(&sha256[..2]).join(sha256)
    }

    /// Store `bytes` and record a fresh metadata row for this occurrence.
    ///
    /// Streams `bytes` to `<root>/tmp/<uuid>` while hashing, then atomically
    /// renames the temp file to its content-addressed path. If that blob
    /// already exists the write is skipped (the temp file is removed) but a new
    /// `artifacts` row is still inserted with its own [`ArtifactId`], the given
    /// `classification`, and `provenance`. Returns the [`ArtifactRef`] built
    /// from this row.
    pub async fn put(
        &self,
        pool: &SqlitePool,
        media_type: &str,
        classification: DataClassification,
        provenance: Provenance,
        bytes: &[u8],
    ) -> anyhow::Result<ArtifactRef> {
        // `bytes` is already fully in memory, so hash it directly — no disk write
        // is needed to learn the content address. A dedup hit therefore touches
        // no temp file at all.
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let sha256 = hex::encode(hasher.finalize());

        let blob_path = self.blob_path(&sha256);
        if !tokio::fs::try_exists(&blob_path).await? {
            // New blob: stage it in tmp/ with a single write + fsync, then
            // atomically rename into place. A crash leaves only tmp/ garbage.
            let tmp_dir = self.tmp_dir();
            tokio::fs::create_dir_all(&tmp_dir).await?;
            let tmp_path = tmp_dir.join(Uuid::now_v7().to_string());
            {
                let mut file = tokio::fs::File::create(&tmp_path).await?;
                file.write_all(bytes).await?;
                file.flush().await?;
                // Durability of the blob's bytes before we publish it by rename.
                file.sync_all().await?;
            }
            if let Some(parent) = blob_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            // Atomic publish. A concurrent put of the same new bytes may have
            // won the race; renaming over it replaces the file with identical
            // content, which is harmless.
            tokio::fs::rename(&tmp_path, &blob_path).await?;
        }

        // Per-occurrence metadata row: its own id, this classification and
        // provenance — never inherited from an earlier occurrence.
        let id = ArtifactId::new();
        let byte_length = bytes.len() as u64;
        let created_at = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO artifacts \
             (id, sha256, media_type, byte_length, classification, created_at, provenance_json) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id.to_string())
        .bind(&sha256)
        .bind(media_type)
        .bind(i64::try_from(byte_length)?)
        .bind(classification_to_str(classification))
        .bind(&created_at)
        .bind(serde_json::to_string(&provenance)?)
        .execute(pool)
        .await?;

        Ok(ArtifactRef {
            id,
            media_type: media_type.to_string(),
            byte_length,
            sha256,
            sensitivity: classification,
        })
    }

    /// Open the blob backing the artifact row `id` for reading.
    pub async fn open(&self, pool: &SqlitePool, id: ArtifactId) -> anyhow::Result<tokio::fs::File> {
        let sha256 = self.lookup_sha256(pool, id).await?;
        let path = self.blob_path(&sha256);
        let file = tokio::fs::File::open(&path).await?;
        Ok(file)
    }

    /// Re-hash the blob backing `id` and report whether it matches the SHA-256
    /// recorded in the artifact row (integrity check).
    pub async fn verify(&self, pool: &SqlitePool, id: ArtifactId) -> anyhow::Result<bool> {
        let stored = self.lookup_sha256(pool, id).await?;
        let actual = hash_file(&self.blob_path(&stored)).await?;
        Ok(actual == stored)
    }

    /// Delete leftover files under `<root>/tmp/` — crash garbage from writes
    /// that never reached their atomic rename. Called on startup. A missing
    /// `tmp/` directory is not an error.
    pub async fn sweep_tmp(&self) -> anyhow::Result<()> {
        let tmp_dir = self.tmp_dir();
        let mut entries = match tokio::fs::read_dir(&tmp_dir).await {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if entry.file_type().await?.is_dir() {
                tokio::fs::remove_dir_all(&path).await?;
            } else {
                tokio::fs::remove_file(&path).await?;
            }
        }
        Ok(())
    }

    /// Resolve the stored SHA-256 for an artifact row by id.
    async fn lookup_sha256(&self, pool: &SqlitePool, id: ArtifactId) -> anyhow::Result<String> {
        let row: Option<(String,)> = sqlx::query_as("SELECT sha256 FROM artifacts WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(pool)
            .await?;
        row.map(|(sha256,)| sha256)
            .ok_or_else(|| anyhow::anyhow!("no artifact row for id {id}"))
    }
}

/// The lowercase-hex SHA-256 of a file's contents, hashed by streaming so large
/// blobs are never held in memory whole.
async fn hash_file(path: &Path) -> anyhow::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Encode a [`DataClassification`] as the plain tag stored in the `artifacts`
/// row's `classification` column. Any future (or `Unknown`) variant is stored
/// as `"Unknown"`, matching the protocol's "treat unknown as most restrictive"
/// rule.
fn classification_to_str(classification: DataClassification) -> &'static str {
    match classification {
        DataClassification::Public => "Public",
        DataClassification::Internal => "Internal",
        DataClassification::Confidential => "Confidential",
        DataClassification::Secret => "Secret",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn test_pool(dir: &Path) -> SqlitePool {
        crate::db::open_database(&dir.join("test.db"))
            .await
            .expect("open database")
    }

    /// Count regular files anywhere under `root` (recursively).
    fn count_files(root: &Path) -> usize {
        let mut total = 0;
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    total += count_files(&path);
                } else {
                    total += 1;
                }
            }
        }
        total
    }

    #[tokio::test]
    async fn put_open_round_trip() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let store = ArtifactStore::new(dir.path().join("artifacts"));

        let bytes = b"hello content-addressed world";
        let reference = store
            .put(
                &pool,
                "text/plain",
                DataClassification::Internal,
                Provenance::user_upload(),
                bytes,
            )
            .await
            .unwrap();
        assert_eq!(reference.byte_length, bytes.len() as u64);

        let mut file = store.open(&pool, reference.id).await.unwrap();
        let mut read_back = Vec::new();
        file.read_to_end(&mut read_back).await.unwrap();
        assert_eq!(read_back, bytes);
    }

    #[tokio::test]
    async fn verify_returns_true_for_intact_blob() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let store = ArtifactStore::new(dir.path().join("artifacts"));

        let reference = store
            .put(
                &pool,
                "application/octet-stream",
                DataClassification::Confidential,
                Provenance::system("unit-test"),
                b"bytes to verify",
            )
            .await
            .unwrap();

        assert!(store.verify(&pool, reference.id).await.unwrap());
    }

    #[tokio::test]
    async fn dedup_blob_but_per_occurrence_rows() {
        let dir = tempdir().unwrap();
        let pool = test_pool(dir.path()).await;
        let store = ArtifactStore::new(dir.path().join("artifacts"));

        let bytes = b"identical bytes seen twice";
        let internal = store
            .put(
                &pool,
                "text/plain",
                DataClassification::Internal,
                Provenance::system("first"),
                bytes,
            )
            .await
            .unwrap();
        let secret = store
            .put(
                &pool,
                "text/plain",
                DataClassification::Secret,
                Provenance::system("second"),
                bytes,
            )
            .await
            .unwrap();

        // (a) exactly one blob file on disk under sha256/.
        let blob_root = dir.path().join("artifacts").join("sha256");
        assert_eq!(count_files(&blob_root), 1);

        // (b) two distinct rows, same hash, different ids.
        assert_eq!(internal.sha256, secret.sha256);
        assert_ne!(internal.id, secret.id);
        let (rows,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM artifacts WHERE sha256 = ?")
            .bind(&internal.sha256)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 2);

        // (c) the second ref keeps Secret; it did NOT inherit Internal — on the
        //     ref, in the returned value, and as stored in its own row.
        assert_eq!(internal.sensitivity, DataClassification::Internal);
        assert_eq!(secret.sensitivity, DataClassification::Secret);
        let (stored_class,): (String,) =
            sqlx::query_as("SELECT classification FROM artifacts WHERE id = ?")
                .bind(secret.id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored_class, "Secret");
    }

    #[tokio::test]
    async fn sweep_tmp_removes_stray_files() {
        let dir = tempdir().unwrap();
        let store = ArtifactStore::new(dir.path().join("artifacts"));

        let tmp_dir = dir.path().join("artifacts").join("tmp");
        tokio::fs::create_dir_all(&tmp_dir).await.unwrap();
        let stray = tmp_dir.join("leftover-from-crash");
        tokio::fs::write(&stray, b"garbage").await.unwrap();
        assert!(stray.exists());

        store.sweep_tmp().await.unwrap();

        assert!(!stray.exists());
        // The tmp directory itself is retained (empty).
        assert!(tmp_dir.exists());
    }

    #[test]
    fn provenance_round_trips_through_json() {
        let cases = [
            Provenance::tool_output("shell.run", RunId::new()),
            Provenance::user_upload(),
            Provenance::system("chronicle-export"),
        ];
        for original in cases {
            let json = serde_json::to_string(&original).unwrap();
            let parsed: Provenance = serde_json::from_str(&json).unwrap();
            assert_eq!(original, parsed);
        }
    }
}
