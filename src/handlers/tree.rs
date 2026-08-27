//! The tree: branch → commit resolution, the recursive walk, and
//! branch revalidation on a cached repo that 404s (`repo/tree`).

use super::{Handler, WireResult};
use crate::api::{ApiError, Repo};
use crate::cache::{RepoMeta, Tree};
use serde_json::json;

impl Handler {
    /// Branch → commit, then the walk (cache-first: a commit-pinned
    /// tree is immutable).
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
        if let Some(tree) = self.cache.read().tree(full_name, &branch_key(&meta)) {
            return Ok((meta.branch, tree));
        }
        let commit = self.bb.branch_head(full_name, &meta.branch)?;
        let key = commit_key(&commit);
        if let Some(tree) = self.cache.read().tree(full_name, &key) {
            // Remember the head mapping so the next cold start skips
            // the ref round trip.
            self.cache
                .write()
                .store_tree(full_name, &branch_key(&meta), &tree);
            return Ok((meta.branch, tree));
        }
        let (entries, truncated) = self.bb.walk_tree(full_name, &commit)?;
        let tree = Tree {
            entries,
            truncated,
            branch: meta.branch.clone(),
        };
        self.cache.write().store_tree(full_name, &key, &tree);
        self.cache
            .write()
            .store_tree(full_name, &branch_key(&meta), &tree);
        Ok((meta.branch, tree))
    }

    fn revalidate_repo(&self, full_name: &str) -> crate::api::ApiResult<RepoMeta> {
        let repo: Repo = self.bb.repo(full_name)?;
        let meta = RepoMeta {
            full_name: repo.full_name.clone(),
            branch: repo.branch(),
        };
        self.cache.write().store_repo_meta(&meta);
        Ok(meta)
    }

    pub(super) fn repo_tree(&self, full_name: &str) -> WireResult {
        // A 404 on a cached repo means it moved — revalidate once.
        let result = self.tree_at_commit(full_name);
        let (branch, tree) = match result {
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

/// Cache key for the branch→head mapping (trees fetched via a branch
/// name are the head at fetch time; the commit key holds the truth).
fn branch_key(meta: &RepoMeta) -> String {
    format!("branch-{}", meta.branch)
}

fn commit_key(commit: &str) -> String {
    commit.to_string()
}

#[cfg(test)]
mod tests {
    use crate::handlers::tests::{result, set_creds};
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
}
