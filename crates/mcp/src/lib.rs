//! traum-haft platform MCP.

pub mod apps;
pub mod builder;
pub mod google;
pub mod oauth;
pub mod rpc;
pub mod stdb;
pub mod tools;

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
    pub builder: builder::LocalBuilder,
    pub github: Option<GitHubApp>,
    pub platform_repo: String,
    pub mailer: Option<Mailer>,
    /// Google accounts of the triage routine.
    pub platform_agents: Vec<String>,
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
        builder: builder::LocalBuilder,
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
            github,
            platform_repo,
            mailer,
            platform_agents,
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
        .with_state(state)
}
