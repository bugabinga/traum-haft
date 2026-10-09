use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use traum_haft_runner::{Config, Runner, router};

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("missing environment variable {name}"))
}
fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.into())
}

#[tokio::main]
async fn main() -> Result<(), String> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();
    let token = env("RUNNER_TOKEN")?;
    let edge_secret = env("RUNNER_EDGE_SECRET")?;
    if token.len() < 32 || edge_secret.len() < 32 {
        return Err("RUNNER_TOKEN and RUNNER_EDGE_SECRET need at least 32 characters".into());
    }
    let cfg = Config {
        token,
        edge_secret,
        data_dir: PathBuf::from(env_or("RUNNER_DATA_DIR", "/srv/werk")),
        domain: env("RUNNER_DOMAIN")?,
        caddy_admin: env_or("RUNNER_CADDY_ADMIN", "http://127.0.0.1:2019"),
        proxy_listen: env("RUNNER_PROXY_LISTEN")?,
        podman: env_or("RUNNER_PODMAN", "podman"),
        age: env_or("RUNNER_AGE", "age"),
        health_timeout: Duration::from_secs(
            env_or("RUNNER_HEALTH_TIMEOUT_SECS", "60")
                .parse()
                .unwrap_or(60),
        ),
        keep: env_or("RUNNER_KEEP", "3").parse().unwrap_or(3),
    };
    let listen: SocketAddr = env("RUNNER_LISTEN")?
        .parse()
        .map_err(|e| format!("RUNNER_LISTEN: {e}"))?;
    let runner = Runner::new(cfg).await?;
    // Containers and routes come back in the background; the API (and
    // SIGTERM handling) must not wait for the worker's Caddy.
    tokio::spawn({
        let runner = runner.clone();
        async move { runner.restore().await }
    });
    tracing::info!(%listen, "runner starting");
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|e| e.to_string())?;
    axum::serve(listener, router(runner))
        .with_graceful_shutdown(traum_haft_common::shutdown::signal())
        .await
        .map_err(|e| e.to_string())
}
