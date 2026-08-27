//! Blob fetch: content by the tree's pinned content id
//! (`<commit>:<path>`), cache-first (`repo/blob`).

use super::{Handler, WireResult, w};
use serde_json::json;

impl Handler {
    pub(super) fn repo_blob(&self, full_name: &str, sha: &str) -> WireResult {
        if let Some(bytes) = self.cache.read().blob(full_name, sha) {
            return Ok(json!({ "bytes_b64": base64_encode(&bytes) }));
        }
        w(self.bb.blob(full_name, sha), |bytes| {
            self.cache.write().store_blob(full_name, sha, &bytes);
            json!({ "bytes_b64": base64_encode(&bytes) })
        })
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use crate::handlers::tests::{result, set_creds};
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
}
