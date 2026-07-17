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

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::ingest::{DeliveryHeaders, IngestOutcome, WebhookIngestor};
use super::WebhookError;

/// Bind a TCP listener on `addr` (e.g. `127.0.0.1:8765`).
pub async fn bind(addr: &str) -> std::io::Result<TcpListener> {
    TcpListener::bind(addr).await
}

/// Serve the webhook endpoint on `listener`, spawning a task per connection.
///
/// Returns only if the accept loop itself fails; per-connection errors are
/// logged and do not stop the server.
pub async fn serve(listener: TcpListener, ingestor: Arc<WebhookIngestor>) -> std::io::Result<()> {
    loop {
        let (stream, _peer) = listener.accept().await?;
        let ingestor = Arc::clone(&ingestor);
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream, ingestor).await {
                tracing::debug!(%error, "webhook connection ended with error");
            }
        });
    }
}

/// Read one request, run it through the ingestor, and write the status line.
async fn handle_connection(
    mut stream: TcpStream,
    ingestor: Arc<WebhookIngestor>,
) -> std::io::Result<()> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];

    // Read until the end of the header block. The body may arrive in the same
    // read, so we search the accumulated buffer each time.
    let header_end = loop {
        if let Some(position) = find_subslice(&buffer, b"\r\n\r\n") {
            break position;
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
            content_length = value.parse::<usize>().unwrap_or(0);
        } else if name.eq_ignore_ascii_case("x-hub-signature-256") {
            signature = Some(value.to_string());
        } else if name.eq_ignore_ascii_case("x-github-event") {
            event_type = value.to_string();
        } else if name.eq_ignore_ascii_case("x-github-delivery") {
            delivery_id = value.to_string();
        }
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
