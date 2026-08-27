//! Blob fetch: content by the tree's pinned content id
//! (`<commit>:<path>`), cache-first (`repo/blob`) — and `repo/blob_at`
//! (v1.5), a path resolved at a revision straight to bytes + id. The
//! same cache-first path backs the content grep in `code.rs`.

use super::{Handler, WireResult, w};
use serde_json::json;

impl Handler {
    pub(super) fn repo_blob(&self, full_name: &str, sha: &str) -> WireResult {
        w(
            self.blob_bytes(full_name, sha),
            |bytes| json!({ "bytes_b64": base64_encode(&bytes) }),
        )
    }

    /// `repo/blob_at` (v1.5): a path resolved at a revision, straight
    /// to bytes + the content id — the "open the file at this commit"
    /// call. The id stays in the `<commit>:<path>` grammar, so it
    /// round-trips through `repo/blob` and the tree walk; unknown
    /// path or ref lands as a not_found.
    pub(super) fn repo_blob_at(
        &self,
        full_name: &str,
        path: &str,
        ref_: Option<&str>,
    ) -> WireResult {
        let commit = self.resolve_commit(full_name, ref_)?;
        let sha = format!("{commit}:{path}");
        w(
            self.blob_bytes(full_name, &sha),
            |bytes| json!({ "bytes_b64": base64_encode(&bytes), "sha": sha }),
        )
    }

    /// Blob bytes by content id, disk-cache first. Shared by
    /// `repo/blob` and the content grep (a grep pass warms the same
    /// cache a later preview reads).
    pub(super) fn blob_bytes(&self, full_name: &str, sha: &str) -> crate::api::ApiResult<Vec<u8>> {
        if let Some(bytes) = self.cache.read().blob(full_name, sha) {
            return Ok(bytes);
        }
        let bytes = self.bb.blob(full_name, sha)?;
        self.cache.write().store_blob(full_name, sha, &bytes);
        Ok(bytes)
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use crate::handlers::tests::{error, result, set_creds};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    #[tokio::test]
    async fn a_sha_outside_the_grammar_is_not_found() {
        // The grep-era ids are "<commit>:<path>"; an id we could never
        // have served is a missing blob (FC-062), not a transport
        // failure — rootle maps the taxonomy from data.kind.
        set_creds();
        let server = MockServer::start().await;
        let e = error(
            &server.uri(),
            "repo/blob",
            json!({ "repo": "team/alpha", "sha": "0".repeat(64) }),
        );
        assert_eq!(e["data"]["kind"], "not_found");
    }

    #[tokio::test]
    async fn blob_at_serves_bytes_and_a_grammar_sha() {
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
                "target": { "hash": "abc123" }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/src/abc123/src/main.rs"))
            .respond_with(ResponseTemplate::new(200).set_body_string("fn main() {}"))
            .mount(&server)
            .await;
        let at = result(
            &server.uri(),
            "repo/blob_at",
            json!({ "repo": "team/alpha", "path": "src/main.rs" }),
        );
        assert_eq!(at["sha"], "abc123:src/main.rs");
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(at["bytes_b64"].as_str().unwrap())
            .unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "fn main() {}");
        // The minted id is servable by `repo/blob` — the grammar
        // round-trips (a fresh handler against the same backend).
        let again = result(
            &server.uri(),
            "repo/blob",
            json!({ "repo": "team/alpha", "sha": "abc123:src/main.rs" }),
        );
        assert_eq!(again["bytes_b64"], at["bytes_b64"]);
    }

    #[tokio::test]
    async fn blob_at_an_explicit_tag_ref_pins_the_tagged_commit() {
        set_creds();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/refs/branches/v2.0"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/refs/tags/v2.0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "target": { "hash": "tagged9" }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/src/tagged9/lib.rs"))
            .respond_with(ResponseTemplate::new(200).set_body_string("pub fn f() {}"))
            .mount(&server)
            .await;
        let at = result(
            &server.uri(),
            "repo/blob_at",
            json!({ "repo": "team/alpha", "path": "lib.rs", "ref": "v2.0" }),
        );
        assert_eq!(at["sha"], "tagged9:lib.rs");
    }

    #[tokio::test]
    async fn blob_at_an_unknown_path_or_ref_is_not_found() {
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
                "target": { "hash": "abc123" }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/src/abc123/gone.rs"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/refs/branches/nope"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/refs/tags/nope"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let missing_path = error(
            &server.uri(),
            "repo/blob_at",
            json!({ "repo": "team/alpha", "path": "gone.rs" }),
        );
        assert_eq!(missing_path["data"]["kind"], "not_found");
        let missing_ref = error(
            &server.uri(),
            "repo/blob_at",
            json!({ "repo": "team/alpha", "path": "lib.rs", "ref": "nope" }),
        );
        assert_eq!(missing_ref["data"]["kind"], "not_found");
    }
}
