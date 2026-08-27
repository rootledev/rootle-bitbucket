//! The wiremock harness for the handlers tests (shared by the
//! sibling submodules) plus the surface's own cases: the error
//! taxonomy and the missing-credentials path. Per-method wiremock
//! cases live next to the handlers they exercise.

use crate::handlers::WireError;
use crate::{Handler, respond};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Unique temp cache per handler — tests must never share (or touch)
/// the real provider cache.
pub(super) fn temp_cache() -> std::path::PathBuf {
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
fn exchange(
    uri: &str,
    token_env: &str,
    user_env: &str,
    workspaces: &[&str],
    line: &str,
) -> Vec<Value> {
    let cache = temp_cache();
    let out = std::thread::scope(|s| {
        s.spawn(move || {
            let workspaces = workspaces.iter().map(|w| w.to_string()).collect();
            let h = Handler::new(uri, token_env, user_env, Some(cache), workspaces);
            let mut emitted = Vec::new();
            respond(&h, line, &mut |s| emitted.push(s));
            emitted
        })
        .join()
        .unwrap()
    });
    out.iter()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}

/// Sequential requests through ONE handler (one cache) — the shape
/// staleness bugs live in: a later request must observe what the
/// backend changed since the earlier one.
pub(super) fn results(uri: &str, method: &str, params: &[Value]) -> Vec<Value> {
    let lines: Vec<String> = params
        .iter()
        .map(|params| {
            json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string()
        })
        .collect();
    let cache = temp_cache();
    let raw = std::thread::scope(|s| {
        s.spawn(move || {
            let h = Handler::new(uri, "TEST_TOKEN", "TEST_USER", Some(cache), Vec::new());
            let mut emitted = Vec::new();
            for line in lines {
                respond(&h, &line, &mut |s| emitted.push(s));
            }
            emitted
        })
        .join()
        .unwrap()
    });
    raw.iter()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}

/// Every wire message one request elicits, in order — `$/partial`
/// batches (when the request opted in) followed by the reply. The
/// reply is the last line (§Progressive results: partials precede).
pub(super) fn lines(uri: &str, method: &str, params: Value) -> Vec<Value> {
    let line = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string();
    exchange(uri, "TEST_TOKEN", "TEST_USER", &[], &line)
}

pub(super) fn reply(uri: &str, method: &str, params: Value) -> Value {
    lines(uri, method, params).pop().expect("a reply line")
}

pub(super) fn result(uri: &str, method: &str, params: Value) -> Value {
    let r = reply(uri, method, params);
    assert!(r.get("result").is_some(), "expected result, got: {r}");
    r["result"].clone()
}

/// Like [`result`], with configured workspaces (the discovery-free
/// path CHANGE-2770 tokens must take).
pub(super) fn result_ws(uri: &str, workspaces: &[&str], method: &str, params: Value) -> Value {
    let line = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string();
    let r = exchange(uri, "TEST_TOKEN", "TEST_USER", workspaces, &line)
        .pop()
        .expect("a reply line");
    assert!(r.get("result").is_some(), "expected result, got: {r}");
    r["result"].clone()
}

pub(super) fn error(uri: &str, method: &str, params: Value) -> Value {
    let r = reply(uri, method, params);
    r["error"].clone()
}

/// Set once, process-wide (edition-2024 set_var is unsafe — fine in
/// tests, done exactly here, before any handler exists).
pub(super) fn set_creds() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        std::env::set_var("TEST_TOKEN", "app-password");
        std::env::set_var("TEST_USER", "someone");
    });
}

#[tokio::test]
async fn missing_credentials_is_an_auth_error_not_a_panic() {
    let server = MockServer::start().await;
    // The vars are distinct from set_creds' — absence is the default.
    let line =
        json!({ "jsonrpc": "2.0", "id": 1, "method": "org/repos", "params": { "org": "team" } })
            .to_string();
    let r = exchange(
        &server.uri(),
        "MISSING_TOKEN_XYZ",
        "MISSING_USER_XYZ",
        &[],
        &line,
    )
    .pop()
    .unwrap();
    let e = r["error"].clone();
    assert_eq!(e["data"]["kind"], "auth");
    assert!(e["message"].as_str().unwrap().contains("credentials"));
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

#[test]
fn the_preview_cap_is_provider_policy_not_a_transport_fault() {
    // The 1 MiB refusal is ours, made before the transfer — the
    // taxonomy says provider, not network (the UI's network bucket
    // would suggest retrying).
    let e = WireError::from_api(&crate::api::ApiError::Api {
        status: 413,
        message: "blob over the 1 MiB preview cap".into(),
        retry_after: None,
    });
    assert_eq!(e.kind, "provider");
}

#[tokio::test]
async fn blame_is_the_honest_unknown_method() {
    // Bitbucket Cloud has no blame API: the handshake says
    // blame:false and dispatch has no repo/blame arm. The
    // unknown-method error is the correct reply — a stub that
    // fake-succeeds would lie to the history lens.
    let server = MockServer::start().await;
    let e = error(
        &server.uri(),
        "repo/blame",
        json!({ "repo": "team/alpha", "path": "lib.rs" }),
    );
    assert_eq!(e["data"]["kind"], "provider");
    let msg = e["message"].as_str().unwrap();
    assert!(msg.contains("repo/blame"), "names the method: {msg}");
}
