//! The trusted-publisher key store (STEP 6.2 — verification gets real keys).
//!
//! [`verify_artifact`](crate::verify::verify_artifact) checks a plugin's signature
//! against a publisher's ed25519 public key, but it takes that key as an argument —
//! it has no notion of *which* publishers are trusted. This module is that missing
//! piece: a **data-only** map from a publisher id to its 32-byte ed25519 public
//! key, persisted as a TOML config file under the data/config dir (the
//! `models.toml` precedent — no database, no migration).
//!
//! The store is the resolver a caller threads into verification:
//! [`key_for`](TrustedPublishers::key_for) returns the key for a publisher, or
//! `None`. A signed plugin whose publisher is **not** in the store resolves to no
//! key, so `verify_artifact` fails closed
//! ([`InvalidPublisherKey`](crate::verify::VerifyError::InvalidPublisherKey)) — an
//! unknown publisher can never be installed. An unsigned plugin still falls under
//! the default-**deny** unsigned policy. A **missing** store file is an *empty*
//! store, not an error: with no trusted keys, every signed plugin fails closed —
//! the safe default.

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine;
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};

/// The number of raw bytes in an ed25519 public key.
const ED25519_PUBLIC_KEY_LEN: usize = 32;

/// One `[[publisher]]` entry in the store file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PublisherEntry {
    /// The publisher id (matched against `manifest.publisher`).
    id: String,
    /// Base64 (standard) of the 32 raw ed25519 public-key bytes.
    public_key: String,
}

/// The on-disk shape of the store: a bare array of `[[publisher]]` tables, the
/// same layout convention as `models.toml`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    #[serde(default, rename = "publisher")]
    publisher: Vec<PublisherEntry>,
}

