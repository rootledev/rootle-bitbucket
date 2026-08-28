//! forge-conformance harness: this process IS the provider — it
//! serves rootledev/forge-conformance's canonical fixture (three
//! repos, `alpha`, `beta`, `vcs`) through the real adapter
//! (`rootle_bitbucket`) against an in-process Bitbucket Cloud REST
//! 2.0 mock backed by the fixture directory.
//!
//!     PROVIDER=target/debug/examples/forge_conformance python3 run
//!
//! (the suite appends the materialized fixture dir as the final argv
//! element and exports FORGE_FIXTURE_DIR; FORGE_ORG names the
//! workspace, default `local`.)
//!
//! Two backends behind the same endpoints:
//!
//! - plain directories (`alpha`, `beta`): served as files, with
//!   content-derived "commits" (FNV-1a over the sorted files) so an
//!   FC-011 mutation moves the head and with it every
//!   `<commit>:<path>` content id;
//! - real git repos (`vcs`, built by the suite's materializer): refs,
//!   trees, blobs, and log served from git itself via the `git` CLI —
//!   the suite computes its expectations from the same repo, so the
//!   answers are git's answers (FC-090..FC-098, protocol v1.5).
//!
//! Everything is computed per request; nothing is snapshotted at
//! startup. Credentials are satisfied in-process with dummies (the
//! mock ignores them): the adapter's no-anonymous guard is about real
//! Bitbucket, and the suite scrubs credential vars from the child env
//! by contract (FC-052).

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

/// The endpoint families the adapter speaks to: workspace repo
/// listings, single repos, ref collections + single refs, commits
/// (log), and src (directory entries vs raw file bytes — the
/// format=entries query is the discriminator, mirroring the real
/// API).
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
            r"^/2\.0/repositories/([^/]+)/([^/]+)/refs/(branches|tags)$",
        ))
        .respond_with(move |req: &Request| ref_collection(req.url.path(), &root3))
        .mount(server)
        .await;

    let root4 = root.to_path_buf();
    Mock::given(method("GET"))
        .and(path_regex(
            r"^/2\.0/repositories/([^/]+)/([^/]+)/refs/(branches|tags)/([^/]+)$",
        ))
        .respond_with(move |req: &Request| single_ref(req.url.path(), &root4))
        .mount(server)
        .await;

    let root5 = root.to_path_buf();
    Mock::given(method("GET"))
        .and(path_regex(r"^/2\.0/repositories/([^/]+)/([^/]+)/commits$"))
        .respond_with(move |req: &Request| commits(req, &root5))
        .mount(server)
        .await;

    let (root6, org6) = (root.to_path_buf(), org.to_string());
    Mock::given(method("GET"))
        .and(path_regex(
            r"^/2\.0/repositories/([^/]+)/([^/]+)/src/([^/]+)(/.*)?$",
        ))
        .respond_with(move |req: &Request| src(req, &root6, &org6))
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

/// `/2.0/repositories/{org}/{repo}` — mainbranch drives the default
/// ref; links keep the URL methods honest. Git repos report their
/// checked-out branch (the materialized vcs ends on main).
fn single_repo(path: &str, root: &Path, org: &str) -> ResponseTemplate {
    let Some((_, repo)) = split_repo(path, org) else {
        return not_found();
    };
    let dir = repo_dir(root, &repo);
    if !dir.is_dir() {
        return not_found();
    }
    let branch = git_text(&dir, &["symbolic-ref", "--short", "HEAD"])
        .map(|b| b.trim().to_string())
        .unwrap_or_else(|| "main".to_string());
    ResponseTemplate::new(200)
        .set_body_json(repo_json_with_branch(&format!("{org}/{repo}"), &branch))
}

/// `/2.0/repositories/{org}/{repo}/refs/{branches|tags}` — the ref
/// listings (v1.5 `repo/refs`). Git-backed repos answer from git;
/// plain repos have no refs to list.
fn ref_collection(path: &str, root: &Path) -> ResponseTemplate {
    let Some((repo, kind)) = split_ref_path(path) else {
        return not_found();
    };
    let dir = repo_dir(root, &repo);
    if !dir.is_dir() {
        return not_found();
    }
    let values: Vec<Value> = ref_names(&dir, kind)
        .into_iter()
        .filter_map(|name| {
            let full = format!("{}/{name}", git_ref_prefix(kind));
            git_text(
                &dir,
                &["rev-parse", "--verify", &format!("{full}^{{commit}}")],
            )
            .map(|sha| {
                let sha = sha.trim().to_string();
                json!({ "name": name, "target": { "hash": sha } })
            })
        })
        .collect();
    ResponseTemplate::new(200).set_body_json(json!({ "values": values }))
}

