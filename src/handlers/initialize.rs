//! The handshake: protocol version, the capability split, the icon —
//! and re-rooting the cache at rootle's `cache_dir`.

use super::{Handler, WireResult};
use crate::cache::Cache;
use serde_json::{Value, json};

impl Handler {
    pub(super) fn initialize(&self, params: &Value) -> WireResult {
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
                "file_search": true,
                // Revision awareness (v1.5): branch/tag listings and
                // commit history are served; blame is not —
                // Bitbucket Cloud has no blame API, and false is the
                // honest answer (the UI hides the lens).
                "refs": true,
                "log": true,
                "blame": false
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use crate::handlers::tests::result;
    use serde_json::json;
    use wiremock::MockServer;

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
        // v1.5 revision trio: refs and log served, blame honestly
        // absent (no Bitbucket Cloud blame API).
        assert_eq!(r["capabilities"]["refs"], true);
        assert_eq!(r["capabilities"]["log"], true);
        assert_eq!(r["capabilities"]["blame"], false);
    }
}
