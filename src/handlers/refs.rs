//! `repo/refs` (v1.5): the branch and tag listings with their commit
//! hashes — the revision picker's data. The default marker rides the
//! repo entity's mainbranch (repo meta is cached and lazily
//! revalidated; tree.rs owns that machinery).

use super::{Handler, WireResult};
use serde_json::json;

impl Handler {
    pub(super) fn repo_refs(&self, full_name: &str) -> WireResult {
        let branches = self.bb.branches(full_name)?;
        let tags = self.bb.tags(full_name)?;
        // Two statements, deliberately: the read guard is a temporary
        // that must drop before revalidate takes the write lock
        // (parking_lot deadlocks otherwise).
        let cached = self.cache.read().repo_meta(full_name);
        let default_branch = cached
            .or_else(|| self.revalidate_repo(full_name).ok())
            .map(|m| m.branch);
        Ok(json!({
            // `default` marks at most one branch — present only on
            // the match, absent everywhere else (optional per item).
            "branches": branches
                .iter()
                .map(|b| {
                    let mut item = json!({ "name": b.name, "sha": b.target.hash });
                    if Some(&b.name) == default_branch.as_ref() {
                        item["default"] = json!(true);
                    }
                    item
                })
                .collect::<Vec<_>>(),
            "tags": tags
                .iter()
                .map(|t| json!({ "name": t.name, "sha": t.target.hash }))
                .collect::<Vec<_>>(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use crate::handlers::tests::{error, result, set_creds};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn refs_list_branches_and_tags_with_one_default_marker() {
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
            .and(path("/2.0/repositories/team/alpha/refs/branches"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "values": [
                    { "name": "main", "target": { "hash": "abc123" } },
                    { "name": "dev", "target": { "hash": "def456" } }
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/refs/tags"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "values": [ { "name": "v1.0", "target": { "hash": "abc123" } } ]
            })))
            .mount(&server)
            .await;
        let r = result(&server.uri(), "repo/refs", json!({ "repo": "team/alpha" }));
        let branches = r["branches"].as_array().unwrap();
        assert_eq!(branches.len(), 2);
        assert_eq!(branches[0]["name"], "main");
        assert_eq!(branches[0]["sha"], "abc123");
        assert_eq!(branches[0]["default"], true);
        assert_eq!(branches[1]["name"], "dev");
        assert_eq!(
            branches[1].get("default"),
            None,
            "default marks at most one branch — absent elsewhere"
        );
        let tags = r["tags"].as_array().unwrap();
        assert_eq!(tags[0]["name"], "v1.0");
        assert_eq!(tags[0]["sha"], "abc123");
        assert!(tags[0].get("default").is_none(), "tags carry no default");
    }

    #[tokio::test]
    async fn a_repo_without_refs_lists_empty_both_ways() {
        set_creds();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "full_name": "team/alpha"
            })))
            .mount(&server)
            .await;
        for p in [
            "/2.0/repositories/team/alpha/refs/branches",
            "/2.0/repositories/team/alpha/refs/tags",
        ] {
            Mock::given(method("GET"))
                .and(path(p))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "values": []
                })))
                .mount(&server)
                .await;
        }
        let r = result(&server.uri(), "repo/refs", json!({ "repo": "team/alpha" }));
        assert_eq!(r["branches"], json!([]));
        assert_eq!(r["tags"], json!([]));
    }

    #[tokio::test]
    async fn an_unknown_repo_is_not_found() {
        set_creds();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/nope/refs/branches"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let e = error(&server.uri(), "repo/refs", json!({ "repo": "team/nope" }));
        assert_eq!(e["data"]["kind"], "not_found");
    }
}
