//! rootle-bitbucket — the Bitbucket Cloud provider for rootle.
//!
//! Speaks the rootle stdio provider protocol (NDJSON-RPC 2.0 over
//! stdin/stdout; the spec lives in rootledev/rootle,
//! doc/provider-protocol.md) against Bitbucket Cloud's REST 2.0 API.
//! Shares no code with rootle — the wire contract is the entire
//! interface (same shape as rootle-gitlab).
//!
//! Bitbucket Cloud has **no code-search index** — the handshake
//! declares `code_search: false, file_search: true` (protocol v1.3's
//! split): queries are answered by walking the repo tree (cached,
//! commit-keyed). Path/extension terms serve legal path-only hits;
//! bare terms grep the fetched blobs (binary-skipping, line-anchored,
//! bounded) — no index, just the tree.
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

/// One request line → zero or more reply lines. Used by the binary's
/// stdin loop, the fixture-backed conformance example, and tests
/// driving the protocol surface directly.
///
/// Notifications (no id) are tolerated chatter and never answered (the
/// advisory `$/cancelRequest` is the only one today and is ignored by
/// contract). Requests that opted into progressive results
/// (`"partial": true`, protocol v1.3) additionally emit their
/// `$/partial` batches *before* the reply through the same channel —
/// line order on the single pipe is part of the contract.
pub fn respond(handler: &Handler, line: &str, emit: &mut dyn FnMut(String)) {
    let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
        return;
    };
    let Some(id) = msg.get("id") else {
        return;
    };
    let id = id.clone();
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        return;
    };
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let reply = {
        let mut partials = handlers::PartialSink::new(&id, &params, emit);
        match handler.dispatch(method, &params, &mut partials) {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e.to_json() }),
        }
    };
    emit(reply.to_string());
}
