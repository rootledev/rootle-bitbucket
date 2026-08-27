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
use std::sync::atomic::Ordering;
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

/// Page size for the commits listing (the endpoint's own maximum) —
/// `repo/log` aggregates pages up to the caller's limit.
const COMMITS_PAGELEN: usize = 100;

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
pub struct UserWorkspace {
    pub workspace: Workspace,
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

/// A branch or tag listing entry (`repo/refs`, v1.5). The default
/// marker rides the repo entity's mainbranch, not this listing.
#[derive(Debug, Deserialize)]
pub struct NamedRef {
    pub name: String,
    pub target: RefTarget,
}

/// One commit of the history listing (`repo/log`, v1.5). Author and
/// date are null-tolerant (service commits can lack an author); the
/// date is ISO-8601 as the forge reports it — rootle's history lens
/// takes it verbatim.
#[derive(Debug, Deserialize)]
pub struct Commit {
    pub hash: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub author: Option<CommitAuthor>,
    #[serde(default)]
    pub date: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CommitAuthor {
    /// "Name <email>" as the forge reports the author.
    pub raw: String,
}

impl Commit {
    /// The subject line (one line per history item).
    pub fn subject(&self) -> String {
        self.message.lines().next().unwrap_or("").to_string()
    }

    pub fn author_raw(&self) -> &str {
        self.author.as_ref().map(|a| a.raw.as_str()).unwrap_or("")
    }
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
    /// caller reads from the returned count). Truncated means
    /// provably incomplete: the backend signalled more (`next`), or
    /// a single page overflowed the cap — a page landing exactly on
    /// the cap with no `next` is complete history.
    fn paged<T>(&self, first: &str, cap: usize) -> ApiResult<(Vec<T>, bool)>
    where
        T: for<'de> Deserialize<'de>,
    {
        let mut out = Vec::new();
        let mut next = Some(first.to_string());
        while let Some(url) = next.take() {
            let page: Paged<T> = self.get(&url)?;
            out.extend(page.values);
            if out.len() >= cap {
                let truncated = page.next.is_some() || out.len() > cap;
                out.truncate(cap);
                return Ok((out, truncated));
            }
            next = page.next;
        }
        Ok((out, false))
    }

