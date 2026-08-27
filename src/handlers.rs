//! Protocol dispatch: one method per arm, the wire shapes from
//! doc/provider-protocol.md (v1.3). Capabilities declare the split:
//! `code_search: false, file_search: true` — Bitbucket Cloud has no
//! code-search API; filename search walks the commit-pinned tree and
//! serves legal path-only hits.

use crate::api::{ApiError, ApiResult, Bitbucket, Repo};
use crate::cache::{Cache, RepoMeta, Tree};
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
            "org/url" => Ok(json!({
                "url": format!("https://bitbucket.org/{}", params["org"].as_str().unwrap_or(""))
            })),
            "search/code" => self.search_code(params),
            other => Err(WireError {
                kind: "provider",
                message: format!("unknown method {other:?}"),
                retry_after_s: None,
            }),
        }
    }

    fn initialize(&self, params: &Value) -> WireResult {
        // The handshake's cache_dir wins over the default — rootle
        // owns the subtree naming (protocol v1.2) and respawns re-send
        // it, so re-rooting is idempotent.
        if let Some(dir) = params["cache_dir"].as_str()
            && let path = std::path::PathBuf::from(dir)
        {
            let same_root = self
                .cache
                .read()
                .root_as_str()
                .is_some_and(|r| r == path.to_string_lossy());
            if !same_root {
                *self.cache.write() = Cache::new(Some(path));
            }
        }
        Ok(json!({
            "protocol": 1,
            "name": "bitbucket",
            // v1.3: the modeline icon (rootle renders its bitbucket
            // glyph when the user enables nerd_font).
            "icon": "bitbucket",
            // The split (v1.3): Bitbucket Cloud has no code-search
            // API; filename search walks the tree.
            "capabilities": {
                "orgs": true,
                "code_search": false,
                "file_search": true
            }
        }))
    }

    fn search(&self, query: &str) -> ApiResult<Vec<(String, Vec<Repo>)>> {
        let q = query.to_lowercase();
        // Configured workspaces are served directly (a token scoped to
        // repositories only can't discover — CHANGE-2770); otherwise
        // /user/workspaces, which wants the account read scope.
        let slugs: Vec<String> = if !self.workspaces.is_empty() {
            self.workspaces
                .iter()
                .filter(|s| s.to_lowercase().contains(&q))
                .cloned()
                .collect()
        } else {
            self.bb
                .workspaces()?
                .into_iter()
                .filter(|ws| {
                    ws.slug.to_lowercase().contains(&q)
                        || ws
                            .name
                            .as_deref()
                            .unwrap_or_default()
                            .to_lowercase()
                            .contains(&q)
                })
                .map(|ws| ws.slug)
                .collect()
        };
        let mut out = Vec::new();
        for slug in slugs {
            let repos = self.bb.workspace_repos(&slug)?;
            out.push((slug, repos));
            if out.len() >= 5 {
                break;
            }
        }
        Ok(out)
    }

    fn search_repos(&self, query: &str) -> WireResult {
        w(self.search(query), |groups| {
            let mut items = Vec::new();
            for (ws, repos) in &groups {
                items.push(json!({ "org": ws }));
                for repo in repos.iter().take(10) {
                    items.push(json!({ "full_name": repo.full_name }));
                }
                if items.len() >= 20 {
                    break;
                }
            }
            if items.is_empty() {
                // Honest fallback: the query as a workspace guess.
                items.push(json!({ "org": query }));
            }
            json!({ "items": items })
        })
    }

    fn org_repos(&self, org: &str) -> WireResult {
        w(
            self.bb.workspace_repos(org),
            |repos| json!({ "repos": repos.iter().map(|r| r.name().to_string()).collect::<Vec<_>>() }),
        )
    }

    /// Branch → commit, then the walk (cache-first: a commit-pinned
    /// tree is immutable).
    fn tree_at_commit(&self, full_name: &str) -> crate::api::ApiResult<(String, Tree)> {
        // Two statements, deliberately: the read guard is a temporary
        // that must drop before revalidate takes the write lock
        // (parking_lot deadlocks otherwise).
        let cached = self.cache.read().repo_meta(full_name);
        let meta = cached.or_else(|| self.revalidate_repo(full_name).ok());
        let Some(meta) = meta else {
            return Err(ApiError::Api {
                status: 404,
                message: format!("no such repo {full_name:?}"),
                retry_after: None,
            });
        };
        if let Some(tree) = self.cache.read().tree(full_name, &branch_key(&meta)) {
            return Ok((meta.branch, tree));
        }
        let commit = self.bb.branch_head(full_name, &meta.branch)?;
        let key = commit_key(&commit);
        if let Some(tree) = self.cache.read().tree(full_name, &key) {
            // Remember the head mapping so the next cold start skips
            // the ref round trip.
            self.cache
                .write()
                .store_tree(full_name, &branch_key(&meta), &tree);
            return Ok((meta.branch, tree));
        }
        let (entries, truncated) = self.bb.walk_tree(full_name, &commit)?;
        let tree = Tree {
            entries,
            truncated,
            branch: meta.branch.clone(),
        };
        self.cache.write().store_tree(full_name, &key, &tree);
        self.cache
            .write()
            .store_tree(full_name, &branch_key(&meta), &tree);
        Ok((meta.branch, tree))
    }

    fn revalidate_repo(&self, full_name: &str) -> crate::api::ApiResult<RepoMeta> {
        let repo: Repo = self.bb.repo(full_name)?;
        let meta = RepoMeta {
            full_name: repo.full_name.clone(),
            branch: repo.branch(),
        };
        self.cache.write().store_repo_meta(&meta);
        Ok(meta)
    }

    fn repo_tree(&self, full_name: &str) -> WireResult {
        // A 404 on a cached repo means it moved — revalidate once.
        let result = self.tree_at_commit(full_name);
        let (branch, tree) = match result {
            Ok(v) => v,
            Err(ApiError::Api { status: 404, .. }) => {
                self.cache.write().drop_repo_meta(full_name);
                let repo = self.bb.repo(full_name)?;
                let meta = RepoMeta {
                    full_name: repo.full_name.clone(),
                    branch: repo.branch(),
                };
                self.cache.write().store_repo_meta(&meta);
                self.tree_at_commit(full_name)?
            }
            Err(e) => return Err(e.into()),
        };
        Ok(json!({
            "entries": tree
                .entries
                .iter()
                .map(|e| json!({
                    "path": e.path,
                    "type": if e.is_dir { "tree" } else { "blob" },
                    "sha": e.sha,
                    "size": e.size,
                }))
                .collect::<Vec<_>>(),
            "truncated": tree.truncated,
            "branch": branch,
        }))
    }

    fn repo_blob(&self, full_name: &str, sha: &str) -> WireResult {
        if let Some(bytes) = self.cache.read().blob(full_name, sha) {
            return Ok(json!({ "bytes_b64": base64_encode(&bytes) }));
        }
        w(self.bb.blob(full_name, sha), |bytes| {
            self.cache.write().store_blob(full_name, sha, &bytes);
            json!({ "bytes_b64": base64_encode(&bytes) })
        })
    }

    fn repo_clone_url(&self, full_name: &str) -> WireResult {
        w(
            self.bb.repo(full_name),
            |repo: Repo| json!({ "clone_url": repo.clone_remote() }),
        )
    }

    fn repo_web_url(
        &self,
        full_name: &str,
        path: &str,
        branch: &str,
        line: Option<u64>,
        is_file: bool,
    ) -> WireResult {
        w(self.bb.repo(full_name), |repo: Repo| {
            let branch = if branch.is_empty() {
                repo.branch()
            } else {
                branch.to_string()
            };
            let mut url = if path.is_empty() {
                repo.web()
            } else {
                format!("{}/src/{branch}/{path}", repo.web())
            };
            if is_file && let Some(line) = line {
                url.push_str(&format!("#lines-{line}"));
            }
            json!({ "url": url })
        })
    }

    /// `search/code` for a forge without content search: `path:`
    /// queries over repo/org scope walk the tree and answer as legal
    /// **path-only hits** (v1.3). Content grep answers honestly.
    fn search_code(&self, params: &Value) -> WireResult {
        let q = params["q"].as_str().unwrap_or("");
        let (path_term, repo_scope, org_scope, extension) = parse_query(q);
        let Some(path_term) = path_term else {
            return Err(WireError {
                kind: "provider",
                message: "bitbucket cloud has no code-search API — use file find \
                          (leader f) or a path: query scoped to a repo or workspace"
                    .into(),
                retry_after_s: None,
            });
        };
        let repos: Vec<String> = if let Some(repo) = repo_scope {
            vec![repo]
        } else if let Some(org) = org_scope {
            self.bb
                .workspace_repos(&org)
                .map_err(|e| WireError::from_api(&e))?
                .iter()
                .map(|r| r.full_name.clone())
                .take(20)
                .collect()
        } else {
            return Err(WireError {
                kind: "provider",
                message: "path-only search needs a repo: or org: scope on bitbucket \
                          (no global index)"
                    .into(),
                retry_after_s: None,
            });
        };
        let needle = path_term.to_lowercase();
        let mut items = Vec::new();
        let mut truncated = false;
        for repo in repos {
            let Ok((branch, tree)) = self.tree_at_commit(&repo) else {
                continue;
            };
            for entry in &tree.entries {
                if entry.is_dir {
                    continue;
                }
                if !entry.path.to_lowercase().contains(&needle) {
                    continue;
                }
                if let Some(ext) = &extension
                    && !entry.path.to_lowercase().ends_with(&format!(".{ext}"))
                {
                    continue;
                }
                if items.len() >= 100 {
                    truncated = true;
                    break;
                }
                items.push(json!({
                    "repo": repo,
                    "path": entry.path,
                    "sha": entry.sha,
                    "branch": branch,
                    "matches": [],
                }));
            }
            if truncated {
                break;
            }
        }
        Ok(json!({ "items": items, "truncated": truncated }))
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Cache key for the branch→head mapping (trees fetched via a branch
/// name are the head at fetch time; the commit key holds the truth).
fn branch_key(meta: &RepoMeta) -> String {
    format!("branch-{}", meta.branch)
}

fn commit_key(commit: &str) -> String {
    commit.to_string()
}

/// Split a rootle code query: (path term, repo scope, org scope,
/// extension). Mirrors the qualifier grammar rootle emits.
fn parse_query(
    q: &str,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let mut path_term = None;
    let mut repo = None;
    let mut org = None;
    let mut extension = None;
    for token in q.split_whitespace() {
        if let Some(v) = token.strip_prefix("path:") {
            path_term = Some(v.to_string());
        } else if let Some(v) = token.strip_prefix("repo:") {
            repo = Some(v.to_string());
        } else if let Some(v) = token.strip_prefix("org:") {
            org = Some(v.to_string());
        } else if let Some(v) = token.strip_prefix("extension:") {
            extension = Some(v.trim_start_matches('.').to_lowercase());
        }
    }
    (path_term, repo, org, extension)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_grammar_splits_scopes() {
        let (path, repo, org, ext) = parse_query("path:parser repo:team/proj extension:rs");
        assert_eq!(path.as_deref(), Some("parser"));
        assert_eq!(repo.as_deref(), Some("team/proj"));
        assert_eq!(org, None);
        assert_eq!(ext.as_deref(), Some("rs"));
    }

    #[test]
    fn content_grep_without_scope_errors_honestly() {
        let h = Handler::new(
            "http://unused.invalid",
            "NOPE_TOKEN",
            "NOPE_USER",
            None,
            Vec::new(),
        );
        let err = h.search_code(&json!({ "q": "render" })).unwrap_err();
        assert_eq!(err.kind, "provider");
        assert!(err.message.contains("no code-search API"));
    }
}
