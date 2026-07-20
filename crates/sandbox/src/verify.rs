//! Artifact verification (STEP 6.1): checksum and publisher signature.
//!
//! Before a plugin is ever installed (even disabled), its artifact bytes are
//! matched against the manifest's `sha256:` checksum, and — when the manifest is
//! signed — the checksum is verified against the publisher's ed25519 key. An
//! *unsigned* plugin follows policy `[plugins].unsigned`, which defaults to
//! **deny**: a plugin with no signature is refused unless the operator has
//! explicitly opted into allowing unsigned plugins.
//!
//! The checksum binds the manifest to a specific artifact; the signature binds
//! that checksum to a publisher. Verifying the signature over the checksum (not
//! re-hashing the artifact under the key) keeps the two concerns separable and
//! matches how the manifest stores them.

use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::manifest::PluginManifest;

/// Policy for plugins whose manifest carries no signature (`[plugins].unsigned`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnsignedPolicy {
    /// Refuse unsigned plugins. The default posture (STEP 6.1).
    #[default]
    Deny,
    /// Allow unsigned plugins (operator opt-in; development installs).
    Allow,
}

/// Why verification failed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum VerifyError {
    /// The manifest's checksum is not the `sha256:<hex>` shape.
    #[error("malformed checksum (expected `sha256:<hex>`): {0}")]
    MalformedChecksum(String),
    /// The artifact bytes do not hash to the manifest's checksum.
    #[error("checksum mismatch: manifest {expected}, artifact {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    /// The plugin is unsigned and policy denies unsigned plugins.
    #[error("plugin is unsigned and policy denies unsigned plugins")]
    UnsignedDenied,
    /// The publisher key could not be decoded.
    #[error("invalid publisher key: {0}")]
    InvalidPublisherKey(String),
    /// The signature could not be decoded.
    #[error("invalid signature encoding: {0}")]
    InvalidSignature(String),
    /// The signature did not verify against the publisher key.
    #[error("signature does not verify against the publisher key")]
    SignatureMismatch,
}

/// The outcome of verifying an artifact — the trust facts a lifecycle records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verified {
    /// Whether a real signature was present and verified.
    pub signed: bool,
}

/// Compute the `sha256:<hex>` checksum of some artifact bytes, in the manifest's
/// canonical form.
#[must_use]
pub fn checksum_of(artifact: &[u8]) -> String {
    let digest = Sha256::digest(artifact);
    format!("sha256:{}", hex::encode(digest))
}

