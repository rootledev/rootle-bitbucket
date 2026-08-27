//! URL mapping: web/clone URLs from the repo's `links` object
//! (`repo/web_url`, `repo/clone_url`) and the workspace landing page
//! (`org/url`).

use super::{Handler, WireResult, w};
use crate::api::Repo;
use serde_json::json;

impl Handler {
    pub(super) fn repo_clone_url(&self, full_name: &str) -> WireResult {
        w(
            self.bb.repo(full_name),
            |repo: Repo| json!({ "clone_url": repo.clone_remote() }),
        )
    }

    pub(super) fn repo_web_url(
        &self,
        full_name: &str,
        path: &str,
        branch: &str,
        line: Option<u64>,
        is_file: bool,
    ) -> WireResult {
        w(self.bb.repo(full_name), |repo: Repo| {
            let branch = if branch.is_empty() {
                repo.branch()
            } else {
                branch.to_string()
            };
            let mut url = if path.is_empty() {
                repo.web()
            } else {
                format!("{}/src/{branch}/{path}", repo.web())
            };
            if is_file && let Some(line) = line {
                url.push_str(&format!("#lines-{line}"));
            }
            json!({ "url": url })
        })
    }

    pub(super) fn org_url(&self, org: &str) -> WireResult {
        Ok(json!({ "url": format!("https://bitbucket.org/{org}") }))
    }
}

#[cfg(test)]
mod tests {
    use crate::handlers::tests::{result, set_creds};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn clone_and_web_urls_come_from_links() {
        set_creds();
        let server = MockServer::start().await;
        let repo_json = json!({
            "full_name": "team/alpha",
            "mainbranch": { "name": "main" },
            "links": {
                "html": { "href": "https://bitbucket.org/team/alpha" },
                "clone": [
                    { "name": "https", "href": "https://bitbucket.org/team/alpha.git" },
                    { "name": "ssh", "href": "git@bitbucket.org:team/alpha.git" }
                ]
            }
        });
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team/alpha"))
            .respond_with(ResponseTemplate::new(200).set_body_json(repo_json))
            .mount(&server)
            .await;
        let clone = result(
            &server.uri(),
            "repo/clone_url",
            json!({ "repo": "team/alpha" }),
        );
        assert_eq!(clone["clone_url"], "https://bitbucket.org/team/alpha.git");
        let web = result(
            &server.uri(),
            "repo/web_url",
            json!({ "repo": "team/alpha", "path": "src/main.rs", "branch": "main", "line": 42, "is_file": true }),
        );
        assert_eq!(
            web["url"],
            "https://bitbucket.org/team/alpha/src/main/src/main.rs#lines-42"
        );
    }
}
