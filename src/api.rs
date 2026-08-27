//! Bitbucket Cloud REST 2.0 client: lazy credentials, error taxonomy
//! mapping, page aggregation, and the directory walk that replaces a
//! recursive tree endpoint (Bitbucket lists one directory per call).
//!
//! Content ids: Bitbucket's API exposes no git object ids, so this
//! adapter pins every listing and blob fetch to a commit hash and
//! uses `<commit>:<path>` as the content id — immutable for a pinned
//! commit (the protocol's requirement: ids change when content
//! changes — a different commit is a different id).

use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const DEFAULT_INSTANCE: &str = "https://api.bitbucket.org";
pub const DEFAULT_TOKEN_ENV: &str = "BITBUCKET_TOKEN";
pub const DEFAULT_USERNAME_ENV: &str = "BITBUCKET_USERNAME";

/// rootle refuses blobs over 1 MiB at its boundary; refusing here
/// first saves the transfer.
pub const BLOB_CAP: usize = 1024 * 1024;

/// The tree walk aggregates this many entries; past it the listing
/// reports `truncated: true`.
pub const TREE_ENTRY_CAP: usize = 25_000;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{message}")]
    Api {
        status: u16,
        message: String,
        retry_after: Option<u64>,
    },
    #[error("network: {0}")]
    Network(String),
}

pub type ApiResult<T> = Result<T, ApiError>;

pub struct Bitbucket {
    instance: String,
    token_env: String,
    username_env: String,
    http: reqwest::blocking::Client,
    token: std::sync::OnceLock<String>,
}