/// `/2.0/repositories/{org}/{repo}/refs/{kind}/{name}` — one ref's
/// commit (the adapter's ref-resolution probe order). Git-backed
/// repos deref through git; plain repos answer only the branch head
/// (the content-derived one).
fn single_ref(path: &str, root: &Path) -> ResponseTemplate {
    let Some((repo, kind, name)) = split_single_ref(path) else {
        return not_found();
    };
    let dir = repo_dir(root, &repo);
    if !dir.is_dir() {
        return not_found();
    }
    let hash = if is_git(&dir) {
        let full = format!("{}/{name}", git_ref_prefix(kind));
        git_text(
            &dir,
            &["rev-parse", "--verify", &format!("{full}^{{commit}}")],
        )
        .map(|s| s.trim().to_string())
    } else if kind == "branches" {
        commit_for(&dir)
    } else {
        None
    };
    match hash {
        Some(hash) => ResponseTemplate::new(200).set_body_json(json!({
            "target": { "hash": hash }
        })),
        None => not_found(),
    }
}

/// `/2.0/repositories/{org}/{repo}/commits?include=&pagelen=&path=`
/// — the log (v1.5 `repo/log`), newest first, honestly paginated:
/// a `next` link rides every page that is not the whole history (the
/// adapter's truncation reads it).
fn commits(req: &Request, root: &Path) -> ResponseTemplate {
    let path = req.url.path();
    let Some(repo) = path
        .strip_prefix("/2.0/repositories/")
        .and_then(|rest| rest.split('/').nth(1))
        .map(percent_decode)
    else {
        return not_found();
    };
    let dir = repo_dir(root, &repo);
    if !is_git(&dir) {
        return not_found();
    }
    let query = query_map(req);
    let include = query.get("include").cloned().unwrap_or_default();
    if git_text(
        &dir,
        &["rev-parse", "--verify", &format!("{include}^{{commit}}")],
    )
    .is_none()
    {
        return not_found();
    }
    let pagelen: usize = query
        .get("pagelen")
        .and_then(|p| p.parse().ok())
        .unwrap_or(100)
        .max(1);
    let page: usize = query.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
    let history_path = query.get("path").map(|p| percent_decode(p));

    let mut total_args: Vec<&str> = vec!["rev-list", "--count", &include];
    if let Some(p) = history_path.as_deref() {
        total_args.extend(["--", p]);
    }
    let total: usize = git_text(&dir, &total_args)
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0);

    let log_args: Vec<String> = vec![
        "log".into(),
        "--format=%H%x00%s%x00%an <%ae>%x00%aI".into(),
        format!("--skip={}", (page - 1) * pagelen),
        format!("-n{pagelen}"),
        include.clone(),
    ];
    let mut log_args_ref: Vec<&str> = log_args.iter().map(String::as_str).collect();
    if let Some(p) = history_path.as_deref() {
        log_args_ref.extend(["--", p]);
    }
    let values: Vec<Value> = git_text(&dir, &log_args_ref)
        .map(|out| {
            out.lines()
                .filter(|l| !l.trim().is_empty())
                .map(|line| {
                    let mut parts = line.split('\u{0}');
                    let hash = parts.next().unwrap_or_default();
                    let subject = parts.next().unwrap_or_default();
                    let author = parts.next().unwrap_or_default();
                    let date = parts.next().unwrap_or_default();
                    json!({
                        "hash": hash,
                        "message": subject,
                        "author": { "raw": author },
                        "date": date,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let mut body = json!({ "values": values });
    if page * pagelen < total {
        let mut next = format!(
            "/2.0/repositories/{repo}/commits?include={include}&pagelen={pagelen}&page={}",
            page + 1
        );
        if let Some(p) = history_path.as_deref() {
            next.push_str("&path=");
            next.push_str(p);
        }
        body["next"] = json!(next);
    }
    ResponseTemplate::new(200).set_body_json(body)
}

/// `/2.0/repositories/{org}/{repo}/src/{commit}[/{path}]` — directory
/// entries (format=entries, paths repo-root-relative as the live API
/// serves them) or the raw file bytes. Git-backed repos answer at the
/// COMMIT (ls-tree / cat-file); plain repos serve the directory and
/// derive heads from content.
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
    // First segment after /src/ is the commit the ref endpoints handed
    // out; the rest is the repo-relative path.
    let (commit, rel) = match after.split_once('/') {
        Some((c, r)) => (c.to_string(), percent_decode(r)),
        None => (after.to_string(), String::new()),
    };
    let dir = repo_dir(root, &repo);
    if is_git(&dir) {
        // A commit git cannot verify is a missing tree (FC-092's sha
        // probe 404s in walk_tree, not just at resolution).
        if git_text(
            &dir,
            &["rev-parse", "--verify", &format!("{commit}^{{commit}}")],
        )
        .is_none()
        {
            return not_found();
        }
        let rel = rel.trim_end_matches('/');
        if req
            .url
            .query()
            .is_some_and(|q| q.contains("format=entries"))
        {
            return git_dir_listing(&dir, &commit, rel);
        }
        let spec = format!("{commit}:{rel}");
        return match git_bytes(&dir, &["show", &spec]) {
            Some(bytes) => ResponseTemplate::new(200).set_body_bytes(bytes),
            None => not_found(),
        };
    }
    // Plain backend: no history — serve the CURRENT content.
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

/// `({repo}, {branches|tags})` from a refs-collection path.
fn split_ref_path(path: &str) -> Option<(String, &'static str)> {
    let rest = path.strip_prefix("/2.0/repositories/")?;
    let mut it = rest.split('/');
    let _ws = it.next()?;
    let repo = it.next()?;
    if it.next()? != "refs" {
        return None;
    }
    let kind = match it.next()? {
        "branches" => "branches",
        "tags" => "tags",
        _ => return None,
    };
    if it.next().is_some() {
        return None;
    }
    Some((percent_decode(repo), kind))
}

/// `({repo}, {kind}, {name})` from a single-ref path.
fn split_single_ref(path: &str) -> Option<(String, &'static str, String)> {
    let rest = path.strip_prefix("/2.0/repositories/")?;
    let mut it = rest.split('/');
    let _ws = it.next()?;
    let repo = it.next()?;
    if it.next()? != "refs" {
        return None;
    }
    let kind = match it.next()? {
        "branches" => "branches",
        "tags" => "tags",
        _ => return None,
    };
    let name = it.next()?.to_string();
    if it.next().is_some() {
        return None;
    }
    Some((percent_decode(repo), kind, name))
}

/// One directory level of a plain (non-git) repo, paths
/// repo-root-relative.
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
        if name == ".git" {
            continue;
        }
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

/// `git ls-tree -l` at a commit: one level, repo-root-relative paths.
fn git_dir_listing(dir: &Path, commit: &str, rel: &str) -> ResponseTemplate {
    let prefix = if rel.is_empty() {
        String::new()
    } else {
        format!("{rel}/")
    };
    let mut args: Vec<String> = vec!["ls-tree".into(), "-l".into(), commit.to_string()];
    if !prefix.is_empty() {
        args.push(prefix);
    }
    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    let Some(out) = git_text(dir, &args_ref) else {
        return not_found();
    };
    let values: Vec<Value> = out
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            // <mode> <type> <object> <size>\t<path>
            let (meta, path) = line.split_once('\t')?;
            let mut fields = meta.split_whitespace();
            let _mode = fields.next()?;
            let kind = fields.next()?;
            let is_dir = kind == "tree";
            let size: u64 = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            if is_dir {
                Some(json!({ "type": "commit_directory", "path": path }))
            } else {
                Some(json!({ "type": "commit_file", "path": path, "size": size }))
            }
        })
        .collect();
    ResponseTemplate::new(200).set_body_json(json!({ "values": values }))
}

fn not_found() -> ResponseTemplate {
    ResponseTemplate::new(404).set_body_json(json!({ "error": { "message": "not found" } }))
}

fn repo_json(full_name: &str) -> Value {
    repo_json_with_branch(full_name, "main")
}

fn repo_json_with_branch(full_name: &str, branch: &str) -> Value {
    json!({
        "full_name": full_name,
        "mainbranch": { "name": branch },
        "links": {
            "html": { "href": format!("https://bitbucket.org/{full_name}") },
            "clone": [
                { "name": "https", "href": format!("https://bitbucket.org/{full_name}.git") }
            ]
        }
    })
}

/// `{org}/{repo}` from a `/2.0/repositories/{org}/{repo}` path (the
/// part before /src/, /refs/, or /commits). None when the workspace
/// mismatches.
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

fn repo_dir(root: &Path, repo: &str) -> PathBuf {
    root.join(repo)
}

fn repo_dirs(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A repo the suite's materializer built with git (fixture/vcs).
fn is_git(dir: &Path) -> bool {
    dir.join(".git").exists()
}

fn git_text(dir: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_bytes(dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout)
}

fn ref_names(dir: &Path, kind: &str) -> Vec<String> {
    let prefix = format!("{}/", git_ref_prefix(kind));
    let Some(out) = git_text(dir, &["for-each-ref", &prefix, "--format=%(refname:short)"]) else {
        return Vec::new();
    };
    out.lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The wire says `refs/branches`; git keeps branches under
/// `refs/heads` (Bitbucket's URL grammar predates git's layout).
fn git_ref_prefix(kind: &str) -> &'static str {
    match kind {
        "branches" => "refs/heads",
        _ => "refs/tags",
    }
}

/// The query string as a map (first occurrence wins; keys and values
/// percent-decoded).
fn query_map(req: &Request) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    if let Some(q) = req.url.query() {
        for pair in q.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            map.entry(percent_decode(k))
                .or_insert_with(|| percent_decode(v));
        }
    }
    map
}

/// Content-derived head: FNV-1a over the sorted repo files (path,
/// length, bytes). Any content change moves the "commit", which is
/// the whole plain-dir backend — there is no history to serve, only
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
        if name == ".git" {
            continue;
        }
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