    /// A user's workspaces (CHANGE-2770 world: the cross-workspace
    /// listings are gone; this is the sanctioned replacement). Needs
    /// the account read scope — tokens scoped to repositories only
    /// get a 403, and callers fall back to configured workspaces.
    pub fn workspaces(&self) -> ApiResult<Vec<Workspace>> {
        let (pages, _) = self.paged::<UserWorkspace>("/2.0/user/workspaces?pagelen=100", 200)?;
        Ok(pages.into_iter().map(|p| p.workspace).collect())
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
    /// Ref → commit hash (the content-id pin for trees and blobs).
    pub fn branch_head(&self, full_name: &str, branch: &str) -> ApiResult<String> {
        let r: Ref = self.get(&format!(
            "/2.0/repositories/{full_name}/refs/branches/{branch}"
        ))?;
        Ok(r.target.hash)
    }

    /// Branches with their heads (`repo/refs`, v1.5). The 500-ref
    /// budget mirrors the repo listing cap — enough for real repos,
    /// bounded for the request deadline.
    pub fn branches(&self, full_name: &str) -> ApiResult<Vec<NamedRef>> {
        let (refs, _) = self.paged(
            &format!("/2.0/repositories/{full_name}/refs/branches?pagelen=100"),
            500,
        )?;
        Ok(refs)
    }

    /// Tags with the commits they mark (`repo/refs`, v1.5) — a tag's
    /// target hash IS the commit it was created at.
    pub fn tags(&self, full_name: &str) -> ApiResult<Vec<NamedRef>> {
        let (refs, _) = self.paged(
            &format!("/2.0/repositories/{full_name}/refs/tags?pagelen=100"),
            500,
        )?;
        Ok(refs)
    }

    /// Resolve a user-facing revision — branch, tag, or commit sha —
    /// to the commit hash every content id pins to (v1.5). A
    /// well-formed sha is taken at face value (the pinned fetch 404s
    /// if it doesn't exist — shas arrive from our own listings and
    /// no forge survives a 40-hex branch name); otherwise branch,
    /// then tag; anything else is a not_found naming the ref.
    pub fn resolve_ref(&self, full_name: &str, ref_: &str) -> ApiResult<String> {
        if looks_like_commit_sha(ref_) {
            return Ok(ref_.to_string());
        }
        for kind in ["branches", "tags"] {
            match self.get::<Ref>(&format!("/2.0/repositories/{full_name}/refs/{kind}/{ref_}")) {
                Ok(r) => return Ok(r.target.hash),
                // Not this kind of ref — try the next.
                Err(ApiError::Api { status: 404, .. }) => {}
                Err(e) => return Err(e),
            }
        }
        Err(ApiError::Api {
            status: 404,
            message: format!("unknown ref {ref_:?} in {full_name}"),
            retry_after: None,
        })
    }

    /// Commit history at an include point, optionally filtered to a
    /// path, newest first (`repo/log`, v1.5). `include` is a commit
    /// hash — the handler resolves refs first so unknown refs are a
    /// clean not_found. The cap rides the bounded-compute contract:
    /// stop at ~limit, report truncation past it.
    pub fn commits(
        &self,
        full_name: &str,
        include: &str,
        path: Option<&str>,
        limit: usize,
    ) -> ApiResult<(Vec<Commit>, bool)> {
        let mut first = format!(
            "/2.0/repositories/{full_name}/commits?include={include}&pagelen={}",
            limit.min(COMMITS_PAGELEN)
        );
        if let Some(p) = path.filter(|p| !p.is_empty()) {
            first.push_str("&path=");
            first.push_str(&encode_query(p));
        }
        self.paged(&first, limit)
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
    /// level per call), eight directories in flight — a sequential
    /// walk costs one round trip per directory and outruns rootle's
    /// request deadline on any real repo. Entries are repo-root-
    /// relative (verified live). Returns protocol entries + truncated.
    pub fn walk_tree(
        &self,
        full_name: &str,
        commit: &str,
    ) -> ApiResult<(Vec<crate::cache::TreeEntry>, bool)> {
        use std::collections::VecDeque;
        use std::sync::atomic::AtomicBool;

        const WORKERS: usize = 8;
        let queue: std::sync::Mutex<VecDeque<String>> = std::sync::Mutex::new(VecDeque::new());
        queue.lock().unwrap().push_back(String::new());
        let out: std::sync::Mutex<Vec<crate::cache::TreeEntry>> = std::sync::Mutex::new(Vec::new());
        let truncated = AtomicBool::new(false);
        let dead: std::sync::Mutex<Option<ApiError>> = std::sync::Mutex::new(None);

        let active = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..WORKERS {
                scope.spawn(|| {
                    loop {
                        if truncated.load(Ordering::Relaxed) || dead.lock().unwrap().is_some() {
                            return;
                        }
                        // Drain semantics: exit only when the queue is
                        // empty AND nobody is mid-fetch (their results
                        // may enqueue more directories) — a plain
                        // pop-or-return collapses the pool to one
                        // worker on the first directory.
                        let dir = {
                            let mut q = queue.lock().unwrap();
                            loop {
                                match q.pop_front() {
                                    Some(d) => break d,
                                    None if active.load(Ordering::Relaxed) == 0 => return,
                                    None => {
                                        drop(q);
                                        std::thread::sleep(Duration::from_millis(5));
                                        q = queue.lock().unwrap();
                                    }
                                }
                            }
                        };
                        active.fetch_add(1, Ordering::Relaxed);
                        match self.src_dir(full_name, commit, &dir) {
                            Ok(entries) => {
                                let mut q = queue.lock().unwrap();
                                let mut o = out.lock().unwrap();
                                for entry in entries {
                                    if o.len() >= TREE_ENTRY_CAP {
                                        truncated.store(true, Ordering::Relaxed);
                                        return;
                                    }
                                    let is_dir = entry.is_dir();
                                    if is_dir {
                                        q.push_back(entry.path.clone());
                                    }
                                    let sha = if is_dir {
                                        commit.to_string()
                                    } else {
                                        format!("{commit}:{}", entry.path)
                                    };
                                    o.push(crate::cache::TreeEntry {
                                        path: entry.path,
                                        is_dir,
                                        sha,
                                        size: entry.size,
                                    });
                                }
                            }
                            Err(e) => {
                                *dead.lock().unwrap() = Some(e);
                                return;
                            }
                        }
                        active.fetch_sub(1, Ordering::Relaxed);
                    }
                });
            }
        });

        if let Some(e) = dead.into_inner().unwrap() {
            return Err(e);
        }
        Ok((out.into_inner().unwrap(), truncated.load(Ordering::Relaxed)))
    }

    /// Blob bytes at a pinned commit path (sha = "<commit>:<path>").
    pub fn blob(&self, full_name: &str, sha: &str) -> ApiResult<Vec<u8>> {
        let Some((commit, path)) = sha.split_once(':') else {
            // A sha outside our grammar can never have been served —
            // that is a missing blob, not a transport failure (the
            // taxonomy is mapped from the status).
            return Err(ApiError::Api {
                status: 404,
                message: format!("no such blob {sha:?} — content ids are <commit>:<path>"),
                retry_after: None,
            });
        };
        // /src/<commit>/<path> returns the raw bytes for files (the
        // dedicated /raw/ route no longer exists).
        self.get_bytes(&format!(
            "/2.0/repositories/{full_name}/src/{commit}/{path}"
        ))
    }
}

fn classify_send(e: reqwest::Error) -> ApiError {
    ApiError::Network(e.to_string())
}

/// A git commit sha as this forge shapes them: 40 hex (sha1) or 64
/// hex (sha256 repos).
fn looks_like_commit_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Percent-encode a query-param value (unreserved plus `/` and `.`
/// stay literal — paths read better in logs, servers decode alike).
fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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
