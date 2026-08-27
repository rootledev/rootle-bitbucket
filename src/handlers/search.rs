//! Repo discovery: the org search across workspaces and the
//! workspace repo listing (`search/repos`, `org/repos`).

use super::{Handler, WireResult, w};
use crate::api::{ApiResult, Repo};
use serde_json::json;

impl Handler {
    fn search(&self, query: &str) -> ApiResult<Vec<(String, Vec<Repo>)>> {
        let q = query.to_lowercase();
        // Configured workspaces are served directly (a token scoped to
        // repositories only can't discover — CHANGE-2770); otherwise
        // /user/workspaces, which wants the account read scope.
        let slugs: Vec<String> = if !self.workspaces.is_empty() {
            self.workspaces
                .iter()
                .filter(|s| s.to_lowercase().contains(&q))
                .cloned()
                .collect()
        } else {
            self.bb
                .workspaces()?
                .into_iter()
                .filter(|ws| {
                    ws.slug.to_lowercase().contains(&q)
                        || ws
                            .name
                            .as_deref()
                            .unwrap_or_default()
                            .to_lowercase()
                            .contains(&q)
                })
                .map(|ws| ws.slug)
                .collect()
        };
        let mut out = Vec::new();
        for slug in slugs {
            let repos = self.bb.workspace_repos(&slug)?;
            out.push((slug, repos));
            if out.len() >= 5 {
                break;
            }
        }
        Ok(out)
    }

    pub(super) fn search_repos(&self, query: &str) -> WireResult {
        w(self.search(query), |groups| {
            let mut items = Vec::new();
            for (ws, repos) in &groups {
                items.push(json!({ "org": ws }));
                for repo in repos.iter().take(10) {
                    items.push(json!({ "full_name": repo.full_name }));
                }
                if items.len() >= 20 {
                    break;
                }
            }
            if items.is_empty() {
                // Honest fallback: the query as a workspace guess.
                items.push(json!({ "org": query }));
            }
            json!({ "items": items })
        })
    }

    pub(super) fn org_repos(&self, org: &str) -> WireResult {
        w(
            self.bb.workspace_repos(org),
            |repos| json!({ "repos": repos.iter().map(|r| r.name().to_string()).collect::<Vec<_>>() }),
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::handlers::tests::{result, set_creds};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn org_repos_strips_the_workspace_prefix() {
        set_creds();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/2.0/repositories/team"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "values": [
                    { "full_name": "team/alpha", "mainbranch": { "name": "main" } },
                    { "full_name": "team/beta" }
                ]
            })))
            .mount(&server)
            .await;
        let r = result(&server.uri(), "org/repos", json!({ "org": "team" }));
        assert_eq!(r["repos"], json!(["alpha", "beta"]));
    }
}
