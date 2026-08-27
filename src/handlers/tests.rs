//! The wiremock harness for the handlers tests (shared by the
//! sibling submodules) plus the surface's own cases: the error
//! taxonomy and the missing-credentials path. Per-method wiremock
//! cases live next to the handlers they exercise.

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
pub(super) fn reply(uri: &str, method: &str, params: Value) -> Value {
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

pub(super) fn result(uri: &str, method: &str, params: Value) -> Value {
    let r = reply(uri, method, params);
    assert!(r.get("result").is_some(), "expected result, got: {r}");
    r["result"].clone()
}

pub(super) fn error(uri: &str, method: &str, params: Value) -> Value {
    let r = reply(uri, method, params);
    r["error"].clone()
}

/// Set once, process-wide (edition-2024 set_var is unsafe — fine in
/// tests, done exactly here, before any handler exists).
pub(super) fn error_with_env(
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
