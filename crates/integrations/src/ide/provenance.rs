//! Source-provenance resolution (Chapter 10, exit criterion 2).
//!
//! Every file excerpt entering model context carries exactly one
//! [`SourceProvenance`] label so a client can always answer "where did this text
//! come from?". [`resolve_source`] applies a fixed precedence: an unsaved editor
//! buffer that diverges from disk is the current truth and wins; otherwise the
//! agent's worktree, then the working tree on disk, then a committed revision.

use codypendent_protocol::ide::{DirtyBufferDigest, SourceProvenance};
use sha2::{Digest, Sha256};

/// The lowercase hex SHA-256 of `bytes`, matching the digest format the IDE
/// sends in a [`DirtyBufferDigest`].
pub fn digest_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Resolve the provenance of the excerpt for `path`.
///
/// Precedence:
/// 1. A dirty buffer exists for `path` and its digest differs from the
///    filesystem digest (or there is no filesystem digest) →
///    [`SourceProvenance::UnsavedIdeBuffer`]: the unsaved buffer is the current
///    truth and diverges from disk.
/// 2. Otherwise, if the file lives in the agent's worktree →
///    [`SourceProvenance::AgentWorktree`].
/// 3. Otherwise, if a filesystem digest is known →
///    [`SourceProvenance::Filesystem`].
/// 4. Otherwise, if a committed revision is known →
///    [`SourceProvenance::CommittedAt`].
/// 5. Otherwise → [`SourceProvenance::Filesystem`].
///
/// A dirty buffer whose digest *equals* the filesystem digest is not a
/// divergence and falls through to the later cases.
pub fn resolve_source(
    path: &str,
    dirty_buffers: &[DirtyBufferDigest],
    filesystem_digest: Option<&str>,
    committed_revision: Option<&str>,
    in_agent_worktree: bool,
) -> SourceProvenance {
    if let Some(buffer) = dirty_buffers.iter().find(|buffer| buffer.path == path) {
        // The IDE-supplied digest is untrusted text: normalize (trim + lowercase)
        // before comparing so an editor that reports uppercase or padded hex does
        // not make identical content read as a divergence.
        let diverges = match filesystem_digest {
            Some(fs_digest) => !buffer.sha256.trim().eq_ignore_ascii_case(fs_digest.trim()),
            None => true,
        };
        if diverges {
            return SourceProvenance::UnsavedIdeBuffer;
        }
    }

    if in_agent_worktree {
        return SourceProvenance::AgentWorktree;
    }

    if filesystem_digest.is_some() {
        return SourceProvenance::Filesystem;
    }

    if let Some(revision) = committed_revision {
        return SourceProvenance::CommittedAt {
            revision: revision.to_string(),
        };
    }

    SourceProvenance::Filesystem
}

/// Convenience wrapper for [`SourceProvenance::label`].
pub fn label_for(prov: &SourceProvenance) -> String {
    prov.label()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirty(path: &str, sha: &str) -> DirtyBufferDigest {
        DirtyBufferDigest {
            path: path.to_string(),
            sha256: sha.to_string(),
            byte_length: 0,
        }
    }

    #[test]
    fn digest_is_lowercase_hex_sha256() {
        // Known SHA-256 of the empty input.
        assert_eq!(
            digest_bytes(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn dirty_buffer_diverging_from_fs_is_unsaved() {
        let buffers = [dirty("src/lib.rs", "AAAA")];
        let prov = resolve_source("src/lib.rs", &buffers, Some("BBBB"), None, false);
        assert_eq!(prov, SourceProvenance::UnsavedIdeBuffer);
        assert_eq!(prov.label(), "unsaved-ide-buffer");
    }

    #[test]
    fn dirty_buffer_without_fs_digest_is_unsaved() {
        let buffers = [dirty("src/lib.rs", "AAAA")];
        let prov = resolve_source("src/lib.rs", &buffers, None, None, false);
        assert_eq!(prov, SourceProvenance::UnsavedIdeBuffer);
    }

    #[test]
    fn dirty_buffer_equal_to_fs_is_filesystem() {
        let buffers = [dirty("src/lib.rs", "AAAA")];
        let prov = resolve_source("src/lib.rs", &buffers, Some("AAAA"), None, false);
        assert_eq!(prov, SourceProvenance::Filesystem);
    }

    #[test]
    fn worktree_file_is_agent_worktree() {
        let prov = resolve_source("src/lib.rs", &[], None, Some("a1b2c3d"), true);
        assert_eq!(prov, SourceProvenance::AgentWorktree);
    }

    #[test]
    fn filesystem_digest_only_is_filesystem() {
        let prov = resolve_source("src/lib.rs", &[], Some("BBBB"), None, false);
        assert_eq!(prov, SourceProvenance::Filesystem);
    }

    #[test]
    fn committed_only_is_committed_at() {
        let prov = resolve_source("src/lib.rs", &[], None, Some("a1b2c3d"), false);
        assert_eq!(
            prov,
            SourceProvenance::CommittedAt {
                revision: "a1b2c3d".to_string()
            }
        );
        assert_eq!(label_for(&prov), "committed@a1b2c3d");
    }

    #[test]
    fn nothing_known_falls_back_to_filesystem() {
        let prov = resolve_source("src/lib.rs", &[], None, None, false);
        assert_eq!(prov, SourceProvenance::Filesystem);
    }
}
