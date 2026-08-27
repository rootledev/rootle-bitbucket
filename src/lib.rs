//! rootle-bitbucket — the Bitbucket Cloud provider for rootle.
//!
//! Speaks the rootle stdio provider protocol (NDJSON-RPC 2.0 over
//! stdin/stdout; the spec lives in rootledev/rootle,
//! doc/provider-protocol.md) against Bitbucket Cloud's REST 2.0 API.
//! Shares no code with rootle — the wire contract is the entire
//! interface (same shape as rootle-gitlab).
//!
//! Bitbucket Cloud has **no code-search API** — the handshake declares
//! `code_search: false, file_search: true` (protocol v1.3's split):
//! filename search walks the repo tree (cached, commit-keyed) and
//! serves legal path-only hits; content grep answers with an honest
//! per-call error.
//!
//! Process-shape obligations (restart obligations): startup is cheap
//! and idempotent — no network, no token read; rootle may kill and
//! respawn this process an unbounded number of times per session.
//! Credentials are read lazily on the first API call; caches are on
//! disk keyed by commit ids, so a respawn loses nothing.

pub mod api;
pub mod cache;
pub mod handlers;

pub use handlers::{Handler, WireError};

use serde_json::{Value, json};

/// One request line → one reply line. Used by the binary's stdin loop
/// and by tests driving the protocol surface directly.
pub fn respond(handler: &Handler, line: &str) -> Option<String> {
    let msg: Value = serde_json::from_str(line.trim()).ok()?;
    // Notifications (no id) are tolerated chatter; the advisory
    // cancel is the only one today and is ignored by contract.
    let id = msg.get("id")?.clone();
    let method = msg.get("method")?.as_str()?.to_string();
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let reply = match handler.dispatch(&method, &params) {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e.to_json() }),
    };
    Some(reply.to_string())
}
