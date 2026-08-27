//! forge-conformance harness: this process IS the provider — it
//! serves rootledev/forge-conformance's canonical fixture (two repos,
//! `alpha` and `beta`) through the real adapter (`rootle_bitbucket`)
//! against an in-process Bitbucket Cloud REST 2.0 mock backed by the
//! fixture directory.
//!
//!     PROVIDER=target/debug/examples/forge_conformance python3 run
//!
//! (the suite appends the materialized fixture dir as the final argv
//! element and exports FORGE_FIXTURE_DIR; FORGE_ORG names the
//! workspace, default `local`.)
//!
//! The mock's "commits" are content-derived (FNV-1a over the sorted
//! repo files): mutating a fixture file moves the head, so the
//! adapter's `<commit>:<path>` content ids move with it — exactly the
//! semantics FC-011 pins. Everything is computed per request; nothing
//! is snapshotted at startup. Credentials are satisfied in-process
//! with dummies (the mock ignores them): the adapter's no-anonymous
//! guard is about real Bitbucket, and the suite scrubs credential
//! vars from the child env by contract (FC-052).

use rootle_bitbucket::{Handler, respond};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// In-process stand-ins satisfying the client's lazy-credential guard
/// (the fixture backend never looks at them).
const TOKEN_ENV: &str = "ROOTLE_BITBUCKET_FIXTURE_TOKEN";
const USER_ENV: &str = "ROOTLE_BITBUCKET_FIXTURE_USER";

fn main() {
    let fixture = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("FORGE_FIXTURE_DIR").ok())
        .expect("usage: forge_conformance <fixture-dir> (appended by the suite)");
    let org = std::env::var("FORGE_ORG").unwrap_or_else(|_| "local".to_string());
    // Before any thread exists — the documented-safe set_var window.
    unsafe {
        std::env::set_var(TOKEN_ENV, "fixture");
        std::env::set_var(USER_ENV, "fixture");
    }

    // The mock server needs a live tokio runtime for as long as the
    // provider runs; the stdio loop below is plain blocking code on
    // the main thread.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let root: PathBuf = fixture.into();
    let ws = org.clone();
    std::thread::Builder::new()
        .name("fixture-server".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("fixture runtime");
            rt.block_on(async move {
                let server = MockServer::start().await;
                mount(&server, &root, &ws).await;
                tx.send(server.uri()).expect("uri handoff");
                // Hold the server (and its runtime) for the process
                // lifetime — the provider loop never ends on its own.
                std::future::pending::<()>().await
            });
        })
        .expect("fixture thread");
    let uri = rx.recv().expect("fixture server uri");

    let handler = Handler::new(&uri, TOKEN_ENV, USER_ENV, None, vec![org]);
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    use std::io::BufRead;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        respond(&handler, &line, &mut |reply| {
            println!("{reply}");
            use std::io::Write;
            let _ = out.flush();
        });
    }
}

/// The four endpoint families the adapter speaks to: workspace repo
/// listings, single repos, branch heads, and src (directory entries
/// vs raw file bytes — the format=entries query is the discriminator,
/// mirroring the real API).
async fn mount(server: &MockServer, root: &Path, org: &str) {
    let (root1, org1) = (root.to_path_buf(), org.to_string());
    Mock::given(method("GET"))
        .and(path_regex(r"^/2\.0/repositories/([^/]+)$"))
        .respond_with(move |req: &Request| repo_listing(req.url.path(), &root1, &org1))
        .mount(server)
        .await;

    let (root2, org2) = (root.to_path_buf(), org.to_string());
    Mock::given(method("GET"))
        .and(path_regex(r"^/2\.0/repositories/([^/]+)/([^/]+)$"))
        .respond_with(move |req: &Request| single_repo(req.url.path(), &root2, &org2))
        .mount(server)
        .await;

    let root3 = root.to_path_buf();
    Mock::given(method("GET"))
        .and(path_regex(
            r"^/2\.0/repositories/([^/]+)/([^/]+)/refs/branches/([^/]+)$",
        ))
        .respond_with(move |req: &Request| branch_head(req.url.path(), &root3))
        .mount(server)
        .await;

    let (root4, org4) = (root.to_path_buf(), org.to_string());
    Mock::given(method("GET"))
        .and(path_regex(
            r"^/2\.0/repositories/([^/]+)/([^/]+)/src/([^/]+)(/.*)?$",
        ))
        .respond_with(move |req: &Request| src(req, &root4, &org4))
        .mount(server)
        .await;
}

