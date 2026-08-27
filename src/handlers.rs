//! Protocol dispatch: one method per arm, the wire shapes from
//! doc/provider-protocol.md (v1.5). Capabilities declare the split:
//! `code_search: false, file_search: true` — Bitbucket Cloud has no
//! code-search index; queries are answered by walking the
//! commit-pinned tree (path/extension terms as path-only hits, bare
//! terms by grepping the fetched blobs). Revision awareness (v1.5):
//! `refs: true, log: true` — and `blame: false`, the honest answer
//! (Bitbucket Cloud has no blame API); dispatch has no `repo/blame`
//! arm, so the call fails as an unknown method instead of a stub
//! that fake-succeeds.
//!
//! Layout: this file is the surface — handler state, dispatch, the
//! $/partial plumbing, and the wire error taxonomy. The method bodies
//! live in sibling submodules by protocol concern: `initialize.rs`
//! (handshake + cache re-rooting), `search.rs` (`search/repos`,
//! `org/repos`), `tree.rs` (branch → commit walk + revalidation, and
//! the v1.5 ref → commit resolution every revision method shares),
//! `refs.rs` (`repo/refs`), `log.rs` (`repo/log`), `blob.rs`
//! (`repo/blob`, `repo/blob_at`), `urls.rs` (`repo/web_url`,
//! `org/url`, `repo/clone_url`), `code.rs` (`search/code` +
//! streaming).

mod blob;
mod code;
mod initialize;
mod log;
mod refs;
mod search;
mod tree;
mod urls;

#[cfg(test)]
mod tests;

use crate::api::{ApiError, Bitbucket};
use crate::cache::Cache;
use serde_json::{Value, json};

pub struct Handler {
    pub bb: Bitbucket,
    /// Workspaces to serve when discovery can't run (a token scoped
    /// to repositories only — CHANGE-2770 killed the cross-workspace
    /// listings, and /user/workspaces wants the account read scope).
    /// From --workspace flags or BITBUCKET_WORKSPACES.
    pub workspaces: Vec<String>,
    /// Rooted at the handshake's cache_dir when rootle passes one
    /// (the documented contract); otherwise the XDG default. Interior
    /// mutability because initialize is the first message — &self
    /// throughout.
    pub cache: parking_lot::RwLock<Cache>,
}

/// Wire error taxonomy (protocol v1.1): semantics ride in data.kind.
pub struct WireError {
    pub kind: &'static str,
    pub message: String,
    pub retry_after_s: Option<u64>,
}

impl WireError {
    pub fn from_api(e: &ApiError) -> WireError {
        match e {
            ApiError::Api {
                status,
                message,
                retry_after,
            } => {
                let kind = match status {
                    401 | 403 => "auth",
                    429 => "rate_limited",
                    404 => "not_found",
                    // Our own 1 MiB preview cap, refused before the
                    // transfer: adapter policy, not a transport fault.
                    413 => "provider",
                    _ => "network",
                };
                WireError {
                    kind,
                    message: message.clone(),
                    retry_after_s: *retry_after,
                }
            }
            ApiError::Network(m) => WireError {
                kind: "network",
                message: m.clone(),
                retry_after_s: None,
            },
        }
    }

    pub fn to_json(&self) -> Value {
        let mut data = json!({ "kind": self.kind });
        if let Some(s) = self.retry_after_s {
            data["retry_after_s"] = json!(s);
        }
        json!({ "code": 1, "message": self.message, "data": data })
    }
}

type WireResult = Result<Value, WireError>;

impl From<ApiError> for WireError {
    fn from(e: ApiError) -> WireError {
        WireError::from_api(&e)
    }
}

fn w<T>(r: crate::api::ApiResult<T>, f: impl FnOnce(T) -> Value) -> WireResult {
    r.map(f).map_err(|e| WireError::from_api(&e))
}

/// The `$/partial` channel (protocol v1.3 progressive results): a
/// request that opted in with `"partial": true` gets its result
/// streamed as append-only batches keyed by the request id, followed
/// by a metadata-only reply. Handlers emit through this sink; when the
/// request did not opt in, `send` is a no-op and the reply carries
/// everything (unchanged v1.2 behavior).
pub struct PartialSink<'a> {
    id: &'a Value,
    enabled: bool,
    emit: &'a mut dyn FnMut(String),
}

impl<'a> PartialSink<'a> {
    pub fn new(id: &'a Value, params: &Value, emit: &'a mut dyn FnMut(String)) -> Self {
        PartialSink {
            id,
            enabled: params
                .get("partial")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            emit,
        }
    }

    /// True when the request opted into streaming — the reply must
    /// then be metadata-only (`items: []`, `truncated` authoritative).
    pub fn wants(&self) -> bool {
        self.enabled
    }

    /// Emit one `$/partial` batch for this request's id.
    fn send(&mut self, items: &[Value]) {
        if !self.enabled {
            return;
        }
        let note = json!({
            "jsonrpc": "2.0",
            "method": "$/partial",
            "params": { "id": self.id, "items": items },
        });
        (self.emit)(note.to_string());
    }
}

impl Handler {
    pub fn new(
        instance: &str,
        token_env: &str,
        username_env: &str,
        cache_base: Option<std::path::PathBuf>,
        workspaces: Vec<String>,
    ) -> Self {
        Handler {
            bb: Bitbucket::new(instance, token_env, username_env),
            cache: parking_lot::RwLock::new(Cache::new(cache_base)),
            workspaces,
        }
    }

    pub fn dispatch(
        &self,
        method: &str,
        params: &Value,
        partials: &mut PartialSink<'_>,
    ) -> WireResult {
        match method {
            "initialize" => self.initialize(params),
            "search/repos" => self.search_repos(params["query"].as_str().unwrap_or("")),
            "org/repos" => self.org_repos(params["org"].as_str().unwrap_or("")),
            "repo/tree" => self.repo_tree(
                params["repo"].as_str().unwrap_or(""),
                params["ref"].as_str(),
            ),
            "repo/blob" => self.repo_blob(
                params["repo"].as_str().unwrap_or(""),
                params["sha"].as_str().unwrap_or(""),
            ),
            // v1.5 revisions: refs, log, blob_at. `repo/blame` is
            // deliberately absent — capability false; the unknown-
            // method error below is the honest reply.
            "repo/refs" => self.repo_refs(params["repo"].as_str().unwrap_or("")),
            "repo/log" => self.repo_log(
                params["repo"].as_str().unwrap_or(""),
                params["path"].as_str(),
                params["ref"].as_str(),
                params["limit"].as_u64(),
            ),
            "repo/blob_at" => self.repo_blob_at(
                params["repo"].as_str().unwrap_or(""),
                params["path"].as_str().unwrap_or(""),
                params["ref"].as_str(),
            ),
            "repo/clone_url" => self.repo_clone_url(params["repo"].as_str().unwrap_or("")),
            "repo/web_url" => self.repo_web_url(
                params["repo"].as_str().unwrap_or(""),
                params["path"].as_str().unwrap_or(""),
                params["branch"].as_str().unwrap_or(""),
                params["line"].as_u64(),
                params["is_file"].as_bool().unwrap_or(false),
            ),
            "org/url" => self.org_url(params["org"].as_str().unwrap_or("")),
            "search/code" => self.search_code(params, partials),
            other => Err(WireError {
                kind: "provider",
                message: format!("unknown method {other:?}"),
                retry_after_s: None,
            }),
        }
    }
}
