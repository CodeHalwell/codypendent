//! A minimal, hand-rolled localhost HTTP/1.1 listener for GitHub deliveries.
//!
//! GitHub only needs a POST endpoint that reads a body and returns a status
//! code, so this is implemented directly over `tokio` rather than pulling in a
//! full HTTP framework. Each connection is parsed just enough to extract the
//! method, the delivery headers, and the `Content-Length` body, which are handed
//! to [`WebhookIngestor::ingest`]; the outcome maps to an HTTP status:
//!
//! - `SignatureMissing` / `SignatureInvalid` → `401`
//! - `Duplicate` → `200`
//! - `Accepted` → `202`
//! - a malformed body → `400`
//! - any other error → `500`
//! - a non-POST request → `405`

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use super::ingest::{DeliveryHeaders, IngestOutcome, WebhookIngestor};
use super::WebhookError;

/// Bind a TCP listener on `addr` (e.g. `127.0.0.1:8765`).
pub async fn bind(addr: &str) -> std::io::Result<TcpListener> {
    TcpListener::bind(addr).await
}

/// Whole-connection deadline: read request + ingest + write response. A GitHub
/// delivery completes in well under this; a client that dribbles bytes (or sends
/// headers then stalls the body — a slowloris) is cut off instead of holding a
/// connection and task open forever. `MAX_HEADER_BYTES` bounds memory; this
/// bounds *time*.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);
/// Concurrent-connection ceiling, so a flood of held-open sockets cannot
/// accumulate unbounded tasks/file descriptors. Excess connections wait for a
/// slot rather than being dropped (GitHub retries are precious).
const MAX_CONNECTIONS: usize = 64;

/// Serve the webhook endpoint on `listener`, spawning a task per connection —
/// bounded by [`MAX_CONNECTIONS`] and each subject to [`CONNECTION_TIMEOUT`].
///
/// Returns only if the accept loop itself fails; per-connection errors are
/// logged and do not stop the server.
pub async fn serve(listener: TcpListener, ingestor: Arc<WebhookIngestor>) -> std::io::Result<()> {
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (stream, _peer) = listener.accept().await?;
        let ingestor = Arc::clone(&ingestor);
        let permits = Arc::clone(&permits);
        tokio::spawn(async move {
            // The semaphore is never closed, so acquire only fails on close.
            let Ok(_permit) = permits.acquire().await else {
                return;
            };
            match tokio::time::timeout(CONNECTION_TIMEOUT, handle_connection(stream, ingestor))
                .await
            {
                Ok(Err(error)) => {
                    tracing::debug!(%error, "webhook connection ended with error");
                }
                Err(_elapsed) => {
                    tracing::debug!("webhook connection timed out");
                }
                Ok(Ok(())) => {}
            }
        });
    }
}

/// The most header bytes we buffer before a complete `\r\n\r\n` terminator. A
/// client that never terminates its headers (or sends absurdly large ones) is
/// rejected rather than allowed to grow our buffer without bound.
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// The most body bytes we accept. GitHub caps deliveries at 25 MiB; this
/// loopback listener is stricter, and a larger `Content-Length` is refused up
/// front instead of read.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Read one request, run it through the ingestor, and write the status line.
async fn handle_connection(
    mut stream: TcpStream,
    ingestor: Arc<WebhookIngestor>,
) -> std::io::Result<()> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];

    // Read until the end of the header block. The body may arrive in the same
    // read, so we search the accumulated buffer each time — bounded by
    // `MAX_HEADER_BYTES` so an unterminated header stream cannot exhaust memory.
    let header_end = loop {
        if let Some(position) = find_subslice(&buffer, b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return write_response(&mut stream, 431, "Request Header Fields Too Large").await;
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            // Connection closed before a complete header block.
            return write_response(&mut stream, 400, "Bad Request").await;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let header_text = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = header_text.split("\r\n");

    // Request line: METHOD PATH VERSION.
    let request_line = lines.next().unwrap_or_default();
    let method = request_line.split_whitespace().next().unwrap_or_default();
    if !method.eq_ignore_ascii_case("POST") {
        return write_response(&mut stream, 405, "Method Not Allowed").await;
    }

    // Headers (names are case-insensitive).
    let mut content_length: usize = 0;
    let mut signature: Option<String> = None;
    let mut event_type = String::new();
    let mut delivery_id = String::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            // A garbled length must be a 400, not silently "0" (which would
            // misprocess a request that does carry a body).
            match value.parse::<usize>() {
                Ok(parsed) => content_length = parsed,
                Err(_) => return write_response(&mut stream, 400, "Bad Request").await,
            }
        } else if name.eq_ignore_ascii_case("x-hub-signature-256") {
            signature = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-github-event") {
            event_type = value.to_string();
        } else if name.eq_ignore_ascii_case("x-github-delivery") {
            delivery_id = value.to_string();
        }
    }

    // A GitHub delivery always carries its event type and a unique delivery id;
    // without them the request is malformed. Rejecting an empty delivery id here
    // is also what keeps a header-less request from poisoning the idempotency
    // table with a `""` key that would make every later such request a duplicate.
    if event_type.is_empty() || delivery_id.is_empty() {
        return write_response(&mut stream, 400, "Bad Request").await;
    }
    // Refuse an over-large body up front rather than reading it.
    if content_length > MAX_BODY_BYTES {
        return write_response(&mut stream, 413, "Payload Too Large").await;
    }

    // The body is whatever followed the header terminator, extended until we
    // have `Content-Length` bytes.
    let body_start = header_end + 4;
    let mut body = buffer[body_start..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);

    let headers = DeliveryHeaders {
        signature,
        event_type,
        delivery_id,
    };
    let (code, reason) = match ingestor.ingest(&headers, &body).await {
        Ok(IngestOutcome::SignatureMissing | IngestOutcome::SignatureInvalid) => {
            (401, "Unauthorized")
        }
        Ok(IngestOutcome::Duplicate) => (200, "OK"),
        Ok(IngestOutcome::Accepted { .. }) => (202, "Accepted"),
        Err(WebhookError::Malformed(_)) => (400, "Bad Request"),
        Err(_) => (500, "Internal Server Error"),
    };
    write_response(&mut stream, code, reason).await
}

/// Write a minimal, bodyless HTTP/1.1 response and close the connection.
async fn write_response(stream: &mut TcpStream, code: u16, reason: &str) -> std::io::Result<()> {
    let response =
        format!("HTTP/1.1 {code} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

/// The index of the first occurrence of `needle` in `haystack`, if any.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
