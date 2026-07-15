//! Content-addressed artifact store (STEP 1.4).
//!
//! Populated by STEP 1.4: blobs are deduplicated by SHA-256 while every `put`
//! records its own per-occurrence metadata row.
