//! A reusable, handshaken connection to `codypendentd`'s session protocol
//! (STEP 1.11: `ClientHello`/`ServerHello`, `Command`/reply correlation, event
//! forwarding, heartbeats).
//!
//! This module is deliberately **not** JSONL-specific: `codypendent run`,
//! `codypendent attach` (`crate::commands`, `crate::stream`), and the future
//! TUI wiring (STEP 1.12) all consume it the same way — connect, handshake,
//! send a command, read the next envelope, repeat. Rendering (JSONL lines or
//! TUI widgets) is entirely the caller's concern.

use std::collections::VecDeque;
use std::path::Path;

use anyhow::{anyhow, bail, Context};
use codypendent_protocol::{
    read_envelope, write_envelope, ClientCapabilities, ClientHello, ClientId, Command, CommandBody,
    CommandId, Envelope, Payload, ResumeToken, ServerHello, PROTOCOL_V1,
};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

/// A persistent connection to the daemon. One `Connection` wraps one Unix
/// socket; a single connection carries the handshake, any number of commands,
/// and the live event stream for whatever it has attached to (Chapter 03: one
/// connection, many attaches/subscriptions).
pub struct Connection {
    stream: UnixStream,
    client_id: ClientId,
    /// Envelopes read off the wire that did not correlate to an outstanding
    /// `request` and were not a heartbeat `Ping` — held for a later
    /// `next_envelope` call, in arrival order. Necessary because, once
    /// attached, live event forwarding and direct command replies share one
    /// socket and may interleave with a still-pending request (STEP 1.11's
    /// `forward_events` and `handle_request` write to the same connection
    /// independently).
    pending: VecDeque<Envelope>,
}

impl Connection {
    /// Connect to `socket_path`. Does not handshake — session commands are
    /// rejected by the daemon (`protocol.handshake-required`) until
    /// [`Connection::handshake`] runs.
    pub async fn connect(socket_path: &Path) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(socket_path)
            .await
            .with_context(|| format!("connecting to daemon socket {}", socket_path.display()))?;
        Ok(Self {
            stream,
            client_id: ClientId::new(),
            pending: VecDeque::new(),
        })
    }

    /// This connection's client identity, stamped on every outgoing envelope.
    pub fn client_id(&self) -> ClientId {
        self.client_id
    }

    /// The first exchange on every connection (Chapter 03): send `ClientHello`
    /// advertising `PROTOCOL_V1` and default capabilities, return the daemon's
    /// `ServerHello`. Presenting the `resume` token a prior `ServerHello`
    /// issued restores that connection's client identity daemon-side; the
    /// caller stores the token from the returned hello for its next reconnect.
    pub async fn handshake(
        &mut self,
        client_name: &str,
        client_version: &str,
        resume: Option<ResumeToken>,
    ) -> anyhow::Result<ServerHello> {
        let hello = ClientHello {
            client_name: client_name.to_string(),
            client_version: client_version.to_string(),
            supported_protocols: vec![PROTOCOL_V1],
            capabilities: ClientCapabilities::default(),
            resume_token: resume,
        };
        let reply = self.request(Payload::ClientHello(hello)).await?;
        match reply.payload {
            Payload::ServerHello(server_hello) => Ok(server_hello),
            Payload::Error(error) => bail!("daemon refused the handshake: {}", error.message),
            other => bail!("expected ServerHello, got {other:?}"),
        }
    }

    /// Send `body` as a fresh, idempotent [`Command`] and wait for its
    /// correlated reply. Returns the whole reply [`Envelope`] — not just the
    /// payload — because some replies carry meaning at the envelope level; see
    /// `crate::commands::run_over_connection` for why that matters
    /// (`CommandAccepted` carries no session/run id, so a freshly created
    /// session's id is read from the reply envelope's own `session_id` field
    /// when the daemon supplies one).
    pub async fn send_command(&mut self, body: CommandBody) -> anyhow::Result<Envelope> {
        let command_id = CommandId::new();
        let command = Command {
            command_id,
            // A fresh v7 UUID is already unique per call; using its own string
            // form as the idempotency key means a genuine client-side retry of
            // this exact command would reuse the same `command_id` (and hence
            // the same key) too — exactly the idempotent-retry contract
            // (Chapter 03 / STEP 1.3), without inventing a second identifier.
            idempotency_key: command_id.to_string(),
            expected_revision: None,
            body,
        };
        self.request(Payload::Command(command)).await
    }

    /// The next envelope not already consumed by a prior `request` call —
    /// draining the buffer first, then reading the wire. Heartbeat `Ping`s are
    /// answered with `Pong` transparently and never surfaced to the caller.
    /// `Ok(None)` means the daemon closed the connection cleanly (end of
    /// stream).
    pub async fn next_envelope(&mut self) -> anyhow::Result<Option<Envelope>> {
        if let Some(envelope) = self.pending.pop_front() {
            return Ok(Some(envelope));
        }
        loop {
            let Some(envelope) = read_envelope(&mut self.stream).await? else {
                return Ok(None);
            };
            if matches!(envelope.payload, Payload::Ping) {
                self.send_pong().await?;
                continue;
            }
            return Ok(Some(envelope));
        }
    }

    /// Write a fresh request envelope and read envelopes until the correlated
    /// reply arrives. Heartbeat `Ping`s are answered transparently; any other
    /// unrelated envelope (for example a live event outracing its own
    /// command's reply once attached) is buffered for the next
    /// `next_envelope` call rather than discarded.
    async fn request(&mut self, payload: Payload) -> anyhow::Result<Envelope> {
        let request = Envelope::request(self.client_id, payload);
        write_envelope(&mut self.stream, &request).await?;
        loop {
            let envelope = read_envelope(&mut self.stream)
                .await?
                .ok_or_else(|| anyhow!("daemon closed the connection before replying"))?;
            if envelope.correlation_id == Some(request.message_id) {
                return Ok(envelope);
            }
            if matches!(envelope.payload, Payload::Ping) {
                self.send_pong().await?;
                continue;
            }
            self.pending.push_back(envelope);
        }
    }

    async fn send_pong(&mut self) -> anyhow::Result<()> {
        let pong = Envelope::request(self.client_id, Payload::Pong);
        write_envelope(&mut self.stream, &pong).await?;
        Ok(())
    }

    /// Consume the connection into independently-owned read and write halves for
    /// a concurrent event loop (STEP 1.12 TUI): one task reads live events off
    /// the read half while the main loop dispatches commands through the write
    /// half. This is the split the request/reply model (`&mut self` for both
    /// directions) cannot express.
    ///
    /// Returns the halves, the [`ClientId`] to stamp on outgoing envelopes, and
    /// any envelopes already buffered by [`Connection::request`] during the
    /// setup handshake (live events that outraced the attach reply) — the caller
    /// must fold these before reading the wire so no event is lost.
    pub fn into_split(self) -> (OwnedReadHalf, OwnedWriteHalf, VecDeque<Envelope>, ClientId) {
        let (read_half, write_half) = self.stream.into_split();
        (read_half, write_half, self.pending, self.client_id)
    }
}
