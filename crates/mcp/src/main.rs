use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use traum_haft_common::github::GitHubApp;
use traum_haft_common::keys::SigningKey;
use traum_haft_common::mail::Mailer;
use traum_haft_mcp::{AppState, apps, builder, google, oauth, router, stdb};

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
        // No color codes in journald.
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();
    let apps_domain = env("MCP_APPS_DOMAIN")?;
    let data_dir = PathBuf::from(env_or("MCP_DATA_DIR", "/data"));
    std::fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;

    let key =
        SigningKey::load_or_create(&data_dir.join("signing-key.pem")).map_err(|e| e.to_string())?;
    let origin = env_or("MCP_ORIGIN", &format!("https://mcp.{apps_domain}"));
    let oauth = oauth::OAuthServer::load(&origin, &data_dir);
    let google = google::GoogleLogin::new(
        &env_or("MCP_GOOGLE_ISSUER", "https://accounts.google.com"),
        &env("MCP_GOOGLE_CLIENT_ID")?,
        &env("MCP_GOOGLE_CLIENT_SECRET")?,
        &env("MCP_ALLOWED_DOMAIN")?,
        http.clone(),
    );
    let issuer = env_or("MCP_APP_TOKEN_ISSUER", &format!("https://{apps_domain}"));
    let store = apps::AppStore {
        apps_dir: PathBuf::from(env_or("MCP_APPS_DIR", "/srv/apps")),
        repos_dir: data_dir.join("repos"),
        template_dir: PathBuf::from(env_or("MCP_TEMPLATE_DIR", "/opt/traum-haft/template")),
        module_crate_dep: env_or(
            "MCP_MODULE_CRATE_DEP",
            r#"{ git = "https://github.com/bugabinga/traum-haft" }"#,
        ),
        issuer: issuer.clone(),
    };
    let stdb = stdb::Spacetime::connect(
        &env_or("MCP_SPACETIME_URL", "http://127.0.0.1:3000"),
        &data_dir.join("spacetime-token"),
        http.clone(),
    )
    .await?;
    let builder = match env_or("MCP_BUILDER", "local").as_str() {
        "actions" => builder::Builder::Actions(builder::ActionsBuilder {
            cache_dir: data_dir.join("build-cache"),
            poll: std::time::Duration::from_secs(
                env_or("MCP_BUILD_POLL_SECS", "5").parse().unwrap_or(5),
            ),
            timeout: std::time::Duration::from_secs(20 * 60),
        }),
        "local" => {
            tracing::warn!(
                "MCP_BUILDER=local runs agent-written code on this machine; development only"
            );
            builder::Builder::Local(builder::LocalBuilder {
                issuer,
                spacetime_cli: PathBuf::from(env_or("MCP_SPACETIME_CLI", "spacetime")),
                cache_dir: data_dir.join("build-cache"),
            })
        }
        other => return Err(format!("MCP_BUILDER must be actions or local, not {other}")),
    };
    let github = match std::env::var("MCP_GITHUB_APP_ID") {
        Ok(id) => Some(
            GitHubApp::new(
                &env_or("MCP_GITHUB_API", "https://api.github.com"),
                &env("MCP_GITHUB_ORG")?,
                &env_or("MCP_GITHUB_REPO_PREFIX", "app-"),
                &id,
                &std::fs::read_to_string(env("MCP_GITHUB_KEY_FILE")?).map_err(|e| e.to_string())?,
                http.clone(),
            )
            .map_err(|e| e.to_string())?,
        ),
        Err(_) => None,
    };
    let mailer = match std::env::var("MCP_SMTP_URL") {
        Ok(url) => Some(Mailer::new(&url, &env("MCP_MAIL_FROM")?)?),
        Err(_) => None,
    };
    let agents = env_or("MCP_PLATFORM_AGENTS", "")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let mut state = AppState::new(
        apps_domain,
        data_dir,
        key,
        oauth,
        google,
        store,
        stdb,
        builder,
        env_or(
            "MCP_GIT_REMOTE",
            "https://x-access-token:{token}@github.com/{org}/{repo}.git",
        ),
        env_or("MCP_GITHUB_ORG", ""),
        std::time::Duration::from_secs(
            env_or("MCP_DEPLOY_WAIT_SECS", "200").parse().unwrap_or(200),
        ),
        github,
        env_or("MCP_PLATFORM_REPO", "traum-haft"),
        mailer,
        agents,
    );

    // Developer apps (werk): on when a worker is configured.
    if let Ok(runner_url) = std::env::var("MCP_RUNNER_URL") {
        let deployer = GitHubApp::new(
            &env_or("MCP_GITHUB_API", "https://api.github.com"),
            &env("MCP_GITHUB_ORG")?,
            "",
            &env("MCP_DEPLOY_GITHUB_APP_ID")?,
            &std::fs::read_to_string(env("MCP_DEPLOY_GITHUB_KEY_FILE")?)
                .map_err(|e| e.to_string())?,
            http.clone(),
        )
        .map_err(|e| e.to_string())?;
        let secs = |name: &str, default: u64| {
            std::time::Duration::from_secs(
                env_or(name, &default.to_string())
                    .parse()
                    .unwrap_or(default),
            )
        };
        state.werk = Some(traum_haft_mcp::werk::Werk::new(
            deployer,
            runner_url,
            env("MCP_RUNNER_TOKEN")?,
            env("MCP_WERK_DOMAIN")?,
            env("MCP_ALLOWED_DOMAIN")?,
            secs("MCP_WERK_POLL_SECS", 60),
            secs("MCP_BUILD_POLL_SECS", 5),
            secs("MCP_WERK_BUILD_TIMEOUT_SECS", 30 * 60),
            &state.data_dir,
            http.clone(),
        ));
        state.edge_secret = Some(env("MCP_EDGE_SECRET")?);
    }

    let listen: SocketAddr = env_or("MCP_LISTEN", "0.0.0.0:8080")
        .parse()
        .map_err(|e| format!("MCP_LISTEN: {e}"))?;
    tracing::info!(%listen, %origin, "platform MCP starting");
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|e| e.to_string())?;
    let state = Arc::new(state);
    tokio::spawn(traum_haft_mcp::werk::run(state.clone()));
    axum::serve(listener, router(state))
        .with_graceful_shutdown(traum_haft_common::shutdown::signal())
        .await
        .map_err(|e| e.to_string())
}
