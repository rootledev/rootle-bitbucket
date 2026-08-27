//! `search/code` for a forge without a backend index: the query is
//! answered by walking the commit-pinned tree. `path:`/`extension:`
//! terms filter filenames and serve legal **path-only hits** (v1.3 —
//! empty `matches`); bare terms grep the fetched blobs through the
//! same cache a preview reads — binary-skipping, line-anchored,
//! bounded. Streaming (`"partial": true`, v1.3): one `$/partial` batch
//! per repo, then the metadata-only reply.

use super::{Handler, PartialSink, WireError, WireResult};
use serde_json::{Value, json};

/// Past this many hits the scan stops and reports `truncated: true`.
const HIT_CAP: usize = 100;
/// Repos walked per workspace scope — the walk is real work and an
/// unbounded scan would outrun the request deadline on real accounts.
const REPO_CAP: usize = 20;
/// Workspaces considered for an unscoped query.
const WORKSPACE_CAP: usize = 5;
/// Binary sniff window — a NUL this early means "not text, not grep".
const BINARY_SNIFF: usize = 8192;

impl Handler {
    pub(super) fn search_code(&self, params: &Value, partials: &mut PartialSink<'_>) -> WireResult {
        let query = parse_query(params["q"].as_str().unwrap_or(""));
        let repos = self.scope_repos(&query)?;
        let mut all: Vec<Value> = Vec::new();
        let mut truncated = false;
        for repo in repos {
            // A dead repo in a multi-repo scope yields fewer results,
            // not a failed search (one deleted repo must not kill the
            // whole org's answer).
            let Ok((branch, tree)) = self.tree_at_commit(&repo) else {
                continue;
            };
            let mut batch = Vec::new();
            for entry in &tree.entries {
                if entry.is_dir
                    || !query.extension_matches(&entry.path)
                    || !query.path_matches(&entry.path)
                {
                    continue;
                }
                let (matches, line) = if query.terms.is_empty() {
                    // Path-only hit (v1.3): "this file matched" — no
                    // blob fetch, no line anchor.
                    (Vec::new(), None)
                } else {
                    match self.grep_entry(&repo, &entry.sha, &query) {
                        Some(hit) => hit,
                        // Unreadable (over the preview cap), binary, or
                        // termless — the fs contract: skip, don't fail.
                        None => continue,
                    }
                };
                if all.len() + batch.len() >= HIT_CAP {
                    truncated = true;
                    break;
                }
                let mut item = json!({
                    "repo": repo,
                    "path": entry.path,
                    "sha": entry.sha,
                    "branch": branch,
                    "matches": matches,
                });
                if let Some(line) = line {
                    item["line"] = json!(line);
                }
                batch.push(item);
            }
            if !batch.is_empty() && partials.wants() {
                partials.send(&batch);
            }
            all.extend(batch);
            if truncated {
                break;
            }
        }
        // When the provider streamed, the reply is metadata-only
        // (items empty, truncated authoritative) — §Progressive
        // results; without `partial` the reply carries everything.
        let items = if partials.wants() { Vec::new() } else { all };
        Ok(json!({ "items": items, "truncated": truncated }))
    }

    /// Repos of the query's scope: the one repo, one workspace, or —
    /// unscoped — every configured (or discoverable) workspace,
    /// bounded. Configured workspaces first: a token scoped to
    /// repositories only can't run discovery (CHANGE-2770).
    fn scope_repos(&self, query: &Query) -> Result<Vec<String>, super::WireError> {
        if let Some(repo) = &query.repo {
            return Ok(vec![repo.clone()]);
        }
        let workspaces: Vec<String> = match &query.org {
            Some(org) => vec![org.clone()],
            None => {
                if !self.workspaces.is_empty() {
                    self.workspaces
                        .iter()
                        .take(WORKSPACE_CAP)
                        .cloned()
                        .collect()
                } else {
                    self.bb
                        .workspaces()
                        .map_err(WireError::from)?
                        .into_iter()
                        .take(WORKSPACE_CAP)
                        .map(|ws| ws.slug)
                        .collect()
                }
            }
        };
        let mut out = Vec::new();
        for slug in workspaces {
            // One dead workspace degrades to the others' results.
            let Ok(repos) = self.bb.workspace_repos(&slug) else {
                continue;
            };
            for repo in repos {
                if out.len() >= REPO_CAP {
                    return Ok(out);
                }
                out.push(repo.full_name);
            }
        }
        Ok(out)
    }

