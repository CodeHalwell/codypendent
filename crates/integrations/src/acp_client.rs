//! ACP (Agent Client Protocol) *client* — the inverse of `acp.rs`.
//!
//! `acp.rs` is the SERVER role (Codypendent serves ACP to Zed). This module is
//! the CLIENT/host role: Codypendent spawns an external ACP agent
//! (`gemini --acp`, `npx @agentclientprotocol/claude-agent-acp`, ...), does the
//! initialize/session handshake, delegates a run's objective as an ACP prompt,
//! and maps the agent's streamed `session/update`s onto Codypendent's existing
//! `EventBody` model. The agent owns its model; we send no model id.