/// A failure loading, saving, or mutating the trusted-publisher store.
#[derive(Debug, thiserror::Error)]
pub enum TrustStoreError {
    /// The store file could not be read.
    #[error("reading trusted-publisher store {path}: {source}")]
    Read {
        /// The store path.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The store file could not be written.
    #[error("writing trusted-publisher store {path}: {source}")]
    Write {
        /// The store path.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The store file is not valid TOML / the expected shape.
    #[error("parsing trusted-publisher store {path}: {source}")]
    Parse {
        /// The store path.
        path: String,
        /// The underlying parse error.
        #[source]
        source: toml::de::Error,
    },
    /// The store could not be serialized to TOML.
    #[error("serializing trusted-publisher store: {0}")]
    Serialize(#[source] toml::ser::Error),
    /// A publisher's public key is not a valid base64 32-byte ed25519 key.
    #[error("invalid public key for publisher `{id}`: {reason}")]
    InvalidKey {
        /// The offending publisher id.
        id: String,
        /// Why the key was rejected.
        reason: String,
    },
    /// A publisher id was empty.
    #[error("publisher id must not be empty")]
    EmptyId,
    /// A publisher id is already present (adds do not silently overwrite a key).
    #[error("publisher `{0}` is already trusted; remove it first to replace its key")]
    Duplicate(String),
}

/// An in-memory trusted-publisher key store: publisher id → ed25519 public key.
#[derive(Debug, Clone, Default)]
pub struct TrustedPublishers {
    keys: BTreeMap<String, [u8; ED25519_PUBLIC_KEY_LEN]>,
}

impl TrustedPublishers {
    /// An empty store — trusts no publisher (every signed plugin fails closed).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Load the store from `path`. A **missing file is an empty store** (not an
    /// error): with no trusted keys, every signed plugin fails closed.
    pub fn load(path: &Path) -> Result<Self, TrustStoreError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(source) => {
                return Err(TrustStoreError::Read {
                    path: path.display().to_string(),
                    source,
                })
            }
        };
        let file: TrustFile = toml::from_str(&text).map_err(|source| TrustStoreError::Parse {
            path: path.display().to_string(),
            source,
        })?;
        let mut keys = BTreeMap::new();
        for entry in file.publisher {
            let key = decode_key(&entry.id, &entry.public_key)?;
            keys.insert(entry.id, key);
        }
        Ok(Self { keys })
    }

    /// Persist the store to `path`, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> Result<(), TrustStoreError> {
        let file = TrustFile {
            publisher: self
                .keys
                .iter()
                .map(|(id, key)| PublisherEntry {
                    id: id.clone(),
                    public_key: base64::engine::general_purpose::STANDARD.encode(key),
                })
                .collect(),
        };
        let text = toml::to_string_pretty(&file).map_err(TrustStoreError::Serialize)?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|source| TrustStoreError::Write {
                    path: path.display().to_string(),
                    source,
                })?;
            }
        }
        std::fs::write(path, text).map_err(|source| TrustStoreError::Write {
            path: path.display().to_string(),
            source,
        })
    }

    /// Trust a publisher: add its id and base64 ed25519 public key. The key is
    /// decoded and validated as a real curve point; an existing id is **not**
    /// overwritten (a key rotation is an explicit remove-then-add).
    pub fn add(
        &mut self,
        id: impl Into<String>,
        public_key_b64: &str,
    ) -> Result<(), TrustStoreError> {
        let id = id.into();
        if id.trim().is_empty() {
            return Err(TrustStoreError::EmptyId);
        }
        if self.keys.contains_key(&id) {
            return Err(TrustStoreError::Duplicate(id));
        }
        let key = decode_key(&id, public_key_b64)?;
        self.keys.insert(id, key);
        Ok(())
    }

    /// Stop trusting a publisher. Returns whether it was present.
    pub fn remove(&mut self, id: &str) -> bool {
        self.keys.remove(id).is_some()
    }

    /// The raw 32-byte public key for a publisher, if trusted — the resolver
    /// [`verify_artifact`](crate::verify::verify_artifact) consumes (pass
    /// `store.key_for(&manifest.publisher).map(|k| k.as_slice())`). `None` for an
    /// unknown publisher, which makes verification of a signed plugin fail closed.
    #[must_use]
    pub fn key_for(&self, id: &str) -> Option<&[u8; ED25519_PUBLIC_KEY_LEN]> {
        self.keys.get(id)
    }

    /// Whether a publisher is trusted.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.keys.contains_key(id)
    }

    /// Whether the store trusts no publishers.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The number of trusted publishers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Iterate `(id, base64-public-key)` in stable (sorted) id order — for display.
    pub fn list(&self) -> impl Iterator<Item = (&str, String)> {
        self.keys.iter().map(|(id, key)| {
            (
                id.as_str(),
                base64::engine::general_purpose::STANDARD.encode(key),
            )
        })
    }
}

