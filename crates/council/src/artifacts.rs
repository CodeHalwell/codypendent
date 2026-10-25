//! Bounded, verified artifact transfers shared by daemon-backed clients.
use std::path::Path;

use anyhow::{bail, Context};
use async_trait::async_trait;
use base64::Engine as _;
use codypendent_protocol::{ArtifactRef, CommandBody, Payload};
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;

use crate::connection::Connection;

/// One bounded chunk, regardless of the artifact's total on-disk size.
pub const ARTIFACT_CHUNK_BYTES: u32 = 1024 * 1024;
const MAX_ENCODED_CHUNK_BYTES: usize = (ARTIFACT_CHUNK_BYTES as usize).div_ceil(3) * 4;

/// The transport owns command correlation, deadlines and connection recovery.
#[async_trait]
pub trait ArtifactSource: Send {
    async fn read_chunk(&mut self, command: CommandBody) -> anyhow::Result<Payload>;
}

#[async_trait]
impl ArtifactSource for Connection {
    async fn read_chunk(&mut self, command: CommandBody) -> anyhow::Result<Payload> {
        Ok(self.send_command(command).await?.payload)
    }
}

struct Transfer<'a> {
    artifact: &'a ArtifactRef,
    offset: u64,
    digest: Sha256,
    complete: bool,
}

impl<'a> Transfer<'a> {
    fn new(artifact: &'a ArtifactRef) -> anyhow::Result<Self> {
        if artifact.sha256.len() != 64 || !artifact.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("the artifact reference does not contain a valid SHA-256 digest");
        }
        Ok(Self {
            artifact,
            offset: 0,
            digest: Sha256::new(),
            complete: false,
        })
    }

    async fn next(
        &mut self,
        source: &mut (impl ArtifactSource + ?Sized),
    ) -> anyhow::Result<Vec<u8>> {
        let payload = source
            .read_chunk(CommandBody::ReadArtifact {
                artifact_id: self.artifact.id,
                offset: self.offset,
                limit: ARTIFACT_CHUNK_BYTES,
                expected_sha256: self.artifact.sha256.clone(),
            })
            .await?;
        let Payload::ArtifactChunk {
            artifact_id,
            offset,
            bytes_base64,
            eof,
            sha256,
        } = payload
        else {
            if let Payload::CommandRejected(error) = payload {
                bail!("ReadArtifact rejected: {} ({})", error.message, error.code);
            }
            bail!("unexpected reply to ReadArtifact");
        };
        if artifact_id != self.artifact.id
            || offset != self.offset
            || sha256 != self.artifact.sha256
        {
            bail!("the artifact chunk does not match the requested identity, offset or digest");
        }
        // Bound the encoded form BEFORE allocating decoded storage. A matching
        // content address alone does not license an oversized chunk allocation.
        if bytes_base64.len() > MAX_ENCODED_CHUNK_BYTES {
            bail!("the artifact chunk exceeds the requested byte limit");
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(bytes_base64)
            .context("decoding an artifact chunk")?;
        if bytes.len() > ARTIFACT_CHUNK_BYTES as usize {
            bail!("the artifact chunk exceeds the requested byte limit");
        }
        let length = self
            .offset
            .checked_add(bytes.len() as u64)
            .context("artifact length overflow")?;
        if length > self.artifact.byte_length {
            bail!(
                "the artifact read ran past its declared length of {} bytes",
                self.artifact.byte_length
            );
        }
        if eof && length != self.artifact.byte_length {
            bail!(
                "the artifact read returned {length} bytes but the reference declares {}",
                self.artifact.byte_length
            );
        }
        if !eof && bytes.is_empty() {
            bail!("the artifact read made no progress before end of file");
        }
        self.digest.update(&bytes);
        self.offset = length;
        if eof {
            let actual = format!("{:x}", self.digest.clone().finalize());
            if actual != self.artifact.sha256 {
                bail!("artifact digest mismatch: the contents did not match the digest on its reference");
            }
            self.complete = true;
        }
        Ok(bytes)
    }
}