    /// Content match for one blob: `(matched terms, 1-based line of
    /// the first match)`. None = skip (unreadable or binary).
    fn grep_entry(
        &self,
        repo: &str,
        sha: &str,
        query: &Query,
    ) -> Option<(Vec<String>, Option<u64>)> {
        let bytes = self.blob_bytes(repo, sha).ok()?;
        if bytes.first() == Some(&0) || bytes[..bytes.len().min(BINARY_SNIFF)].contains(&0) {
            return None;
        }
        let text = String::from_utf8_lossy(&bytes);
        let lowered = text.to_lowercase();
        let matched: Vec<String> = query
            .terms
            .iter()
            .filter(|t| lowered.contains(t.as_str()))
            .cloned()
            .collect();
        if matched.is_empty() {
            return None;
        }
        // The anchor is the real line of the first match (v1.3): the
        // first substring occurrence in the whole file is often the
        // wrong one, which is exactly why the protocol carries line.
        let needle = matched[0].as_str();
        let line = lowered
            .lines()
            .position(|l| l.contains(needle))
            .map(|i| i as u64 + 1);
        Some((matched, line))
    }
}

/// A rootle code query: bare terms grep content, `path:` terms match
/// filenames (path-only hits), `repo:`/`org:` bound the scope,
/// `extension:` filters by suffix. Mirrors the qualifier grammar
/// rootle emits; everything is matched case-insensitively.
struct Query {
    terms: Vec<String>,
    path_terms: Vec<String>,
    repo: Option<String>,
    org: Option<String>,
    extension: Option<String>,
}

impl Query {
    fn extension_matches(&self, path: &str) -> bool {
        self.extension
            .as_ref()
            .is_none_or(|ext| path.to_lowercase().ends_with(&format!(".{ext}")))
    }

    fn path_matches(&self, path: &str) -> bool {
        let lowered = path.to_lowercase();
        self.path_terms.iter().all(|t| lowered.contains(t.as_str()))
    }
}

