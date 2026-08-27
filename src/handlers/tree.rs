//! The tree: branch → commit resolution, the recursive walk, and
//! branch revalidation on a cached repo that 404s (`repo/tree`).

use super::{Handler, WireResult};
use crate::api::{ApiError, Repo};
use crate::cache::{RepoMeta, Tree};
use serde_json::json;

impl Handler {
    /// Branch → commit → tree. The head is resolved on every call: a
    /// branch-keyed cache entry would serve the tree as of first
    /// fetch forever, and content ids must move when the backend's
    /// content moves (a pushed commit is a new id set — rootle's
    /// cache is content-keyed and never invalidates). Only the
    /// commit-keyed tree cache is safe to serve blind, because a
    /// commit-pinned tree is immutable; the ref round trip is the
    /// price of freshness.
    pub(super) fn tree_at_commit(&self, full_name: &str) -> crate::api::ApiResult<(String, Tree)> {
        // Two statements, deliberately: the read guard is a temporary
        // that must drop before revalidate takes the write lock
        // (parking_lot deadlocks otherwise).
        let cached = self.cache.read().repo_meta(full_name);
        let meta = cached.or_else(|| self.revalidate_repo(full_name).ok());
        let Some(meta) = meta else {
            return Err(ApiError::Api {
                status: 404,
                message: format!("no such repo {full_name:?}"),
                retry_after: None,
            });
        };
        let commit = self.bb.branch_head(full_name, &meta.branch)?;
        let key = commit_key(&commit);
        if let Some(tree) = self.cache.read().tree(full_name, &key) {
            return Ok((meta.branch, tree));
        }
        let (entries, truncated) = self.bb.walk_tree(full_name, &commit)?;
        let tree = Tree {
            entries,
            truncated,
            branch: meta.branch.clone(),
        };
        self.cache.write().store_tree(full_name, &key, &tree);
        Ok((meta.branch, tree))
    }

    /// Default-branch head commit (repo meta, lazily revalidated) —
    /// what revision methods pin to when no ref was sent.
    pub(super) fn head_commit(&self, full_name: &str) -> crate::api::ApiResult<String> {
        // Two statements, deliberately: the read guard is a temporary
        // that must drop before revalidate takes the write lock
        // (parking_lot deadlocks otherwise).
        let cached = self.cache.read().repo_meta(full_name);
        let meta = cached.or_else(|| self.revalidate_repo(full_name).ok());
        let Some(meta) = meta else {
            return Err(ApiError::Api {
                status: 404,
                message: format!("no such repo {full_name:?}"),
                retry_after: None,
            });
        };
        self.bb.branch_head(full_name, &meta.branch)
    }

    /// Ref → commit, shared by every revision method (tree, log,
    /// blob_at). No ref means the default branch's head.
    pub(super) fn resolve_commit(
        &self,
        full_name: &str,
        ref_: Option<&str>,
    ) -> crate::api::ApiResult<String> {
        match ref_ {
            None => self.head_commit(full_name),
            Some(r) => self.bb.resolve_ref(full_name, r),
        }
    }

    /// Tree pinned to an explicit ref (v1.5): branch, tag, or sha,
    /// resolved fresh every call (a branch head moves; tags and shas
    /// are immutable), then the same commit-keyed cache + walk. The
    /// reply names the ref that was served.
    pub(super) fn tree_at_ref(
        &self,
        full_name: &str,
        ref_: &str,
    ) -> crate::api::ApiResult<(String, Tree)> {
        let commit = self.bb.resolve_ref(full_name, ref_)?;
        let key = commit_key(&commit);
        if let Some(tree) = self.cache.read().tree(full_name, &key) {
            return Ok((ref_.to_string(), tree));
        }
        let (entries, truncated) = self.bb.walk_tree(full_name, &commit)?;
        let tree = Tree {
            entries,
            truncated,
            branch: ref_.to_string(),
        };
        self.cache.write().store_tree(full_name, &key, &tree);
        Ok((ref_.to_string(), tree))
    }

    pub(super) fn revalidate_repo(&self, full_name: &str) -> crate::api::ApiResult<RepoMeta> {
        let repo: Repo = self.bb.repo(full_name)?;
        let meta = RepoMeta {
            full_name: repo.full_name.clone(),
            branch: repo.branch(),
        };
        self.cache.write().store_repo_meta(&meta);
        Ok(meta)
    }