/// Assemble a preview only within a caller-owned memory ceiling.
pub async fn read_all(
    source: &mut (impl ArtifactSource + ?Sized),
    artifact: &ArtifactRef,
    max_bytes: u64,
) -> anyhow::Result<Vec<u8>> {
    if artifact.byte_length > max_bytes || usize::try_from(artifact.byte_length).is_err() {
        bail!("the artifact reference declares {} bytes, above the {max_bytes} byte ceiling this client will assemble; save the file instead", artifact.byte_length);
    }
    let mut transfer = Transfer::new(artifact)?;
    let mut bytes = Vec::new();
    while !transfer.complete {
        bytes.extend_from_slice(&transfer.next(source).await?);
    }
    Ok(bytes)
}

/// Stream into a private sibling file and replace the destination only after
/// complete length/digest verification and a successful disk sync. Failure or
/// cancellation during transfer drops staging and leaves an existing export intact.
pub async fn save_to(
    source: &mut (impl ArtifactSource + ?Sized),
    artifact: &ArtifactRef,
    destination: &Path,
) -> anyhow::Result<u64> {
    let mut transfer = Transfer::new(artifact)?;
    let destination = destination.to_owned();
    let path = destination.clone();
    let staged = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        if path.file_name().is_none() {
            bail!("the export destination must name a file");
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        tempfile::Builder::new()
            .prefix(".codypendent-export-")
            .tempfile_in(parent)
            .context("creating the export staging file")
    })
    .await
    .context("preparing the export file")??;
    let mut file =
        tokio::fs::File::from_std(staged.reopen().context("opening the export staging file")?);
    while !transfer.complete {
        file.write_all(&transfer.next(source).await?)
            .await
            .context("writing the export")?;
    }
    file.sync_all().await.context("syncing the export")?;
    drop(file);
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        if let Err(error) = staged.persist(&destination) {
            // PersistError owns the staging file. Drop it before returning an
            // error that a UI might retain for the rest of the session.
            drop(error.file);
            return Err(error.error)
                .with_context(|| format!("saving the export to {}", destination.display()));
        }
        Ok(())
    })
    .await
    .context("finishing the export")??;
    Ok(transfer.offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codypendent_protocol::{ArtifactId, DataClassification};
    use std::collections::VecDeque;

    struct Source {
        replies: VecDeque<Payload>,
        offsets: Vec<u64>,
    }
    #[async_trait]
    impl ArtifactSource for Source {
        async fn read_chunk(&mut self, command: CommandBody) -> anyhow::Result<Payload> {
            let CommandBody::ReadArtifact { offset, limit, .. } = command else {
                panic!("expected read")
            };
            assert_eq!(limit, ARTIFACT_CHUNK_BYTES);
            self.offsets.push(offset);
            self.replies.pop_front().context("connection dropped")
        }
    }
    fn artifact(bytes: &[u8]) -> ArtifactRef {
        ArtifactRef {
            id: ArtifactId::new(),
            media_type: "application/octet-stream".into(),
            byte_length: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
            sensitivity: DataClassification::Confidential,
        }
    }
    fn chunk(reference: &ArtifactRef, offset: u64, bytes: &[u8], eof: bool) -> Payload {
        Payload::ArtifactChunk {
            artifact_id: reference.id,
            offset,
            bytes_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            eof,
            sha256: reference.sha256.clone(),
        }
    }
    fn source(replies: Vec<Payload>) -> Source {
        Source {
            replies: replies.into(),
            offsets: vec![],
        }
    }

    #[tokio::test]
    async fn verifies_multiple_chunks_and_an_empty_artifact() {
        let reference = artifact(b"abcdef");
        let mut peer = source(vec![
            chunk(&reference, 0, b"ab", false),
            chunk(&reference, 2, b"cdef", true),
        ]);
        assert_eq!(read_all(&mut peer, &reference, 6).await.unwrap(), b"abcdef");
        assert_eq!(peer.offsets, vec![0, 2]);
        let empty = artifact(b"");
        assert!(
            read_all(&mut source(vec![chunk(&empty, 0, b"", true)]), &empty, 0)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn rejects_wrong_identity_offset_and_digest_metadata() {
        let reference = artifact(b"data");
        for field in 0..3 {
            let mut reply = chunk(&reference, 0, b"data", true);
            let Payload::ArtifactChunk {
                artifact_id,
                offset,
                sha256,
                ..
            } = &mut reply
            else {
                unreachable!()
            };
            match field {
                0 => *artifact_id = ArtifactId::new(),
                1 => *offset = 1,
                _ => *sha256 = "b".repeat(64),
            }
            assert!(read_all(&mut source(vec![reply]), &reference, 4)
                .await
                .unwrap_err()
                .to_string()
                .contains("does not match"));
        }
    }

    #[tokio::test]
    async fn rejects_truncation_overrun_stall_and_corrupt_contents() {
        let reference = artifact(b"data");
        for (bytes, eof, message) in [
            (b"da".as_slice(), true, "returned 2"),
            (b"data!", true, "declared length"),
            (b"", false, "no progress"),
            (b"nope", true, "digest"),
        ] {
            let error = read_all(
                &mut source(vec![chunk(&reference, 0, bytes, eof)]),
                &reference,
                4,
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
        }
    }

    #[tokio::test]
    async fn bounds_encoded_and_decoded_chunks_before_assembly() {
        let reference = artifact(b"data");
        for size in [
            ARTIFACT_CHUNK_BYTES as usize + 1,
            ARTIFACT_CHUNK_BYTES as usize + 4,
        ] {
            let reply = chunk(&reference, 0, &vec![b'x'; size], true);
            let error = read_all(&mut source(vec![reply]), &reference, 4)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("byte limit"), "{error}");
        }
        let mut reply = chunk(&reference, 0, b"", true);
        if let Payload::ArtifactChunk { bytes_base64, .. } = &mut reply {
            *bytes_base64 = "!!!!".into();
        }
        assert!(read_all(&mut source(vec![reply]), &reference, 4)
            .await
            .unwrap_err()
            .to_string()
            .contains("decoding"));
    }

    #[tokio::test]
    async fn rejects_excessive_preview_and_invalid_reference_before_requesting_bytes() {
        let mut reference = artifact(b"data");
        let mut peer = source(vec![]);
        assert!(read_all(&mut peer, &reference, 3)
            .await
            .unwrap_err()
            .to_string()
            .contains("save the file instead"));
        reference.sha256 = "invalid".into();
        assert!(read_all(&mut peer, &reference, 4).await.is_err());
        assert!(peer.offsets.is_empty());
    }

    #[tokio::test]
    async fn failed_reads_never_replace_an_existing_file_or_leave_staging_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.bin");
        std::fs::write(&path, b"existing export").unwrap();
        let reference = artifact(b"data");
        for replies in [
            vec![chunk(&reference, 0, b"da", false)],
            vec![chunk(&reference, 0, b"nope", true)],
        ] {
            let error = save_to(&mut source(replies), &reference, &path)
                .await
                .unwrap_err();
            assert_eq!(std::fs::read(&path).unwrap(), b"existing export", "{error}");
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }

    #[tokio::test]
    async fn saves_exact_private_bytes_and_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/export.bin");
        let bytes = [0, 255, 13, 10, 1];
        let reference = artifact(&bytes);
        for _ in 0..2 {
            assert_eq!(
                save_to(
                    &mut source(vec![chunk(&reference, 0, &bytes, true)]),
                    &reference,
                    &path
                )
                .await
                .unwrap(),
                5
            );
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o077,
                0
            );
        }
    }

    #[tokio::test]
    async fn saves_to_a_bare_relative_filename() {
        // No process-wide chdir: an owned temporary name exercises a bare
        // relative --out path without changing the other tests' directory.
        let reserved = tempfile::NamedTempFile::new_in(".").unwrap();
        let path = Path::new(reserved.path().file_name().unwrap());
        let reference = artifact(b"data");
        save_to(
            &mut source(vec![chunk(&reference, 0, b"data", true)]),
            &reference,
            path,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"data");
        reserved.close().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replaces_a_destination_symlink_without_truncating_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("important.txt");
        let path = dir.path().join("export.bin");
        std::fs::write(&target, "keep original").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let reference = artifact(b"data");
        save_to(
            &mut source(vec![chunk(&reference, 0, b"data", true)]),
            &reference,
            &path,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(target).unwrap(), "keep original");
        assert_eq!(std::fs::read(&path).unwrap(), b"data");
        assert!(!std::fs::symlink_metadata(path)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[tokio::test]
    async fn streams_an_export_larger_than_the_desktop_preview_ceiling() {
        struct Repeated {
            reference: ArtifactRef,
            block: Vec<u8>,
            requests: usize,
        }
        #[async_trait]
        impl ArtifactSource for Repeated {
            async fn read_chunk(&mut self, command: CommandBody) -> anyhow::Result<Payload> {
                let CommandBody::ReadArtifact {
                    offset,
                    limit,
                    expected_sha256,
                    ..
                } = command
                else {
                    unreachable!()
                };
                assert_eq!(limit, ARTIFACT_CHUNK_BYTES);
                assert_eq!(offset, self.requests as u64 * u64::from(limit));
                assert_eq!(expected_sha256, self.reference.sha256);
                self.requests += 1;
                Ok(chunk(
                    &self.reference,
                    offset,
                    &self.block,
                    self.requests == 65,
                ))
            }
        }
        let block = vec![b'x'; ARTIFACT_CHUNK_BYTES as usize];
        let mut digest = Sha256::new();
        for _ in 0..65 {
            digest.update(&block);
        }
        let mut reference = artifact(b"");
        reference.sha256 = format!("{:x}", digest.finalize());
        reference.byte_length = 65 * u64::from(ARTIFACT_CHUNK_BYTES);
        let mut peer = Repeated {
            reference: reference.clone(),
            block,
            requests: 0,
        };
        assert!(read_all(&mut peer, &reference, 64 * 1024 * 1024)
            .await
            .is_err());
        assert_eq!(peer.requests, 0);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.export");
        assert_eq!(
            save_to(&mut peer, &reference, &path).await.unwrap(),
            reference.byte_length
        );
        assert_eq!(peer.requests, 65);
        assert_eq!(
            std::fs::metadata(path).unwrap().len(),
            reference.byte_length
        );
    }

    #[tokio::test]
    async fn persist_failure_cleans_up_even_while_the_error_is_retained() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("directory");
        std::fs::create_dir(&path).unwrap();
        let reference = artifact(b"data");
        let error = save_to(
            &mut source(vec![chunk(&reference, 0, b"data", true)]),
            &reference,
            &path,
        )
        .await
        .unwrap_err();
        assert!(path.is_dir(), "{error}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn cancellation_removes_partial_staging_and_preserves_the_destination() {
        struct Pausing {
            reference: ArtifactRef,
            requested: tokio::sync::oneshot::Sender<()>,
            first: bool,
        }
        #[async_trait]
        impl ArtifactSource for Option<Pausing> {
            async fn read_chunk(&mut self, _: CommandBody) -> anyhow::Result<Payload> {
                let data = self.as_mut().unwrap();
                if data.first {
                    data.first = false;
                    return Ok(chunk(&data.reference, 0, b"da", false));
                }
                let data = self.take().unwrap();
                data.requested.send(()).unwrap();
                std::future::pending().await
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("export.bin");
        std::fs::write(&path, b"original").unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let reference = artifact(b"data");
        let mut peer = Some(Pausing {
            reference: reference.clone(),
            requested: tx,
            first: true,
        });
        let task_path = path.clone();
        let task = tokio::spawn(async move { save_to(&mut peer, &reference, &task_path).await });
        rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