/// `/2.0/repositories/{org}` — every repo dir under the fixture.
fn repo_listing(path: &str, root: &Path, org: &str) -> ResponseTemplate {
    let workspace = path.rsplit('/').next().unwrap_or_default();
    if workspace != org {
        return not_found();
    }
    let values: Vec<Value> = repo_dirs(root)
        .into_iter()
        .map(|name| repo_json(&format!("{org}/{name}")))
        .collect();
    ResponseTemplate::new(200).set_body_json(json!({ "values": values }))
}

/// `/2.0/repositories/{org}/{repo}` — mainbranch drives the head
/// resolution; links keep the URL methods honest.
fn single_repo(path: &str, root: &Path, org: &str) -> ResponseTemplate {
    let Some((_, repo)) = split_repo(path, org) else {
        return not_found();
    };
    if !repo_dir(root, &repo).is_dir() {
        return not_found();
    }
    ResponseTemplate::new(200).set_body_json(repo_json(&format!("{org}/{repo}")))
}

/// `/2.0/repositories/{org}/{repo}/refs/branches/{branch}` — the head
/// is derived from the repo's CURRENT content, per request.
fn branch_head(path: &str, root: &Path) -> ResponseTemplate {
    let Some(repo) = split_repo_head(path) else {
        return not_found();
    };
    let Some(commit) = commit_for(&repo_dir(root, &repo)) else {
        return not_found();
    };
    ResponseTemplate::new(200).set_body_json(json!({ "target": { "hash": commit } }))
}

/// `/2.0/repositories/{org}/{repo}/src/{commit}[/{path}]` — directory
/// entries (format=entries, paths repo-root-relative as the live API
/// serves them) or the raw file bytes.
fn src(req: &Request, root: &Path, org: &str) -> ResponseTemplate {
    let path = req.url.path();
    // Anchor on the FIRST /src/ — the repo itself has a `src/`
    // directory, so a naive split chases its own tail.
    let Some(pos) = path.find("/src/") else {
        return not_found();
    };
    let Some((_, repo)) = split_repo(&path[..pos], org) else {
        return not_found();
    };
    let after = &path[pos + "/src/".len()..];
    // First segment after /src/ is the commit the branch-head endpoint
    // handed out; the rest is the repo-relative path. The mock has no
    // history — it always serves the CURRENT content, so a stale
    // commit simply resolves to now.
    let rel = after
        .split_once('/')
        .map(|(_, r)| percent_decode(r))
        .unwrap_or_default();
    let dir = repo_dir(root, &repo);
    if req
        .url
        .query()
        .is_some_and(|q| q.contains("format=entries"))
    {
        return dir_listing(&dir, rel.trim_end_matches('/'));
    }
    // Raw file bytes (blobs). A trailing-slash path with no format
    // query never occurs from the adapter.
    match std::fs::read(dir.join(rel.trim_end_matches('/'))) {
        Ok(bytes) => ResponseTemplate::new(200).set_body_bytes(bytes),
        Err(_) => not_found(),
    }
}

