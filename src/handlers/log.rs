//! `repo/log` (v1.5): commit history at a revision, newest first —
//! sha, subject line, author, ISO-8601 date, all verbatim from the
//! forge (rootle's history lens never re-derives authorship). `path`
//! filters to commits touching it; `limit` rides the bounded-compute
//! contract: stop at ~N, `truncated: true` past it.

use super::{Handler, WireResult, w};
use crate::api::Commit;
use serde_json::{Value, json};

/// History depth when the client sent no budget — what a history lens
/// renders usefully before the user narrows.
const DEFAULT_LIMIT: usize = 200;
/// The log never exceeds this, whatever `limit` says: bounded compute
/// cuts both ways (rootle's own render budget is 500).
const HARD_CAP: usize = 500;

impl Handler {
    pub(super) fn repo_log(
        &self,
        full_name: &str,
        path: Option<&str>,
        ref_: Option<&str>,
        limit: Option<u64>,
    ) -> WireResult {
        let limit = limit
            .unwrap_or(DEFAULT_LIMIT as u64)
            .clamp(1, HARD_CAP as u64) as usize;
        // The ref resolves first (unknown → a clean not_found naming
        // it); the commits listing is then pinned to a full hash.
        let include = self.resolve_commit(full_name, ref_)?;
        w(
            self.bb.commits(full_name, &include, path, limit),
            |(commits, truncated)| {
                json!({
                    "items": commits.iter().map(log_item).collect::<Vec<_>>(),
                    "truncated": truncated,
                })
            },
        )
    }
}

/// One wire item: sha, subject, author, date — the four fields the
/// history lens renders. Missing author/date (service commits)
/// degrade to empty strings, never a dropped item.
fn log_item(c: &Commit) -> Value {
    json!({
        "sha": c.hash,
        "subject": c.subject(),
        "author": c.author_raw(),
        "date": c.date.clone().unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use crate::handlers::tests::{error, result, set_creds};
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// The default-branch resolution mounts: repo entity + head.
    async fn mount_default_head(server: &MockServer, branch: &str, hash: &str) {
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "full_name": "team/alpha", "mainbranch": { "name": branch }
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/2.0/repositories/team/alpha/refs/branches/{branch}"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "target": { "hash": hash }
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn log_lists_commits_newest_first_with_subjects() {
        set_creds();
        let server = MockServer::start().await;
        mount_default_head(&server, "main", "abc123").await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/commits"))
            .and(query_param("include", "abc123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "values": [
                    {
                        "hash": "c2",
                        "message": "fix: the widget\n\nthe long body nobody\nwants in a list",
                        "author": { "raw": "Tarek <tarek@example.com>" },
                        "date": "2026-08-27T10:00:00+00:00"
                    },
                    {
                        "hash": "c1",
                        "message": "init",
                        "author": null,
                        "date": "2026-08-26T09:00:00+00:00"
                    }
                ]
            })))
            .mount(&server)
            .await;
        let r = result(&server.uri(), "repo/log", json!({ "repo": "team/alpha" }));
        let items = r["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        // Newest first, as the forge ordered them.
        assert_eq!(items[0]["sha"], "c2");
        // Subject is the first line, body dropped.
        assert_eq!(items[0]["subject"], "fix: the widget");
        assert_eq!(items[0]["author"], "Tarek <tarek@example.com>");
        assert_eq!(items[0]["date"], "2026-08-27T10:00:00+00:00");
        // A null author degrades to empty, never a dropped item.
        assert_eq!(items[1]["author"], "");
        assert_eq!(r["truncated"], false);
    }

    #[tokio::test]
    async fn log_filters_by_path_and_stops_at_the_limit() {
        set_creds();
        let server = MockServer::start().await;
        mount_default_head(&server, "main", "abc123").await;
        // pagelen rides the limit (bounded compute); the path filter
        // passes through verbatim. A full page with a `next` link is
        // provably more history: truncated.
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/commits"))
            .and(query_param("include", "abc123"))
            .and(query_param("path", "src/main.rs"))
            .and(query_param("pagelen", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "values": [
                    { "hash": "c2", "message": "touch the file",
                      "author": { "raw": "T <t@x.y>" }, "date": "2026-08-27T10:00:00+00:00" }
                ],
                "next": "https://api.example.org/2.0/repositories/team/alpha/commits?include=abc123&pagelen=1&page=2"
            })))
            .mount(&server)
            .await;
        let r = result(
            &server.uri(),
            "repo/log",
            json!({ "repo": "team/alpha", "path": "src/main.rs", "limit": 1 }),
        );
        assert_eq!(r["items"].as_array().unwrap().len(), 1);
        assert_eq!(r["truncated"], true);
    }

    #[tokio::test]
    async fn a_page_landing_exactly_on_the_limit_without_next_is_complete() {
        set_creds();
        let server = MockServer::start().await;
        mount_default_head(&server, "main", "abc123").await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/commits"))
            .and(query_param("include", "abc123"))
            .and(query_param("pagelen", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "values": [
                    { "hash": "c2", "message": "two",
                      "author": { "raw": "T <t@x.y>" }, "date": "2026-08-27T10:00:00+00:00" },
                    { "hash": "c1", "message": "one",
                      "author": { "raw": "T <t@x.y>" }, "date": "2026-08-26T10:00:00+00:00" }
                ]
            })))
            .mount(&server)
            .await;
        let r = result(
            &server.uri(),
            "repo/log",
            json!({ "repo": "team/alpha", "limit": 2 }),
        );
        // Exactly the budget, no `next`: nothing was dropped.
        assert_eq!(r["items"].as_array().unwrap().len(), 2);
        assert_eq!(r["truncated"], false);
    }

    #[tokio::test]
    async fn log_at_a_commit_sha_pins_without_probes() {
        set_creds();
        let server = MockServer::start().await;
        let sha = "0123456789abcdef0123456789abcdef01234567";
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/commits"))
            .and(query_param("include", sha))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "values": [
                    { "hash": sha, "message": "the commit itself",
                      "author": { "raw": "T <t@x.y>" }, "date": "2026-08-27T10:00:00+00:00" }
                ]
            })))
            .mount(&server)
            .await;
        let r = result(
            &server.uri(),
            "repo/log",
            json!({ "repo": "team/alpha", "ref": sha }),
        );
        assert_eq!(r["items"][0]["sha"], sha);
    }

    #[tokio::test]
    async fn an_unknown_ref_is_not_found() {
        set_creds();
        let server = MockServer::start().await;
        for p in [
            "/2.0/repositories/team/alpha/refs/branches/nope",
            "/2.0/repositories/team/alpha/refs/tags/nope",
        ] {
            Mock::given(method("GET"))
                .and(path(p))
                .respond_with(ResponseTemplate::new(404))
                .mount(&server)
                .await;
        }
        let e = error(
            &server.uri(),
            "repo/log",
            json!({ "repo": "team/alpha", "ref": "nope" }),
        );
        assert_eq!(e["data"]["kind"], "not_found");
    }
}