/// Decode and validate a base64-encoded 32-byte ed25519 public key.
fn decode_key(id: &str, b64: &str) -> Result<[u8; ED25519_PUBLIC_KEY_LEN], TrustStoreError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|error| TrustStoreError::InvalidKey {
            id: id.to_string(),
            reason: format!("not valid base64: {error}"),
        })?;
    let key: [u8; ED25519_PUBLIC_KEY_LEN] =
        bytes
            .as_slice()
            .try_into()
            .map_err(|_| TrustStoreError::InvalidKey {
                id: id.to_string(),
                reason: format!(
                    "expected {ED25519_PUBLIC_KEY_LEN} bytes, got {}",
                    bytes.len()
                ),
            })?;
    // Reject a structurally invalid key (not a valid curve point) at add/load time,
    // so a malformed key can never sit in the store waiting to fail a verification.
    VerifyingKey::from_bytes(&key).map_err(|error| TrustStoreError::InvalidKey {
        id: id.to_string(),
        reason: error.to_string(),
    })?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn key_b64(seed: u8) -> String {
        let signing = SigningKey::from_bytes(&[seed; 32]);
        base64::engine::general_purpose::STANDARD.encode(signing.verifying_key().as_bytes())
    }

    #[test]
    fn add_list_remove_round_trip() {
        let mut store = TrustedPublishers::new();
        assert!(store.is_empty());
        store.add("codypendent-project", &key_b64(7)).unwrap();
        assert!(store.contains("codypendent-project"));
        assert_eq!(store.len(), 1);

        let listed: Vec<_> = store.list().collect();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].0, "codypendent-project");
        assert_eq!(listed[0].1, key_b64(7));

        assert!(store.remove("codypendent-project"));
        assert!(!store.remove("codypendent-project"));
        assert!(store.is_empty());
    }

    #[test]
    fn unknown_publisher_resolves_to_no_key() {
        let store = TrustedPublishers::new();
        // The fail-closed hook: an unknown publisher yields no key, so a signed
        // plugin's verification is refused.
        assert!(store.key_for("nobody").is_none());
    }

    #[test]
    fn key_for_returns_the_stored_key() {
        let mut store = TrustedPublishers::new();
        store.add("pub", &key_b64(3)).unwrap();
        let expected = SigningKey::from_bytes(&[3; 32]);
        assert_eq!(
            store.key_for("pub").unwrap(),
            expected.verifying_key().as_bytes()
        );
    }

    #[test]
    fn adding_a_duplicate_is_refused() {
        let mut store = TrustedPublishers::new();
        store.add("pub", &key_b64(1)).unwrap();
        let err = store.add("pub", &key_b64(2)).unwrap_err();
        assert!(matches!(err, TrustStoreError::Duplicate(_)));
        // The original key (seed 1) is untouched — no silent overwrite by seed 2.
        let original = SigningKey::from_bytes(&[1; 32]);
        assert_eq!(
            store.key_for("pub").unwrap(),
            original.verifying_key().as_bytes()
        );
    }

    #[test]
    fn empty_id_is_refused() {
        let mut store = TrustedPublishers::new();
        assert!(matches!(
            store.add("   ", &key_b64(1)),
            Err(TrustStoreError::EmptyId)
        ));
    }

    #[test]
    fn invalid_base64_key_is_refused() {
        let mut store = TrustedPublishers::new();
        let err = store.add("pub", "not base64!!!").unwrap_err();
        assert!(matches!(err, TrustStoreError::InvalidKey { .. }));
    }

    #[test]
    fn wrong_length_key_is_refused() {
        let mut store = TrustedPublishers::new();
        let short = base64::engine::general_purpose::STANDARD.encode([1u8; 16]);
        let err = store.add("pub", &short).unwrap_err();
        assert!(
            matches!(err, TrustStoreError::InvalidKey { reason, .. } if reason.contains("32 bytes"))
        );
    }

    #[test]
    fn save_then_load_is_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("trusted_publishers.toml");
        let mut store = TrustedPublishers::new();
        store.add("a", &key_b64(1)).unwrap();
        store.add("b", &key_b64(2)).unwrap();
        store.save(&path).unwrap();

        let reloaded = TrustedPublishers::load(&path).unwrap();
        assert_eq!(reloaded.len(), 2);
        assert_eq!(reloaded.key_for("a"), store.key_for("a"));
        assert_eq!(reloaded.key_for("b"), store.key_for("b"));
    }

    #[test]
    fn a_missing_file_is_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.toml");
        let store = TrustedPublishers::load(&missing).unwrap();
        assert!(store.is_empty());
    }

    #[test]
    fn a_corrupt_store_file_is_an_error_not_a_silent_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trusted_publishers.toml");
        std::fs::write(&path, "this is not valid toml [[[").unwrap();
        assert!(matches!(
            TrustedPublishers::load(&path),
            Err(TrustStoreError::Parse { .. })
        ));
    }

    #[test]
    fn loading_a_store_with_a_bad_key_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trusted_publishers.toml");
        std::fs::write(
            &path,
            "[[publisher]]\nid = \"pub\"\npublic_key = \"AAAA\"\n",
        )
        .unwrap();
        assert!(matches!(
            TrustedPublishers::load(&path),
            Err(TrustStoreError::InvalidKey { .. })
        ));
    }
}
