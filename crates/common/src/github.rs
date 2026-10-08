//! The platform's GitHub App, installed on the organisation that holds one
//! private repository per app (`<prefix><app>`).
//!
//! Every call asks GitHub for an installation token limited to the one
//! repository it touches, so a bug elsewhere cannot reach other apps' repos.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, thiserror::Error)]
pub enum GitHubError {
    #[error("GitHub request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("GitHub answered {status}: {body}")]
    Status { status: u16, body: String },
    #[error("GitHub App key invalid: {0}")]
    Key(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub number: u64,
    pub url: String,
}

pub struct GitHubApp {
    api: String,
    org: String,
    repo_prefix: String,
    app_id: String,
    key: EncodingKey,
    http: reqwest::Client,
    installation: tokio::sync::Mutex<Option<u64>>,
    tokens: tokio::sync::Mutex<HashMap<String, (String, Instant)>>,
}

#[derive(Serialize)]
struct AppJwt<'a> {
    iat: u64,
    exp: u64,
    iss: &'a str,
}

impl GitHubApp {
    /// `api` is `https://api.github.com` outside tests.
    pub fn new(
        api: &str,
        org: &str,
        repo_prefix: &str,
        app_id: &str,
        private_key_pem: &str,
        http: reqwest::Client,
    ) -> Result<Self, GitHubError> {
        let key = EncodingKey::from_rsa_pem(private_key_pem.as_bytes())
            .map_err(|e| GitHubError::Key(e.to_string()))?;
        Ok(Self {
            api: api.trim_end_matches('/').into(),
            org: org.into(),
            repo_prefix: repo_prefix.into(),
            app_id: app_id.into(),
            key,
            http,
            installation: tokio::sync::Mutex::new(None),
            tokens: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    pub fn repo_name(&self, app: &str) -> String {
        format!("{}{}", self.repo_prefix, app)
    }

    fn app_jwt(&self) -> Result<String, GitHubError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // GitHub allows at most 10 minutes; backdate for clock drift.
        let claims = AppJwt {
            iat: now - 60,
            exp: now + 540,
            iss: &self.app_id,
        };
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.key)
            .map_err(|e| GitHubError::Key(e.to_string()))
    }

    fn req(&self, method: reqwest::Method, path: &str, bearer: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.api, path))
            .bearer_auth(bearer)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "traum-haft")
    }

    async fn send(req: reqwest::RequestBuilder) -> Result<Value, GitHubError> {
        let res = req.send().await?;
        let status = res.status().as_u16();
        let text = res.text().await?;
        if !(200..300).contains(&status) {
            return Err(GitHubError::Status {
                status,
                body: text.chars().take(500).collect(),
            });
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn installation_id(&self) -> Result<u64, GitHubError> {
        let mut cached = self.installation.lock().await;
        if let Some(id) = *cached {
            return Ok(id);
        }
        let body = Self::send(self.req(
            reqwest::Method::GET,
            &format!("/orgs/{}/installation", self.org),
            &self.app_jwt()?,
        ))
        .await?;
        let id = body["id"].as_u64().ok_or_else(|| GitHubError::Status {
            status: 200,
            body: "no installation id".into(),
        })?;
        *cached = Some(id);
        Ok(id)
    }

    /// Installation token for one repository (or for the org, to create repos).
    async fn token(&self, repo: Option<&str>) -> Result<String, GitHubError> {
        let cache_key = repo.unwrap_or("*").to_string();
        let mut tokens = self.tokens.lock().await;
        if let Some((t, until)) = tokens.get(&cache_key)
            && Instant::now() < *until
        {
            return Ok(t.clone());
        }
        let id = self.installation_id().await?;
        let body = match repo {
            Some(r) => json!({ "repositories": [r] }),
            None => json!({}),
        };
        let res = Self::send(
            self.req(
                reqwest::Method::POST,
                &format!("/app/installations/{id}/access_tokens"),
                &self.app_jwt()?,
            )
            .json(&body),
        )
        .await?;
        let token = res["token"].as_str().unwrap_or_default().to_string();
        // Tokens last an hour; renew after 50 minutes.
        tokens.insert(
            cache_key,
            (token.clone(), Instant::now() + Duration::from_secs(50 * 60)),
        );
        Ok(token)
    }

    pub async fn create_issue(
        &self,
        app: &str,
        title: &str,
        body: &str,
        labels: &[&str],
    ) -> Result<Issue, GitHubError> {
        let repo = self.repo_name(app);
        let token = self.token(Some(&repo)).await?;
        let res = Self::send(
            self.req(
                reqwest::Method::POST,
                &format!("/repos/{}/{repo}/issues", self.org),
                &token,
            )
            .json(&json!({ "title": title, "body": body, "labels": labels })),
        )
        .await?;
        Ok(Issue {
            number: res["number"].as_u64().unwrap_or(0),
            url: res["html_url"].as_str().unwrap_or_default().into(),
        })
    }

    /// Issue in any repository of the org (e.g. the platform repo).
    pub async fn create_issue_in(
        &self,
        repo: &str,
        title: &str,
        body: &str,
        labels: &[&str],
    ) -> Result<Issue, GitHubError> {
        let token = self.token(Some(repo)).await?;
        let res = Self::send(
            self.req(
                reqwest::Method::POST,
                &format!("/repos/{}/{repo}/issues", self.org),
                &token,
            )
            .json(&json!({ "title": title, "body": body, "labels": labels })),
        )
        .await?;
        Ok(Issue {
            number: res["number"].as_u64().unwrap_or(0),
            url: res["html_url"].as_str().unwrap_or_default().into(),
        })
    }

    /// Open issues of an app (pull requests excluded).
    pub async fn list_issues(&self, app: &str) -> Result<Vec<Value>, GitHubError> {
        let repo = self.repo_name(app);
        let token = self.token(Some(&repo)).await?;
        let res = Self::send(self.req(
            reqwest::Method::GET,
            &format!("/repos/{}/{repo}/issues?state=open&per_page=50", self.org),
            &token,
        ))
        .await?;
        Ok(res
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|i| i.get("pull_request").is_none())
            .collect())
    }

    pub async fn get_issue(&self, app: &str, number: u64) -> Result<Value, GitHubError> {
        let repo = self.repo_name(app);
        let token = self.token(Some(&repo)).await?;
        Self::send(self.req(
            reqwest::Method::GET,
            &format!("/repos/{}/{repo}/issues/{number}", self.org),
            &token,
        ))
        .await
    }

    pub async fn comment_and_close(
        &self,
        app: &str,
        number: u64,
        comment: &str,
    ) -> Result<(), GitHubError> {
        let repo = self.repo_name(app);
        let token = self.token(Some(&repo)).await?;
        Self::send(
            self.req(
                reqwest::Method::POST,
                &format!("/repos/{}/{repo}/issues/{number}/comments", self.org),
                &token,
            )
            .json(&json!({ "body": comment })),
        )
        .await?;
        Self::send(
            self.req(
                reqwest::Method::PATCH,
                &format!("/repos/{}/{repo}/issues/{number}", self.org),
                &token,
            )
            .json(&json!({ "state": "closed", "state_reason": "completed" })),
        )
        .await?;
        Ok(())
    }

    /// Installation token limited to one app repository, e.g. for git push.
    pub async fn repo_token(&self, app: &str) -> Result<String, GitHubError> {
        self.token(Some(&self.repo_name(app))).await
    }

    /// Creates the private repository of an app.
    pub async fn create_repo(&self, app: &str, description: &str) -> Result<(), GitHubError> {
        let token = self.token(None).await?;
        Self::send(
            self.req(
                reqwest::Method::POST,
                &format!("/orgs/{}/repos", self.org),
                &token,
            )
            .json(&json!({
                "name": self.repo_name(app),
                "private": true,
                "description": description,
                "has_wiki": false,
                "has_projects": false,
                "auto_init": false,
            })),
        )
        .await?;
        Ok(())
    }

    pub async fn dispatch_workflow(
        &self,
        repo: &str,
        workflow: &str,
        git_ref: &str,
        inputs: &Value,
    ) -> Result<(), GitHubError> {
        let token = self.token(Some(repo)).await?;
        Self::send(
            self.req(
                reqwest::Method::POST,
                &format!(
                    "/repos/{}/{repo}/actions/workflows/{workflow}/dispatches",
                    self.org
                ),
                &token,
            )
            .json(&json!({ "ref": git_ref, "inputs": inputs })),
        )
        .await?;
        Ok(())
    }

    /// Recent `workflow_dispatch` runs of a repository, newest first;
    /// optionally only those of one commit.
    pub async fn dispatch_runs(
        &self,
        repo: &str,
        sha: Option<&str>,
    ) -> Result<Vec<Value>, GitHubError> {
        let token = self.token(Some(repo)).await?;
        let res = Self::send(self.req(
            reqwest::Method::GET,
            &format!(
                "/repos/{}/{repo}/actions/runs?event=workflow_dispatch&per_page=20{}",
                self.org,
                sha.map(|s| format!("&head_sha={s}")).unwrap_or_default()
            ),
            &token,
        ))
        .await?;
        Ok(res["workflow_runs"].as_array().cloned().unwrap_or_default())
    }

    pub async fn run(&self, repo: &str, run_id: u64) -> Result<Value, GitHubError> {
        let token = self.token(Some(repo)).await?;
        Self::send(self.req(
            reqwest::Method::GET,
            &format!("/repos/{}/{repo}/actions/runs/{run_id}", self.org),
            &token,
        ))
        .await
    }

    /// The zip of a named artifact of a run.
    pub async fn artifact_zip(
        &self,
        repo: &str,
        run_id: u64,
        name: &str,
    ) -> Result<Vec<u8>, GitHubError> {
        let token = self.token(Some(repo)).await?;
        let list = Self::send(self.req(
            reqwest::Method::GET,
            &format!("/repos/{}/{repo}/actions/runs/{run_id}/artifacts", self.org),
            &token,
        ))
        .await?;
        let url = list["artifacts"]
            .as_array()
            .and_then(|a| a.iter().find(|x| x["name"] == name))
            .and_then(|x| x["archive_download_url"].as_str())
            .ok_or_else(|| GitHubError::Status {
                status: 404,
                body: format!("artifact {name} not found"),
            })?
            .to_string();
        // GitHub redirects to blob storage; reqwest drops the Authorization
        // header on cross-origin redirects.
        let res = self
            .http
            .get(&url)
            .bearer_auth(&token)
            .header("User-Agent", "traum-haft")
            .send()
            .await?;
        if !res.status().is_success() {
            return Err(GitHubError::Status {
                status: res.status().as_u16(),
                body: "artifact download failed".into(),
            });
        }
        Ok(res.bytes().await?.to_vec())
    }

    /// Log text of the first failed job of a run (for build errors).
    pub async fn failed_job_log(&self, repo: &str, run_id: u64) -> Result<String, GitHubError> {
        let token = self.token(Some(repo)).await?;
        let jobs = Self::send(self.req(
            reqwest::Method::GET,
            &format!("/repos/{}/{repo}/actions/runs/{run_id}/jobs", self.org),
            &token,
        ))
        .await?;
        let Some(job) = jobs["jobs"]
            .as_array()
            .and_then(|j| j.iter().find(|x| x["conclusion"] == "failure"))
        else {
            return Ok(String::new());
        };
        let id = job["id"].as_u64().unwrap_or(0);
        let res = self
            .req(
                reqwest::Method::GET,
                &format!("/repos/{}/{repo}/actions/jobs/{id}/logs", self.org),
                &token,
            )
            .send()
            .await?;
        Ok(res.text().await.unwrap_or_default())
    }

    /// Names of all repositories the App is installed on (not archived).
    pub async fn installed_repos(&self) -> Result<Vec<String>, GitHubError> {
        let token = self.token(None).await?;
        let mut out = Vec::new();
        for page in 1..=20 {
            let res = Self::send(self.req(
                reqwest::Method::GET,
                &format!("/installation/repositories?per_page=100&page={page}"),
                &token,
            ))
            .await?;
            let repos = res["repositories"].as_array().cloned().unwrap_or_default();
            out.extend(
                repos
                    .iter()
                    .filter(|r| r["archived"] != true)
                    .filter_map(|r| r["name"].as_str().map(String::from)),
            );
            if repos.len() < 100 {
                break;
            }
        }
        Ok(out)
    }

    /// Commit sha a branch points to.
    pub async fn branch_head(&self, repo: &str, branch: &str) -> Result<String, GitHubError> {
        let token = self.token(Some(repo)).await?;
        let res = Self::send(self.req(
            reqwest::Method::GET,
            &format!("/repos/{}/{repo}/commits/{branch}", self.org),
            &token,
        ))
        .await?;
        res["sha"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| GitHubError::Status {
                status: 200,
                body: "no sha".into(),
            })
    }

    /// A file's text at a commit, or `None` if it does not exist.
    pub async fn file_at(
        &self,
        repo: &str,
        path: &str,
        git_ref: &str,
    ) -> Result<Option<String>, GitHubError> {
        let token = self.token(Some(repo)).await?;
        let res = self
            .http
            .get(format!(
                "{}/repos/{}/{repo}/contents/{path}?ref={git_ref}",
                self.api, self.org
            ))
            .bearer_auth(&token)
            .header("Accept", "application/vnd.github.raw+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "traum-haft")
            .send()
            .await?;
        match res.status().as_u16() {
            404 => Ok(None),
            200 => Ok(Some(res.text().await?)),
            status => Err(GitHubError::Status {
                status,
                body: res
                    .text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(300)
                    .collect(),
            }),
        }
    }
}