/// Verify a plugin artifact against its manifest under the given unsigned policy
/// and optional publisher key.
///
/// * The artifact must hash to the manifest's checksum.
/// * If the manifest is signed, `publisher_key` (32 raw ed25519 public-key bytes)
///   must verify the signature over the **checksum string**.
/// * If the manifest is unsigned, [`UnsignedPolicy`] decides.
///
/// The publisher key is supplied by the caller (the daemon resolves it from the
/// trusted-publisher store keyed by `manifest.publisher`); passing `None` for a
/// signed manifest is treated as an invalid key.
pub fn verify_artifact(
    manifest: &PluginManifest,
    artifact: &[u8],
    publisher_key: Option<&[u8]>,
    unsigned: UnsignedPolicy,
) -> Result<Verified, VerifyError> {
    // 1. Checksum shape + match. The checksum binds manifest → artifact and is
    //    checked first: a signature over a checksum means nothing if the checksum
    //    does not describe the bytes in hand.
    let expected = manifest.security.checksum.trim();
    let expected_hex = expected
        .strip_prefix("sha256:")
        .ok_or_else(|| VerifyError::MalformedChecksum(expected.to_string()))?;
    if expected_hex.is_empty() || hex::decode(expected_hex).is_err() {
        return Err(VerifyError::MalformedChecksum(expected.to_string()));
    }
    let actual = checksum_of(artifact);
    if !expected.eq_ignore_ascii_case(&actual) {
        return Err(VerifyError::ChecksumMismatch {
            expected: expected.to_string(),
            actual,
        });
    }

    // 2. Signature (or unsigned policy).
    if !manifest.security.is_signed() {
        return match unsigned {
            UnsignedPolicy::Allow => Ok(Verified { signed: false }),
            UnsignedPolicy::Deny => Err(VerifyError::UnsignedDenied),
        };
    }

    let key_bytes = publisher_key.ok_or_else(|| {
        VerifyError::InvalidPublisherKey("no publisher key supplied for a signed plugin".into())
    })?;
    let key_arr: [u8; 32] = key_bytes.try_into().map_err(|_| {
        VerifyError::InvalidPublisherKey(format!("expected 32 bytes, got {}", key_bytes.len()))
    })?;
    let key = VerifyingKey::from_bytes(&key_arr)
        .map_err(|e| VerifyError::InvalidPublisherKey(e.to_string()))?;

    let sig_bytes = base64::engine::general_purpose::STANDARD
        .decode(manifest.security.signature.trim())
        .map_err(|e| VerifyError::InvalidSignature(e.to_string()))?;
    let sig_arr: [u8; 64] = sig_bytes.as_slice().try_into().map_err(|_| {
        VerifyError::InvalidSignature(format!("expected 64 bytes, got {}", sig_bytes.len()))
    })?;
    let signature = Signature::from_bytes(&sig_arr);

    // The signed message is the canonical checksum string — publishing a plugin
    // signs the artifact identity, and the checksum is that identity.
    key.verify_strict(actual.as_bytes(), &signature)
        .map_err(|_| VerifyError::SignatureMismatch)?;
    Ok(Verified { signed: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::parse_manifest;
    use ed25519_dalek::{Signer, SigningKey};

    fn manifest_with(checksum: &str, signature: &str) -> PluginManifest {
        let toml = format!(
            r#"
schema_version = 1
id = "wc"
name = "Word Count"
version = "0.1.0"
kind = "wasm-component"
publisher = "me"
[runtime]
command = "word_count.wasm"
[security]
checksum = "{checksum}"
signature = "{signature}"
"#
        );
        parse_manifest(&toml).expect("manifest parses")
    }

    #[test]
    fn checksum_matches_are_accepted_when_unsigned_allowed() {
        let artifact = b"plugin bytes";
        let m = manifest_with(&checksum_of(artifact), "set-during-packaging");
        let v = verify_artifact(&m, artifact, None, UnsignedPolicy::Allow).expect("verifies");
        assert!(!v.signed);
    }

    #[test]
    fn checksum_mismatch_is_rejected() {
        let m = manifest_with(&checksum_of(b"the real bytes"), "set-during-packaging");
        let err = verify_artifact(&m, b"tampered bytes", None, UnsignedPolicy::Allow).unwrap_err();
        assert!(matches!(err, VerifyError::ChecksumMismatch { .. }));
    }

    #[test]
    fn malformed_checksum_is_rejected() {
        let m = manifest_with("md5:whatever", "set-during-packaging");
        let err = verify_artifact(&m, b"x", None, UnsignedPolicy::Allow).unwrap_err();
        assert!(matches!(err, VerifyError::MalformedChecksum(_)));
    }

    #[test]
    fn unsigned_is_denied_by_default_policy() {
        let artifact = b"plugin bytes";
        let m = manifest_with(&checksum_of(artifact), "set-during-packaging");
        let err = verify_artifact(&m, artifact, None, UnsignedPolicy::Deny).unwrap_err();
        assert_eq!(err, VerifyError::UnsignedDenied);
    }

    #[test]
    fn valid_signature_verifies() {
        let artifact = b"signed plugin bytes";
        let checksum = checksum_of(artifact);
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let sig = signing.sign(checksum.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
        let m = manifest_with(&checksum, &sig_b64);
        let key = signing.verifying_key();
        let v = verify_artifact(&m, artifact, Some(key.as_bytes()), UnsignedPolicy::Deny)
            .expect("signature verifies");
        assert!(v.signed);
    }

    #[test]
    fn signature_from_the_wrong_key_is_rejected() {
        let artifact = b"signed plugin bytes";
        let checksum = checksum_of(artifact);
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let sig = signing.sign(checksum.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
        let m = manifest_with(&checksum, &sig_b64);
        // A different key must not verify this signature.
        let attacker = SigningKey::from_bytes(&[9u8; 32]).verifying_key();
        let err = verify_artifact(
            &m,
            artifact,
            Some(attacker.as_bytes()),
            UnsignedPolicy::Deny,
        )
        .unwrap_err();
        assert_eq!(err, VerifyError::SignatureMismatch);
    }

    #[test]
    fn signature_over_a_tampered_artifact_is_rejected_at_checksum() {
        // A signed checksum for the real bytes, but the artifact shipped is
        // different: checksum fails before signature is even considered.
        let checksum = checksum_of(b"the real bytes");
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let sig = signing.sign(checksum.as_bytes());
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
        let m = manifest_with(&checksum, &sig_b64);
        let key = signing.verifying_key();
        let err = verify_artifact(&m, b"tampered", Some(key.as_bytes()), UnsignedPolicy::Deny)
            .unwrap_err();
        assert!(matches!(err, VerifyError::ChecksumMismatch { .. }));
    }
}
