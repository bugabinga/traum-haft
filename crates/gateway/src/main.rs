use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use traum_haft_common::github::GitHubApp;
use traum_haft_common::mail::Mailer;
use traum_haft_common::routine::Routine;
use traum_haft_gateway::config::IntegrationsConfig;
use traum_haft_gateway::feedback::Feedback;
use traum_haft_gateway::integrations::Integrations;
use traum_haft_gateway::keys::SigningKey;
use traum_haft_gateway::store::{MemoryStore, SecretStore, VaultStore};
use traum_haft_gateway::{AppState, Config, router};

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("missing environment variable {name}"))
}

#[tokio::main]
async fn main() -> Result<(), String> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let apps_domain = env("GATEWAY_APPS_DOMAIN")?;
    let config = Config {
        issuer: std::env::var("GATEWAY_ISSUER")
            .unwrap_or_else(|_| format!("https://{apps_domain}")),
        apps_domain,
        apps_dir: PathBuf::from(
            std::env::var("GATEWAY_APPS_DIR").unwrap_or_else(|_| "/srv/apps".into()),
        ),
        token_ttl_secs: std::env::var("GATEWAY_TOKEN_TTL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(600),
    };
    let key_file = PathBuf::from(
        std::env::var("GATEWAY_KEY_FILE").unwrap_or_else(|_| "/data/signing-key.pem".into()),
    );
    let key = SigningKey::load_or_create(&key_file).map_err(|e| e.to_string())?;
    let listen: SocketAddr = std::env::var("GATEWAY_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:8080".into())
        .parse()
        .map_err(|e| format!("GATEWAY_LISTEN: {e}"))?;

    let providers = match std::env::var("GATEWAY_PROVIDERS") {
        Ok(path) => {
            IntegrationsConfig::load(std::path::Path::new(&path)).map_err(|e| e.to_string())?
        }
        Err(_) => IntegrationsConfig::default(),
    };
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let store: Arc<dyn SecretStore> = match std::env::var("GATEWAY_VAULT_ADDR") {
        Ok(addr) => Arc::new(VaultStore::new(
            &addr,
            &std::env::var("GATEWAY_VAULT_MOUNT").unwrap_or_else(|_| "kv".into()),
            &std::env::var("GATEWAY_VAULT_PREFIX").unwrap_or_else(|_| "traum-haft".into()),
            &env("GATEWAY_VAULT_ROLE_ID")?,
            &env("GATEWAY_VAULT_SECRET_ID")?,
            http.clone(),
        )),
        // Credentials would vanish on restart: only when asked for explicitly.
        Err(_) if std::env::var("GATEWAY_STORE").as_deref() == Ok("memory") => {
            tracing::warn!("using the in-memory secret store; connections are lost on restart");
            Arc::new(MemoryStore::default())
        }
        Err(_) => {
            return Err("set GATEWAY_VAULT_ADDR (or GATEWAY_STORE=memory for local runs)".into());
        }
    };
    let connect_origin = std::env::var("GATEWAY_CONNECT_ORIGIN")
        .unwrap_or_else(|_| format!("https://connect.{}", config.apps_domain));
    let integrations = Integrations::new(providers, store, http.clone(), connect_origin);

    let github = match std::env::var("GATEWAY_GITHUB_APP_ID") {
        Ok(app_id) => {
            let key = std::fs::read_to_string(env("GATEWAY_GITHUB_KEY_FILE")?)
                .map_err(|e| e.to_string())?;
            Some(
                GitHubApp::new(
                    &std::env::var("GATEWAY_GITHUB_API")
                        .unwrap_or_else(|_| "https://api.github.com".into()),
                    &env("GATEWAY_GITHUB_ORG")?,
                    &std::env::var("GATEWAY_GITHUB_REPO_PREFIX").unwrap_or_else(|_| "app-".into()),
                    &app_id,
                    &key,
                    http.clone(),
                )
                .map_err(|e| e.to_string())?,
            )
        }
        Err(_) => None,
    };
    let routine = match std::env::var("GATEWAY_ROUTINE_URL") {
        Ok(url) => Some(Routine::new(
            &url,
            &env("GATEWAY_ROUTINE_TOKEN")?,
            http.clone(),
        )),
        Err(_) => None,
    };
    let mailer = match std::env::var("GATEWAY_SMTP_URL") {
        Ok(url) => Some(Mailer::new(&url, &env("GATEWAY_MAIL_FROM")?)?),
        Err(_) => None,
    };
    let feedback = Feedback::new(github, routine, mailer);

    tracing::info!(%listen, issuer = %config.issuer, kid = %key.kid, "gateway starting");
    let app = router(Arc::new(AppState {
        config,
        key,
        integrations,
        feedback,
    }));
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|e| e.to_string())?;
    axum::serve(listener, app).await.map_err(|e| e.to_string())
}
