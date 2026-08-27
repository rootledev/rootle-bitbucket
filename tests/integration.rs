//! Wiremock-backed integration: the protocol surface against a mocked
//! Bitbucket REST 2.0 — auth schemes, the tree walk's pagination and
//! directory recursion, path-only search, and the error taxonomy.

use rootle_bitbucket::{Handler, respond};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Unique temp cache per handler — tests must never share (or touch)
/// the real provider cache.
fn temp_cache() -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "rootle-bb-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The handler runs on a scoped thread: its blocking reqwest client
/// owns an internal tokio runtime that must not drop inside the
/// async test context (the rootle-gitlab suite's pattern).
fn reply(uri: &str, method: &str, params: Value) -> Value {
    let line = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string();
    let cache = temp_cache();
    let out = std::thread::scope(|s| {
        s.spawn(move || {
            let h = Handler::new(uri, "TEST_TOKEN", "TEST_USER", Some(cache), Vec::new());
            respond(&h, &line).unwrap()
        })
        .join()
        .unwrap()
    });
    serde_json::from_str(&out).unwrap()
}

fn result(uri: &str, method: &str, params: Value) -> Value {
    let r = reply(uri, method, params);
    assert!(r.get("result").is_some(), "expected result, got: {r}");
    r["result"].clone()
}

fn error(uri: &str, method: &str, params: Value) -> Value {
    let r = reply(uri, method, params);
    r["error"].clone()
}

/// Set once, process-wide (edition-2024 set_var is unsafe — fine in
/// tests, done exactly here, before any handler exists).
fn error_with_env(
    uri: &str,
    token_env: &str,
    user_env: &str,
    method: &str,
    params: Value,
) -> Value {
    let line = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string();
    let cache = temp_cache();
    let out = std::thread::scope(|s| {
        s.spawn(move || {
            let h = Handler::new(uri, token_env, user_env, Some(cache), Vec::new());
            respond(&h, &line).unwrap()
        })
        .join()
        .unwrap()
    });
    let r: Value = serde_json::from_str(&out).unwrap();
    r["error"].clone()
}

fn set_creds() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        std::env::set_var("TEST_TOKEN", "app-password");
        std::env::set_var("TEST_USER", "someone");
    });
}

#[tokio::test]
async fn initialize_declares_the_split_and_icon() {
    let server = MockServer::start().await;
    let r = result(&server.uri(), "initialize", json!({ "protocol": 1 }));
    assert_eq!(r["protocol"], 1);
    assert_eq!(r["name"], "bitbucket");
    assert_eq!(r["icon"], "bitbucket");
    assert_eq!(r["capabilities"]["code_search"], false);
    assert_eq!(r["capabilities"]["file_search"], true);
    assert_eq!(r["capabilities"]["orgs"], true);
}

#[tokio::test]
async fn missing_credentials_is_an_auth_error_not_a_panic() {
    let server = MockServer::start().await;
    // The vars are distinct from set_creds' — absence is the default.
    let e = error_with_env(
        &server.uri(),
        "MISSING_TOKEN_XYZ",
        "MISSING_USER_XYZ",
        "org/repos",
        json!({ "org": "team" }),
    );
    assert_eq!(e["data"]["kind"], "auth");
    assert!(e["message"].as_str().unwrap().contains("credentials"));
}

#[tokio::test]
async fn org_repos_strips_the_workspace_prefix() {
    set_creds();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/2.0/repositories/team"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "values": [
                { "full_name": "team/alpha", "mainbranch": { "name": "main" } },
                { "full_name": "team/beta" }
            ]
        })))
        .mount(&server)
        .await;
    let r = result(&server.uri(), "org/repos", json!({ "org": "team" }));
    assert_eq!(r["repos"], json!(["alpha", "beta"]));
}