/// Repository (null-tolerant: private forks omit links fields; one odd
/// repo in a page must not fail the whole listing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repo {
    pub full_name: String,
    #[serde(default)]
    pub mainbranch: Option<Mainbranch>,
    #[serde(default)]
    pub links: Option<RepoLinks>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mainbranch {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoLinks {
    #[serde(default)]
    pub html: Option<Link>,
    #[serde(default)]
    pub clone_: Option<Vec<NamedLink>>,
    /// wire field: "clone"
    #[serde(rename = "clone")]
    pub clone_wire: Option<Vec<NamedLink>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub href: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamedLink {
    pub name: String,
    pub href: String,
}

impl Repo {
    /// "workspace/repo" → "repo" (rootle's org/repos wants names).
    pub fn name(&self) -> &str {
        self.full_name.rsplit('/').next().unwrap_or(&self.full_name)
    }
    pub fn branch(&self) -> String {
        self.mainbranch
            .as_ref()
            .map(|m| m.name.clone())
            .unwrap_or_else(|| "main".into())
    }
    pub fn web(&self) -> String {
        self.links
            .as_ref()
            .and_then(|l| l.html.as_ref())
            .map(|h| h.href.clone())
            .unwrap_or_else(|| format!("https://bitbucket.org/{}", self.full_name))
    }
    pub fn clone_remote(&self) -> String {
        self.links
            .as_ref()
            .and_then(|l| l.clone_wire.as_ref())
            .and_then(|cs| cs.iter().find(|c| c.name == "https").or_else(|| cs.first()))
            .map(|c| c.href.clone())
            .unwrap_or_else(|| format!("https://bitbucket.org/{}.git", self.full_name))
    }
}

#[derive(Debug, Deserialize)]
struct Paged<T> {
    values: Vec<T>,
    #[serde(default)]
    next: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Workspace {
    pub slug: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Ref {
    pub target: RefTarget,
}

#[derive(Debug, Deserialize)]
pub struct RefTarget {
    pub hash: String,
}

#[derive(Debug, Deserialize)]
pub struct SrcEntry {
    #[serde(rename = "type")]
    pub kind: String,
    pub path: String,
    #[serde(default)]
    pub size: Option<u64>,
}

impl SrcEntry {
    pub fn is_dir(&self) -> bool {
        self.kind == "commit_directory"
    }
}

impl Bitbucket {
    pub fn new(instance: &str, token_env: &str, username_env: &str) -> Self {
        Bitbucket {
            instance: instance.trim_end_matches('/').to_string(),
            token_env: token_env.to_string(),
            username_env: username_env.to_string(),
            http: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(25))
                .build()
                .expect("reqwest client"),
            token: std::sync::OnceLock::new(),
        }
    }

    /// Lazy credentials on first call (restart obligations: never at
    /// spawn). Bitbucket Cloud app passwords authenticate as Basic
    /// `username:password`; plain API tokens ride as Bearer. Username
    /// without token (or token without either scheme being usable) is
    /// an auth error, not a silent anonymous mode — Bitbucket's API
    /// is useless anonymous for browsing.
    fn auth_header(&self) -> ApiResult<String> {
        if let Some(header) = self.token.get() {
            return Ok(header.clone());
        }
        let token = std::env::var(&self.token_env)
            .ok()
            .filter(|t| !t.is_empty());
        let username = std::env::var(&self.username_env)
            .ok()
            .filter(|u| !u.is_empty());
        let header = match (token, username) {
            (Some(token), Some(user)) => format!(
                "Basic {}",
                base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    format!("{user}:{token}")
                )
            ),
            (Some(token), None) => format!("Bearer {token}"),
            (None, _) => {
                return Err(ApiError::Api {
                    status: 401,
                    message: format!(
                        "bitbucket needs credentials — set {} (app password) + {} (username), or {} for a bearer token",
                        self.token_env, self.username_env, self.token_env
                    ),
                    retry_after: None,
                });
            }
        };
        Ok(self.token.get_or_init(|| header).clone())
    }

    fn get<T: for<'de> Deserialize<'de>>(&self, path: &str) -> ApiResult<T> {
        let url = if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}{path}", self.instance)
        };
        let auth = self.auth_header()?;
        let resp = self
            .http
            .get(&url)
            .header("Authorization", auth)
            .send()
            .map_err(classify_send)?;
        let resp = classify_status(resp)?;
        resp.json::<T>()
            .map_err(|e| ApiError::Network(e.to_string()))
    }

    /// Raw bytes (blobs): plain GET on the raw src endpoint.
    fn get_bytes(&self, path: &str) -> ApiResult<Vec<u8>> {
        let url = format!("{}{path}", self.instance);
        let auth = self.auth_header()?;
        let resp = self
            .http
            .get(&url)
            .header("Authorization", auth)
            .send()
            .map_err(classify_send)?;
        let resp = classify_status(resp)?;
        let bytes = resp.bytes().map_err(|e| ApiError::Network(e.to_string()))?;
        if bytes.len() > BLOB_CAP {
            return Err(ApiError::Api {
                status: 413,
                message: format!("blob over the 1 MiB preview cap ({} bytes)", bytes.len()),
                retry_after: None,
            });
        }
        Ok(bytes.to_vec())
    }

    /// Aggregate every page of a paginated collection, up to `cap`
    /// values (past it: stop and report truncation via the flag the
    /// caller reads from the returned count).
    fn paged<T>(&self, first: &str, cap: usize) -> ApiResult<(Vec<T>, bool)>
    where
        T: for<'de> Deserialize<'de>,
    {
        let mut out = Vec::new();
        let mut next = Some(first.to_string());
        while let Some(url) = next.take() {
            let page: Paged<T> = self.get(&url)?;
            next = page.next;
            out.extend(page.values);
            if out.len() >= cap {
                out.truncate(cap);
                return Ok((out, true));
            }
        }
        Ok((out, false))
    }

    pub fn workspaces(&self) -> ApiResult<Vec<Workspace>> {
        let (ws, _) = self.paged("/2.0/workspaces?pagelen=100", 200)?;
        Ok(ws)
    }

    pub fn repo(&self, full_name: &str) -> ApiResult<Repo> {
        self.get(&format!("/2.0/repositories/{full_name}"))
    }

    pub fn workspace_repos(&self, workspace: &str) -> ApiResult<Vec<Repo>> {
        let (repos, _) = self.paged(
            &format!("/2.0/repositories/{workspace}?pagelen=100&sort=-updated_on"),
            500,
        )?;
        Ok(repos)
    }

    /// A repo's search surface for the launch popup: workspaces whose
    /// slug or name contains the query, plus repo names inside them.
    pub fn search(&self, query: &str) -> ApiResult<Vec<(Workspace, Vec<Repo>)>> {
        let q = query.to_lowercase();
        let mut out = Vec::new();
        for ws in self.workspaces()? {
            let matches = ws.slug.to_lowercase().contains(&q)
                || ws
                    .name
                    .as_deref()
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains(&q);
            if matches {
                let repos = self.workspace_repos(&ws.slug)?;
                out.push((ws, repos));
                if out.len() >= 5 {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Ref → commit hash (the content-id pin for trees and blobs).
    pub fn branch_head(&self, full_name: &str, branch: &str) -> ApiResult<String> {
        let r: Ref = self.get(&format!(
            "/2.0/repositories/{full_name}/refs/branches/{branch}"
        ))?;
        Ok(r.target.hash)
    }

    /// One directory listing at a pinned commit.
    fn src_dir(&self, full_name: &str, commit: &str, path: &str) -> ApiResult<Vec<SrcEntry>> {
        let suffix = if path.is_empty() {
            String::new()
        } else {
            format!("{}/", path.trim_end_matches('/'))
        };
        let (entries, _) = self.paged(
            &format!(
                "/2.0/repositories/{full_name}/src/{commit}/{suffix}?format=entries&pagelen=100"
            ),
            1000,
        )?;
        Ok(entries)
    }

    /// Recursive tree by walking directories (Bitbucket lists one
    /// level per call). Returns protocol tree entries + truncated.
    pub fn walk_tree(
        &self,
        full_name: &str,
        commit: &str,
    ) -> ApiResult<(Vec<crate::cache::TreeEntry>, bool)> {
        let mut out: Vec<crate::cache::TreeEntry> = Vec::new();
        let mut truncated = false;
        let mut queue: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        queue.push_back(String::new());
        while let Some(dir) = queue.pop_front() {
            for entry in self.src_dir(full_name, commit, &dir)? {
                if out.len() >= TREE_ENTRY_CAP {
                    truncated = true;
                    return Ok((out, truncated));
                }
                let path = if dir.is_empty() {
                    entry.path.clone()
                } else {
                    format!("{dir}/{}", entry.path)
                };
                if entry.is_dir() {
                    queue.push_back(path.clone());
                    out.push(crate::cache::TreeEntry {
                        path,
                        is_dir: true,
                        sha: commit.to_string(),
                        size: None,
                    });
                } else {
                    out.push(crate::cache::TreeEntry {
                        path: path.clone(),
                        is_dir: false,
                        // Commit-pinned content id (see module docs).
                        sha: format!("{commit}:{path}"),
                        size: entry.size,
                    });
                }
            }
        }
        Ok((out, truncated))
    }

    /// Blob bytes at a pinned commit path (sha = "<commit>:<path>").
    pub fn blob(&self, full_name: &str, sha: &str) -> ApiResult<Vec<u8>> {
        let Some((commit, path)) = sha.split_once(':') else {
            return Err(ApiError::Api {
                status: 400,
                message: format!("malformed content id {sha:?} — expected <commit>:<path>"),
                retry_after: None,
            });
        };
        self.get_bytes(&format!(
            "/2.0/repositories/{full_name}/raw/{commit}/{path}"
        ))
    }
}

fn classify_send(e: reqwest::Error) -> ApiError {
    ApiError::Network(e.to_string())
}

fn classify_status(resp: reqwest::blocking::Response) -> ApiResult<reqwest::blocking::Response> {
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(resp);
    }
    let retry_after = resp
        .headers()
        .get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let body: Option<serde_json::Value> = resp.json().ok();
    let message = body
        .as_ref()
        .and_then(|b| b.get("error"))
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("bitbucket api: status {status}"));
    Err(ApiError::Api {
        status,
        message,
        retry_after,
    })
}
