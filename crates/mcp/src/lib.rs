//! traum-haft platform MCP.

pub mod apps;
pub mod builder;
pub mod google;
pub mod oauth;
pub mod rpc;
pub mod stdb;
pub mod tools;
pub mod werk;
pub mod werk_page;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use traum_haft_common::github::GitHubApp;
use traum_haft_common::keys::SigningKey;
use traum_haft_common::mail::Mailer;

pub struct AppState {
    pub apps_domain: String,
    pub data_dir: PathBuf,
    pub key: SigningKey,
    pub oauth: oauth::OAuthServer,
    pub google: google::GoogleLogin,
    pub store: apps::AppStore,
    pub stdb: stdb::Spacetime,
    pub builder: builder::Builder,
    /// e.g. `https://x-access-token:{token}@github.com/{org}/{repo}.git`
    pub git_remote: String,
    pub github_org: String,
    /// How long `deploy` waits before answering "still building".
    pub deploy_wait: std::time::Duration,
    pub jobs: std::sync::Mutex<HashMap<String, DeployJob>>,
    pub github: Option<GitHubApp>,
    pub platform_repo: String,
    pub mailer: Option<Mailer>,
    /// Google accounts of the triage routine.
    pub platform_agents: Vec<String>,
    /// Developer apps; off unless a worker is configured.
    pub werk: Option<werk::Werk>,
    /// Sent by the edge on routes that carry identity headers (logs page).
    pub edge_secret: Option<String>,
    locks: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        apps_domain: String,
        data_dir: PathBuf,
        key: SigningKey,
        oauth: oauth::OAuthServer,
        google: google::GoogleLogin,
        store: apps::AppStore,
        stdb: stdb::Spacetime,
        builder: builder::Builder,
        git_remote: String,
        github_org: String,
        deploy_wait: std::time::Duration,
        github: Option<GitHubApp>,
        platform_repo: String,
        mailer: Option<Mailer>,
        platform_agents: Vec<String>,
    ) -> Self {
        Self {
            apps_domain,
            data_dir,
            key,
            oauth,
            google,
            store,
            stdb,
            builder,
            git_remote,
            github_org,
            deploy_wait,
            jobs: Default::default(),
            github,
            platform_repo,
            mailer,
            platform_agents,
            werk: None,
            edge_secret: None,
            locks: Default::default(),
        }
    }

    /// One deploy or rollback per app at a time.
    pub async fn lock_for(&self, app: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .await
            .entry(app.to_string())
            .or_default()
            .clone()
    }
}

/// The last deploy of an app, for `status` when a build outlives a tool call.
#[derive(Clone, Debug, serde::Serialize)]
pub struct DeployJob {
    pub state: &'static str,
    pub message: String,
    pub started_at: u64,
}

impl AppState {
    /// Push URL for an app repository, with a token scoped to that repo.
    pub async fn remote_url(&self, app: &str) -> Result<Option<String>, String> {
        let Some(gh) = &self.github else {
            return Ok(None);
        };
        let token = gh.repo_token(app).await.map_err(|e| e.to_string())?;
        Ok(Some(
            self.git_remote
                .replace("{token}", &token)
                .replace("{org}", &self.github_org)
                .replace("{repo}", &gh.repo_name(app)),
        ))
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(oauth::protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(oauth::protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(oauth::authorization_server),
        )
        .route("/register", post(oauth::register))
        .route("/authorize", get(oauth::authorize))
        .route("/login/callback", get(oauth::login_callback))
        .route("/token", post(oauth::token))
        .route("/mcp", post(rpc::post).get(rpc::get))
        .route("/werk/logs/{app}", get(werk_page::logs))
        .route("/werk/docs/", get(werk_page::docs))
        .route("/werk/docs/{*path}", get(werk_page::docs))
        .with_state(state)
}
