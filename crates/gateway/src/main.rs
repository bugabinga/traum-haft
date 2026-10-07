use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use traum_haft_gateway::keys::SigningKey;
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

    tracing::info!(%listen, issuer = %config.issuer, kid = %key.kid, "gateway starting");
    let app = router(Arc::new(AppState { config, key }));
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|e| e.to_string())?;
    axum::serve(listener, app).await.map_err(|e| e.to_string())
}
