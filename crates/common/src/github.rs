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
        if let Some((t, until)) = tokens.get(&cache_key) {
            if Instant::now() < *until {
                return Ok(t.clone());
            }
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
}
