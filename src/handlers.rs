//! Protocol dispatch: one method per arm, the wire shapes from
//! doc/provider-protocol.md (v1.3). Capabilities declare the split:
//! `code_search: false, file_search: true` — Bitbucket Cloud has no
//! code-search API; filename search walks the commit-pinned tree and
//! serves legal path-only hits.
//!
//! Layout: this file is the surface — handler state, dispatch, and
//! the wire error taxonomy. The method bodies live in sibling
//! submodules by protocol concern: `initialize.rs` (handshake +
//! cache re-rooting), `search.rs` (`search/repos`, `org/repos`),
//! `tree.rs` (branch → commit walk + revalidation), `blob.rs`
//! (`repo/blob`), `urls.rs` (`repo/web_url`, `org/url`,
//! `repo/clone_url`), `code.rs` (`search/code` as path-only hits).

mod blob;
mod code;
mod initialize;
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

    pub fn dispatch(&self, method: &str, params: &Value) -> WireResult {
        match method {
            "initialize" => self.initialize(params),
            "search/repos" => self.search_repos(params["query"].as_str().unwrap_or("")),
            "org/repos" => self.org_repos(params["org"].as_str().unwrap_or("")),
            "repo/tree" => self.repo_tree(params["repo"].as_str().unwrap_or("")),
            "repo/blob" => self.repo_blob(
                params["repo"].as_str().unwrap_or(""),
                params["sha"].as_str().unwrap_or(""),
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
            "search/code" => self.search_code(params),
            other => Err(WireError {
                kind: "provider",
                message: format!("unknown method {other:?}"),
                retry_after_s: None,
            }),
        }
    }
}