#[tokio::test]
async fn tree_walk_recurses_directories_and_pins_content_ids() {
    set_creds();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/2.0/repositories/team/alpha"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "full_name": "team/alpha", "mainbranch": { "name": "master" }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/2.0/repositories/team/alpha/refs/branches/master"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "target": { "hash": "abc123" }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/2.0/repositories/team/alpha/src/abc123/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "values": [
                { "type": "commit_directory", "path": "src" },
                { "type": "commit_file", "path": "README.md", "size": 12 }
            ]
        })))
        .mount(&server)
        .await;
    // Entries are repo-root-relative (verified live): listing
    // /src/abc123/src/ returns "src/main.rs".
    Mock::given(method("GET"))
        .and(path("/2.0/repositories/team/alpha/src/abc123/src/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "values": [
                { "type": "commit_file", "path": "src/main.rs", "size": 40 }
            ]
        })))
        .mount(&server)
        .await;
    let r = result(&server.uri(), "repo/tree", json!({ "repo": "team/alpha" }));
    assert_eq!(r["branch"], "master");
    assert_eq!(r["truncated"], false);
    let entries = r["entries"].as_array().unwrap();
    let paths: Vec<&str> = entries
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"README.md"));
    assert!(paths.contains(&"src"));
    assert!(paths.contains(&"src/main.rs"));
    // Files pin to <commit>:<path>; dirs carry the commit.
    let main = entries.iter().find(|e| e["path"] == "src/main.rs").unwrap();
    assert_eq!(main["sha"], "abc123:src/main.rs");
    assert_eq!(main["type"], "blob");
}

#[tokio::test]
async fn path_search_serves_path_only_hits() {
    set_creds();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/2.0/repositories/team/alpha"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "full_name": "team/alpha", "mainbranch": { "name": "main" }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/2.0/repositories/team/alpha/refs/branches/main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "target": { "hash": "deadbeef" }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/2.0/repositories/team/alpha/src/deadbeef/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "values": [
                { "type": "commit_file", "path": "parser.rs" },
                { "type": "commit_file", "path": "main.rs" }
            ]
        })))
        .mount(&server)
        .await;
    let r = result(
        &server.uri(),
        "search/code",
        json!({ "q": "path:parser repo:team/alpha" }),
    );
    let items = r["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["path"], "parser.rs");
    assert_eq!(items[0]["matches"], json!([])); // path-only hit (v1.3)
    assert_eq!(items[0]["sha"], "deadbeef:parser.rs");
    assert_eq!(r["truncated"], false);
}

#[tokio::test]
async fn rate_limits_map_to_the_taxonomy() {
    set_creds();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/2.0/repositories/team"))
        .respond_with(ResponseTemplate::new(429).append_header("Retry-After", "37"))
        .mount(&server)
        .await;
    let e = error(&server.uri(), "org/repos", json!({ "org": "team" }));
    assert_eq!(e["data"]["kind"], "rate_limited");
    assert_eq!(e["data"]["retry_after_s"], 37);
}

#[tokio::test]
async fn clone_and_web_urls_come_from_links() {
    set_creds();
    let server = MockServer::start().await;
    let repo_json = json!({
        "full_name": "team/alpha",
        "mainbranch": { "name": "main" },
        "links": {
            "html": { "href": "https://bitbucket.org/team/alpha" },
            "clone": [
                { "name": "https", "href": "https://bitbucket.org/team/alpha.git" },
                { "name": "ssh", "href": "git@bitbucket.org:team/alpha.git" }
            ]
        }
    });
    Mock::given(method("GET"))
        .and(path("/2.0/repositories/team/alpha"))
        .respond_with(ResponseTemplate::new(200).set_body_json(repo_json))
        .mount(&server)
        .await;
    let clone = result(
        &server.uri(),
        "repo/clone_url",
        json!({ "repo": "team/alpha" }),
    );
    assert_eq!(clone["clone_url"], "https://bitbucket.org/team/alpha.git");
    let web = result(
        &server.uri(),
        "repo/web_url",
        json!({ "repo": "team/alpha", "path": "src/main.rs", "branch": "main", "line": 42, "is_file": true }),
    );
    assert_eq!(
        web["url"],
        "https://bitbucket.org/team/alpha/src/main/src/main.rs#lines-42"
    );
}

#[tokio::test]
async fn blob_round_trips_through_the_content_id() {
    set_creds();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/2.0/repositories/team/alpha/src/abc123/src/main.rs"))
        .respond_with(ResponseTemplate::new(200).set_body_string("fn main() {}"))
        .mount(&server)
        .await;
    let r = result(
        &server.uri(),
        "repo/blob",
        json!({ "repo": "team/alpha", "sha": "abc123:src/main.rs" }),
    );
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(r["bytes_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), "fn main() {}");
}
