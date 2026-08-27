//! `search/code` for a forge without content search: `path:` queries
//! over repo/org scope walk the tree and answer as legal
//! **path-only hits** (v1.3). Content grep answers honestly.

use super::{Handler, WireError, WireResult};
use serde_json::{Value, json};

impl Handler {
    pub(super) fn search_code(&self, params: &Value) -> WireResult {
        let q = params["q"].as_str().unwrap_or("");
        let (path_term, repo_scope, org_scope, extension) = parse_query(q);
        let Some(path_term) = path_term else {
            return Err(WireError {
                kind: "provider",
                message: "bitbucket cloud has no code-search API — use file find \
                          (leader f) or a path: query scoped to a repo or workspace"
                    .into(),
                retry_after_s: None,
            });
        };
        let repos: Vec<String> = if let Some(repo) = repo_scope {
            vec![repo]
        } else if let Some(org) = org_scope {
            self.bb
                .workspace_repos(&org)
                .map_err(|e| WireError::from_api(&e))?
                .iter()
                .map(|r| r.full_name.clone())
                .take(20)
                .collect()
        } else {
            return Err(WireError {
                kind: "provider",
                message: "path-only search needs a repo: or org: scope on bitbucket \
                          (no global index)"
                    .into(),
                retry_after_s: None,
            });
        };
        let needle = path_term.to_lowercase();
        let mut items = Vec::new();
        let mut truncated = false;
        for repo in repos {
            let Ok((branch, tree)) = self.tree_at_commit(&repo) else {
                continue;
            };
            for entry in &tree.entries {
                if entry.is_dir {
                    continue;
                }
                if !entry.path.to_lowercase().contains(&needle) {
                    continue;
                }
                if let Some(ext) = &extension
                    && !entry.path.to_lowercase().ends_with(&format!(".{ext}"))
                {
                    continue;
                }
                if items.len() >= 100 {
                    truncated = true;
                    break;
                }
                items.push(json!({
                    "repo": repo,
                    "path": entry.path,
                    "sha": entry.sha,
                    "branch": branch,
                    "matches": [],
                }));
            }
            if truncated {
                break;
            }
        }
        Ok(json!({ "items": items, "truncated": truncated }))
    }
}

/// Split a rootle code query: (path term, repo scope, org scope,
/// extension). Mirrors the qualifier grammar rootle emits.
fn parse_query(
    q: &str,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let mut path_term = None;
    let mut repo = None;
    let mut org = None;
    let mut extension = None;
    for token in q.split_whitespace() {
        if let Some(v) = token.strip_prefix("path:") {
            path_term = Some(v.to_string());
        } else if let Some(v) = token.strip_prefix("repo:") {
            repo = Some(v.to_string());
        } else if let Some(v) = token.strip_prefix("org:") {
            org = Some(v.to_string());
        } else if let Some(v) = token.strip_prefix("extension:") {
            extension = Some(v.trim_start_matches('.').to_lowercase());
        }
    }
    (path_term, repo, org, extension)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::tests::{result, set_creds};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn query_grammar_splits_scopes() {
        let (path, repo, org, ext) = parse_query("path:parser repo:team/proj extension:rs");
        assert_eq!(path.as_deref(), Some("parser"));
        assert_eq!(repo.as_deref(), Some("team/proj"));
        assert_eq!(org, None);
        assert_eq!(ext.as_deref(), Some("rs"));
    }

    #[test]
    fn content_grep_without_scope_errors_honestly() {
        let h = Handler::new(
            "http://unused.invalid",
            "NOPE_TOKEN",
            "NOPE_USER",
            None,
            Vec::new(),
        );
        let err = h.search_code(&json!({ "q": "render" })).unwrap_err();
        assert_eq!(err.kind, "provider");
        assert!(err.message.contains("no code-search API"));
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
}
