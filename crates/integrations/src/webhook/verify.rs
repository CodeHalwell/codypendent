//! GitHub webhook signature verification (`X-Hub-Signature-256`).
//!
//! GitHub signs every webhook body with HMAC-SHA256 keyed by the shared secret
//! and sends the result as an `X-Hub-Signature-256: sha256=<hex>` header. The
//! signature is verified against the *raw* bytes **before** the body is parsed,
//! so a forged payload never reaches the JSON deserializer. The comparison is
//! constant-time (via [`Mac::verify_slice`]) to avoid a timing oracle.

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// HMAC-SHA256, the algorithm named by the `sha256=` prefix.
type HmacSha256 = Hmac<Sha256>;

/// Verify a GitHub `X-Hub-Signature-256` header against `body`.
///
/// The header must be of the form `sha256=<hex>`. Returns `false` on a missing
/// prefix, un-decodable hex, or any signature mismatch — never panicking and
/// never leaking timing information about where a mismatch occurred.
pub fn verify_signature(secret: &[u8], body: &[u8], signature_header: &str) -> bool {
    let Some(hex_signature) = signature_header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = hex::decode(hex_signature) else {
        return false;
    };
    let mut mac = match HmacSha256::new_from_slice(secret) {
        Ok(mac) => mac,
        Err(_) => return false,
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

/// Compute the `sha256=<hex>` signature for `body` under `secret`.
///
/// This is the inverse of [`verify_signature`] and exists chiefly so tests can
/// craft valid signatures. HMAC accepts a key of any length, so construction is
/// infallible.
pub fn sign(secret: &[u8], body: &[u8]) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret).expect("HMAC-SHA256 accepts a key of any length");
    mac.update(body);
    let bytes = mac.finalize().into_bytes();
    format!("sha256={}", hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"It's a Secret to Everybody";
    const BODY: &[u8] = b"Hello, World!";

    #[test]
    fn valid_signature_verifies() {
        let signature = sign(SECRET, BODY);
        assert!(verify_signature(SECRET, BODY, &signature));
    }

    #[test]
    fn tampered_body_fails() {
        let signature = sign(SECRET, BODY);
        assert!(!verify_signature(SECRET, b"Goodbye, World!", &signature));
    }

    #[test]
    fn wrong_secret_fails() {
        let signature = sign(SECRET, BODY);
        assert!(!verify_signature(b"a different secret", BODY, &signature));
    }

    #[test]
    fn malformed_header_fails() {
        // Missing prefix.
        assert!(!verify_signature(SECRET, BODY, "deadbeef"));
        // Prefix present but the hex is invalid.
        assert!(!verify_signature(SECRET, BODY, "sha256=zzzz"));
        // Empty header.
        assert!(!verify_signature(SECRET, BODY, ""));
    }
}