/// One directory level at a pinned commit, paths repo-root-relative.
fn dir_listing(dir: &Path, rel: &str) -> ResponseTemplate {
    let full = if rel.is_empty() {
        dir.to_path_buf()
    } else {
        dir.join(rel)
    };
    let Ok(entries) = std::fs::read_dir(&full) else {
        return not_found();
    };
    let mut rows: Vec<(String, bool, u64)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = if rel.is_empty() {
            name
        } else {
            format!("{rel}/{name}")
        };
        match entry.file_type() {
            Ok(t) if t.is_dir() => rows.push((path, true, 0)),
            Ok(_) => {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                rows.push((path, false, size));
            }
            Err(_) => continue,
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let values: Vec<Value> = rows
        .into_iter()
        .map(|(path, is_dir, size)| {
            if is_dir {
                json!({ "type": "commit_directory", "path": path })
            } else {
                json!({ "type": "commit_file", "path": path, "size": size })
            }
        })
        .collect();
    ResponseTemplate::new(200).set_body_json(json!({ "values": values }))
}

fn not_found() -> ResponseTemplate {
    ResponseTemplate::new(404).set_body_json(json!({ "error": { "message": "not found" } }))
}

fn repo_json(full_name: &str) -> Value {
    json!({
        "full_name": full_name,
        "mainbranch": { "name": "main" },
        "links": {
            "html": { "href": format!("https://bitbucket.org/{full_name}") },
            "clone": [
                { "name": "https", "href": format!("https://bitbucket.org/{full_name}.git") }
            ]
        }
    })
}

/// `{org}/{repo}` from a `/2.0/repositories/{org}/{repo}` path (the
/// part before /src/ or /refs/). None when the workspace mismatches.
fn split_repo(path: &str, org: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix("/2.0/repositories/")?;
    let mut it = rest.split('/');
    let ws = it.next()?;
    let repo = it.next()?;
    if ws != org || it.next().is_some() {
        return None;
    }
    Some((ws.to_string(), percent_decode(repo)))
}

/// The repo segment of a `/2.0/repositories/{org}/{repo}/refs/...`
/// path.
fn split_repo_head(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/2.0/repositories/")?;
    let mut it = rest.split('/');
    let ws = it.next()?;
    let repo = it.next()?;
    if it.next()? != "refs" || it.next()? != "branches" {
        return None;
    }
    let _ = ws;
    Some(percent_decode(repo))
}

fn repo_dir(root: &Path, repo: &str) -> PathBuf {
    root.join(repo)
}

fn repo_dirs(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| {
            e.file_type().is_ok_and(|t| t.is_dir())
                && !SKIP_REPO_DIRS.contains(&e.file_name().to_string_lossy().as_ref())
        })
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Top-level directories never served as repos: the suite's lifecycle
/// group roots the adapter's disk cache at `<fixture>/cache` — a
/// real-cache adapter materializes it, and serving it back would make
/// the walk chase its own cache (the reference adapter skips exactly
/// this class of directory via SKIP_DIRS).
const SKIP_REPO_DIRS: &[&str] = &["cache"];

/// Content-derived head: FNV-1a over the sorted repo files (path,
/// length, bytes). Any content change moves the "commit", which is
/// the whole fixture backend — there is no history to serve, only
/// now. Deterministic across processes (FC-013) and sensitive to
/// mutations (FC-011).
fn commit_for(dir: &Path) -> Option<String> {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    collect_files(dir, "", &mut files);
    if files.is_empty() {
        return None;
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (path, bytes) in &files {
        mix(&mut h, path.as_bytes());
        mix(&mut h, &[0]);
        mix(&mut h, &(bytes.len() as u64).to_le_bytes());
        mix(&mut h, bytes);
    }
    Some(format!("{h:016x}"))
}

fn collect_files(dir: &Path, prefix: &str, out: &mut Vec<(String, Vec<u8>)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            collect_files(&entry.path(), &rel, out);
        } else if let Ok(bytes) = std::fs::read(entry.path()) {
            out.push((rel, bytes));
        }
    }
}

fn mix(h: &mut u64, data: &[u8]) {
    for &b in data {
        *h ^= u64::from(b);
        *h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

/// Minimal %XX decoding for the paths reqwest percent-encodes
/// (unicode filenames in the fixture, e.g. `ünïcode.rs`).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
