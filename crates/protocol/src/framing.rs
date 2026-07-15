//! Length-prefixed JSON framing.
//!
//! ```text
//! +----------------------+-------------------------+
//! | u32 payload length   | serialized envelope     |
//! +----------------------+-------------------------+
//! ```
//!
//! The length prefix is big-endian. JSON is the first serialization for
//! inspectability; MessagePack may be negotiated in a later protocol version.
//! Large data travels as artifact references, never as huge frames.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::envelope::Envelope;

/// Frames larger than this are a protocol violation (16 MiB).
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame of {0} bytes exceeds MAX_FRAME_BYTES")]
    TooLarge(usize),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Write one envelope as a length-prefixed frame and flush.
pub async fn write_envelope<W: AsyncWrite + Unpin>(
    writer: &mut W,
    envelope: &Envelope,
) -> Result<(), FrameError> {
    let bytes = serde_json::to_vec(envelope)?;
    let len = u32::try_from(bytes.len()).map_err(|_| FrameError::TooLarge(bytes.len()))?;
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(bytes.len()));
    }
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one envelope. Returns `Ok(None)` only on a clean end-of-stream before
/// the first length byte. A stream that ends mid-prefix or mid-payload is an
/// error — the first byte is read separately because `read_exact` alone would
/// conflate a clean close (0 bytes) with a connection dropped after a partial
/// prefix (1–3 bytes).
pub async fn read_envelope<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Envelope>, FrameError> {
    let mut len_buf = [0u8; 4];
    match reader.read(&mut len_buf[..1]).await {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(e) => return Err(e.into()),
    }
    reader.read_exact(&mut len_buf[1..]).await?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(len as usize));
    }
    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;
    Ok(Some(serde_json::from_slice(&buf)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{Envelope, Payload};
    use crate::ids::ClientId;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn round_trip() {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        let sent = Envelope::request(ClientId::new(), Payload::Ping);
        write_envelope(&mut writer, &sent).await.expect("write");
        let received = read_envelope(&mut reader)
            .await
            .expect("read")
            .expect("one envelope");
        assert_eq!(received.message_id, sent.message_id);
        assert!(matches!(received.payload, Payload::Ping));
    }

    #[tokio::test]
    async fn clean_eof_returns_none() {
        let (writer, mut reader) = tokio::io::duplex(1024);
        drop(writer);
        let result = read_envelope(&mut reader).await.expect("clean eof is ok");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn truncated_length_prefix_is_an_error() {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        writer.write_all(&[0x00, 0x00]).await.expect("two bytes");
        drop(writer);
        assert!(
            read_envelope(&mut reader).await.is_err(),
            "a stream dropped mid-prefix must not look like a clean close"
        );
    }

    #[tokio::test]
    async fn truncated_payload_is_an_error() {
        let (mut writer, mut reader) = tokio::io::duplex(1024);
        writer.write_all(&8u32.to_be_bytes()).await.expect("prefix");
        writer.write_all(&[1, 2, 3]).await.expect("partial payload");
        drop(writer);
        assert!(read_envelope(&mut reader).await.is_err());
    }
}