fn parse_query(q: &str) -> Query {
    let mut query = Query {
        terms: Vec::new(),
        path_terms: Vec::new(),
        repo: None,
        org: None,
        extension: None,
    };
    for token in q.split_whitespace() {
        if let Some(v) = token.strip_prefix("path:") {
            query.path_terms.push(v.to_lowercase());
        } else if let Some(v) = token.strip_prefix("repo:") {
            query.repo = Some(v.to_string());
        } else if let Some(v) = token.strip_prefix("org:") {
            query.org = Some(v.to_string());
        } else if let Some(v) = token.strip_prefix("extension:") {
            query.extension = Some(v.trim_start_matches('.').to_lowercase());
        } else {
            query.terms.push(token.to_lowercase());
        }
    }
    query
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::tests::{lines, result, result_ws, set_creds};
    use wiremock::matchers::{method, path, path_regex, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn query_grammar_splits_scopes() {
        let q = parse_query("path:Parser repo:team/proj extension:rs");
        assert_eq!(q.path_terms, ["parser"]);
        assert_eq!(q.repo.as_deref(), Some("team/proj"));
        assert_eq!(q.org, None);
        assert_eq!(q.extension.as_deref(), Some("rs"));
        assert!(q.terms.is_empty());
    }

    #[test]
    fn bare_terms_are_content_needles() {
        let q = parse_query("render org:x extension:.md");
        assert_eq!(q.terms, ["render"]);
        assert_eq!(q.org.as_deref(), Some("x"));
        assert_eq!(q.extension.as_deref(), Some("md"));
        assert!(q.path_terms.is_empty());
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
    async fn grep_hits_carry_real_lines_and_skip_binaries() {
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
            .and(path("/2.0/repositories/team/alpha/src/abc123/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "values": [
                    { "type": "commit_file", "path": "src/main.rs" },
                    { "type": "commit_file", "path": "icon.png" }
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/src/abc123/src/main.rs"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("fn main() {}\n// needle_main lives on line 2\n"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/src/abc123/icon.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0x89, 0x50, 0x00, 0x47]))
            .mount(&server)
            .await;
        let r = result(
            &server.uri(),
            "search/code",
            json!({ "q": "needle_main repo:team/alpha" }),
        );
        let items = r["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "binary blobs must be skipped: {items:?}");
        assert_eq!(items[0]["path"], "src/main.rs");
        assert_eq!(items[0]["matches"], json!(["needle_main"]));
        // The anchor is the real 1-based line of the first match.
        assert_eq!(items[0]["line"], 2);
    }

    #[tokio::test]
    async fn unscoped_extension_query_spans_workspaces() {
        // Unscoped = configured workspaces (discovery-free), each
        // walked for the extension's path-only hits.
        set_creds();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team"))
            .and(query_param("pagelen", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "values": [
                    { "full_name": "team/alpha", "mainbranch": { "name": "main" } },
                    { "full_name": "team/beta", "mainbranch": { "name": "main" } }
                ]
            })))
            .mount(&server)
            .await;
        for repo in ["alpha", "beta"] {
            Mock::given(method("GET"))
                .and(path(format!("/2.0/repositories/team/{repo}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "full_name": format!("team/{repo}"), "mainbranch": { "name": "main" }
                })))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path(format!(
                    "/2.0/repositories/team/{repo}/refs/branches/main"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "target": { "hash": format!("{repo}head") }
                })))
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path_regex(format!(
                    r"^/2\.0/repositories/team/{repo}/src/{repo}head/$"
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "values": [ { "type": "commit_file", "path": "notes.txt", "size": 6 } ]
                })))
                .mount(&server)
                .await;
        }
        let r = result_ws(
            &server.uri(),
            &["team"],
            "search/code",
            json!({ "q": "extension:txt" }),
        );
        let items = r["items"].as_array().unwrap();
        let repos: Vec<&str> = items.iter().map(|i| i["repo"].as_str().unwrap()).collect();
        assert_eq!(repos, ["team/alpha", "team/beta"], "items: {items:?}");
        for item in items {
            assert_eq!(item["matches"], json!([])); // path-only hits
        }
    }

    #[tokio::test]
    async fn streamed_search_emits_partials_then_metadata_reply() {
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
        // Without partial: one reply carrying everything (v1.2).
        let plain = result(
            &server.uri(),
            "search/code",
            json!({ "q": "path:parser repo:team/alpha" }),
        );
        assert_eq!(plain["items"].as_array().unwrap().len(), 1);
        // With partial: $/partial batches carrying the request id,
        // then a metadata-only reply (items empty, truncated
        // authoritative) — §Progressive results.
        let out = lines(
            &server.uri(),
            "search/code",
            json!({ "q": "path:parser repo:team/alpha", "partial": true }),
        );
        let partials: Vec<&Value> = out
            .iter()
            .filter(|m| m.get("method") == Some(&json!("$/partial")))
            .collect();
        let reply = out.last().unwrap();
        assert_eq!(partials.len(), 1, "lines: {out:?}");
        assert_eq!(partials[0]["params"]["id"], 1);
        assert_eq!(partials[0]["params"]["items"].as_array().unwrap().len(), 1);
        assert!(reply.get("result").is_some(), "lines: {out:?}");
        assert_eq!(reply["result"]["items"], json!([]));
        assert_eq!(reply["result"]["truncated"], false);
    }
}