    pub(super) fn repo_tree(&self, full_name: &str, ref_: Option<&str>) -> WireResult {
        let (branch, tree) = match ref_ {
            // A pinned revision skips the revalidation dance: the ref
            // resolves fresh every call, and an unknown ref or repo
            // lands as the plain not_found the protocol asks for.
            Some(r) => self.tree_at_ref(full_name, r)?,
            None => {
                // A 404 on a cached repo means it moved — revalidate once.
                match self.tree_at_commit(full_name) {
                    Ok(v) => v,
                    Err(ApiError::Api { status: 404, .. }) => {
                        self.cache.write().drop_repo_meta(full_name);
                        let repo = self.bb.repo(full_name)?;
                        let meta = RepoMeta {
                            full_name: repo.full_name.clone(),
                            branch: repo.branch(),
                        };
                        self.cache.write().store_repo_meta(&meta);
                        self.tree_at_commit(full_name)?
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        };
        Ok(json!({
            "entries": tree
                .entries
                .iter()
                .map(|e| json!({
                    "path": e.path,
                    "type": if e.is_dir { "tree" } else { "blob" },
                    "sha": e.sha,
                    "size": e.size,
                }))
                .collect::<Vec<_>>(),
            "truncated": tree.truncated,
            "branch": branch,
        }))
    }
}
fn commit_key(commit: &str) -> String {
    commit.to_string()
}

#[cfg(test)]
mod tests {
    use crate::handlers::tests::{error, result, results, set_creds};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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
    async fn content_ids_move_when_the_head_moves() {
        // A branch-keyed cache would serve the first-fetched tree
        // forever; content ids must track the backend's head (a
        // pushed commit is a new id set — rootle's cache is
        // content-keyed and never invalidates).
        set_creds();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "full_name": "team/alpha", "mainbranch": { "name": "main" }
            })))
            .mount(&server)
            .await;
        // Head gen 1 answers once, then gen 2 takes over.
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/refs/branches/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "target": { "hash": "gen1" }
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/refs/branches/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "target": { "hash": "gen2" }
            })))
            .mount(&server)
            .await;
        for commit in ["gen1", "gen2"] {
            Mock::given(method("GET"))
                .and(path(format!("/2.0/repositories/team/alpha/src/{commit}/")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "values": [ { "type": "commit_file", "path": "lib.rs", "size": 3 } ]
                })))
                .mount(&server)
                .await;
        }
        let out = results(
            &server.uri(),
            "repo/tree",
            &[
                json!({ "repo": "team/alpha" }),
                json!({ "repo": "team/alpha" }),
            ],
        );
        assert_eq!(out[0]["result"]["entries"][0]["sha"], "gen1:lib.rs");
        assert_eq!(
            out[1]["result"]["entries"][0]["sha"], "gen2:lib.rs",
            "a moved head must move the content ids, not replay the cached tree"
        );
    }

    #[tokio::test]
    async fn tree_at_a_branch_ref_walks_that_branches_head() {
        set_creds();
        let server = MockServer::start().await;
        // The default branch is NOT requested: an explicit ref pins
        // the walk without the repo-meta round trip.
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/refs/branches/feature-x"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "target": { "hash": "feed42" }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/src/feed42/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "values": [ { "type": "commit_file", "path": "lib.rs", "size": 3 } ]
            })))
            .mount(&server)
            .await;
        let r = result(
            &server.uri(),
            "repo/tree",
            json!({ "repo": "team/alpha", "ref": "feature-x" }),
        );
        // The reply names what was actually served (v1.5).
        assert_eq!(r["branch"], "feature-x");
        assert_eq!(r["entries"][0]["sha"], "feed42:lib.rs");
    }

    #[tokio::test]
    async fn tree_at_a_tag_ref_resolves_to_the_tagged_commit() {
        set_creds();
        let server = MockServer::start().await;
        // Not a branch: the probe 404s and the tag listing answers.
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/refs/branches/v1.0"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/refs/tags/v1.0"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "target": { "hash": "tagged9" }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha/src/tagged9/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "values": [ { "type": "commit_file", "path": "lib.rs", "size": 3 } ]
            })))
            .mount(&server)
            .await;
        let r = result(
            &server.uri(),
            "repo/tree",
            json!({ "repo": "team/alpha", "ref": "v1.0" }),
        );
        assert_eq!(r["branch"], "v1.0");
        assert_eq!(r["entries"][0]["sha"], "tagged9:lib.rs");
    }

    #[tokio::test]
    async fn tree_at_a_commit_sha_skips_the_probes() {
        set_creds();
        let server = MockServer::start().await;
        let sha = "0123456789abcdef0123456789abcdef01234567";
        // Only the pinned walk is requested — a well-formed sha is
        // taken at face value (no branch/tag probes; the walk 404s
        // if the sha doesn't exist).
        Mock::given(method("GET"))
            .and(path(format!("/2.0/repositories/team/alpha/src/{sha}/")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "values": [ { "type": "commit_file", "path": "lib.rs", "size": 3 } ]
            })))
            .mount(&server)
            .await;
        let r = result(
            &server.uri(),
            "repo/tree",
            json!({ "repo": "team/alpha", "ref": sha }),
        );
        assert_eq!(r["branch"], sha);
        assert_eq!(r["entries"][0]["sha"], format!("{sha}:lib.rs"));
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
            "repo/tree",
            json!({ "repo": "team/alpha", "ref": "nope" }),
        );
        assert_eq!(e["data"]["kind"], "not_found");
        let msg = e["message"].as_str().unwrap();
        assert!(msg.contains("nope"), "message names the ref: {msg}");
    }
}
