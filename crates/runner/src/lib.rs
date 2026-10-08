//! traum-haft runner: the only process on the worker VM that the platform
//! talks to. It receives a developer app's image (and age-encrypted
//! secrets), starts it with podman, checks its health and only then points
//! the worker's Caddy at it. The previous version keeps serving until the
//! new one is healthy.
//!
//! Control API (bearer token, private network only):
//!   GET  /identity                                age recipient for secrets
//!   PUT  /apps/{app}/releases/{v}/image            docker-archive tarball
//!   PUT  /apps/{app}/releases/{v}/secrets          age-encrypted JSON object
//!   POST /apps/{app}/releases/{v}/activate         {port, health, memory}
//!   POST /apps/{app}/stop                          offline, /data kept
//!   GET  /apps/{app}/logs?lines=N                  container output
//!   GET  /apps                                     state

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use traum_haft_common::names::is_valid_app_name;

const MAX_IMAGE: u64 = 4 << 30;
const MAX_SECRETS: u64 = 1 << 20;

pub struct Config {
    pub token: String,
    pub data_dir: PathBuf,
    /// e.g. `werk.isp-insoft.de`; apps answer on `<app>.<domain>`.
    pub domain: String,
    pub caddy_admin: String,
    /// Where the worker's Caddy listens for the platform edge.
    pub proxy_listen: String,
    /// Sent by the platform edge on every request; others get 403.
    pub edge_secret: String,
    pub podman: String,
    pub age: String,
    pub health_timeout: Duration,
    pub keep: usize,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct AppState {
    pub live: Option<u64>,
    pub container: Option<String>,
    pub host_port: Option<u16>,
    pub releases: BTreeMap<u64, Release>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub port: u16,
    pub health: String,
    pub memory: String,
}

pub struct Runner {
    pub cfg: Config,
    pub http: reqwest::Client,
    state: tokio::sync::Mutex<BTreeMap<String, AppState>>,
    locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Runner {
    pub async fn new(cfg: Config) -> Result<Arc<Self>, String> {
        std::fs::create_dir_all(&cfg.data_dir).map_err(|e| e.to_string())?;
        let state = std::fs::read_to_string(cfg.data_dir.join("state.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let runner = Arc::new(Self {
            cfg,
            http: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()
                .map_err(|e| e.to_string())?,
            state: tokio::sync::Mutex::new(state),
            locks: Default::default(),
        });
        runner.identity_recipient().await?;
        Ok(runner)
    }

    fn identity_file(&self) -> PathBuf {
        self.cfg.data_dir.join("identity.txt")
    }

    /// Creates the age identity on first use; returns its public recipient.
    pub async fn identity_recipient(&self) -> Result<String, String> {
        let file = self.identity_file();
        if !file.exists() {
            let out = Command::new(format!("{}-keygen", self.cfg.age))
                .output()
                .await
                .map_err(|e| format!("age-keygen: {e}"))?;
            if !out.status.success() {
                return Err("age-keygen failed".into());
            }
            traum_haft_common::keys::write_private(&file, &String::from_utf8_lossy(&out.stdout))
                .map_err(|e| e.to_string())?;
        }
        let text = std::fs::read_to_string(&file).map_err(|e| e.to_string())?;
        text.lines()
            .find_map(|l| l.strip_prefix("# public key: "))
            .map(|s| s.trim().to_string())
            .ok_or_else(|| "identity file has no public key".into())
    }

    fn lock_for(&self, app: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap()
            .entry(app.into())
            .or_default()
            .clone()
    }

    fn release_dir(&self, app: &str, v: u64) -> PathBuf {
        self.cfg
            .data_dir
            .join("releases")
            .join(app)
            .join(v.to_string())
    }

    async fn save(&self, state: &BTreeMap<String, AppState>) -> Result<(), String> {
        let tmp = self.cfg.data_dir.join("state.json.tmp");
        tokio::fs::write(
            &tmp,
            serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?,
        )
        .await
        .map_err(|e| e.to_string())?;
        tokio::fs::rename(tmp, self.cfg.data_dir.join("state.json"))
            .await
            .map_err(|e| e.to_string())
    }

    async fn podman(&self, args: &[&str]) -> Result<String, String> {
        self.podman_env(args, &[]).await
    }

    async fn podman_env(&self, args: &[&str], env: &[(String, String)]) -> Result<String, String> {
        let mut cmd = Command::new(&self.cfg.podman);
        cmd.args(args).stdin(Stdio::null()).kill_on_drop(true);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = tokio::time::timeout(Duration::from_secs(600), cmd.output())
            .await
            .map_err(|_| format!("podman {} timed out", args[0]))?
            .map_err(|e| format!("podman: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(format!(
                "podman {}: {}",
                args[0],
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    /// The worker Caddy's whole config: one route per live app, nothing else.
    pub async fn push_routes(&self) -> Result<(), String> {
        let state = self.state.lock().await;
        let mut routes: Vec<Value> = state
            .iter()
            .filter_map(|(app, s)| {
                let port = s.host_port?;
                s.live?;
                Some(json!({
                    "match": [{
                        "host": [format!("{app}.{}", self.cfg.domain)],
                        "header": { "X-Traum-Haft-Edge": [self.cfg.edge_secret] }
                    }],
                    "handle": [
                        { "handler": "headers", "request": { "delete": ["X-Traum-Haft-Edge"] } },
                        { "handler": "reverse_proxy", "upstreams": [{ "dial": format!("127.0.0.1:{port}") }] }
                    ],
                    "terminal": true
                }))
            })
            .collect();
        routes.push(json!({ "handle": [{ "handler": "static_response", "status_code": 404 }] }));
        let admin_listen = self
            .cfg
            .caddy_admin
            .trim_start_matches("http://")
            .trim_end_matches('/');
        let config = json!({
            "admin": { "listen": admin_listen, "origins": [admin_listen] },
            "apps": { "http": { "servers": { "werk": {
                "listen": [self.cfg.proxy_listen],
                "routes": routes,
                "automatic_https": { "disable": true }
            }}}}
        });
        drop(state);
        let res = self
            .http
            .post(format!(
                "{}/load",
                self.cfg.caddy_admin.trim_end_matches('/')
            ))
            .json(&config)
            .send()
            .await
            .map_err(|e| format!("caddy admin: {e}"))?;
        if !res.status().is_success() {
            return Err(format!(
                "caddy refused the config: {}",
                res.text().await.unwrap_or_default()
            ));
        }
        Ok(())
    }

    /// After a restart of the worker: bring live containers back, re-route.
    pub async fn restore(&self) {
        let snapshot = self.state.lock().await.clone();
        for (app, s) in snapshot {
            let (Some(container), Some(release)) = (
                s.container.clone(),
                s.live.and_then(|v| s.releases.get(&v).cloned()),
            ) else {
                continue;
            };
            if let Err(e) = self.podman(&["start", &container]).await {
                tracing::warn!(%app, error = %e, "could not restart container");
                continue;
            }
            if let Ok(port) = self.host_port(&container, release.port).await
                && let Some(a) = self.state.lock().await.get_mut(&app)
            {
                a.host_port = Some(port);
            }
        }
        let state = self.state.lock().await.clone();
        let _ = self.save(&state).await;
        for _ in 0..30 {
            match self.push_routes().await {
                Ok(()) => return,
                Err(e) => tracing::warn!(error = %e, "worker caddy not ready"),
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn host_port(&self, container: &str, port: u16) -> Result<u16, String> {
        let out = self
            .podman(&["port", container, &format!("{port}/tcp")])
            .await?;
        out.lines()
            .find_map(|l| l.trim().rsplit_once(':').and_then(|(_, p)| p.parse().ok()))
            .ok_or_else(|| format!("no published port for {container}"))
    }

    async fn image_ref(&self, app: &str, v: u64) -> Result<String, String> {
        let wanted = format!("localhost/werk/{app}:v{v}");
        // The uploaded tarball is the truth; a tag with the same name can be
        // left over from an earlier app of the same name.
        let tar = self.release_dir(app, v).join("image.tar");
        if !tar.exists() {
            return Err(format!("release {v} has no image"));
        }
        let out = self
            .podman(&["load", "-q", "-i", &tar.to_string_lossy()])
            .await?;
        // "Loaded image: <ref>" or "Loaded image(s): <ref>[,<ref>]"
        let loaded = out
            .lines()
            .find_map(|l| l.split_once(": ").map(|(_, r)| r.trim()))
            .and_then(|r| r.split(',').next())
            .filter(|r| !r.is_empty())
            .ok_or("the image tarball holds no image")?
            .to_string();
        self.podman(&["tag", &loaded, &wanted]).await?;
        Ok(wanted)
    }

    async fn secrets(&self, app: &str, v: u64) -> Result<Vec<(String, String)>, String> {
        let file = self.release_dir(app, v).join("secrets.age");
        if !file.exists() {
            return Ok(vec![]);
        }
        let out = Command::new(&self.cfg.age)
            .args(["-d", "-i"])
            .arg(self.identity_file())
            .arg(&file)
            .output()
            .await
            .map_err(|e| format!("age: {e}"))?;
        if !out.status.success() {
            return Err("secrets could not be decrypted (encrypted for another worker?)".into());
        }
        let map: BTreeMap<String, String> = serde_json::from_slice(&out.stdout)
            .map_err(|_| "secrets are not a JSON object of strings".to_string())?;
        Ok(map.into_iter().collect())
    }

    /// Starts release `v`, waits for it to be healthy, then switches traffic
    /// and stops the previous container. On failure the live version stays.
    pub async fn activate(&self, app: &str, v: u64, release: Release) -> Result<String, String> {
        let lock = self.lock_for(app);
        let _guard = lock.lock().await;
        let image = self.image_ref(app, v).await?;
        let secrets = self.secrets(app, v).await?;
        let network = format!("werk-{app}");
        if self.podman(&["network", "exists", &network]).await.is_err() {
            self.podman(&["network", "create", &network]).await?;
        }
        let data = self.cfg.data_dir.join("volumes").join(app);
        tokio::fs::create_dir_all(&data)
            .await
            .map_err(|e| e.to_string())?;
        let name = format!("werk-{app}-v{v}");
        let _ = self.podman(&["rm", "-f", &name]).await;
        let url = format!("https://{app}.{}", self.cfg.domain);
        let port = release.port.to_string();
        let mut args: Vec<String> = [
            "run",
            "-d",
            "--name",
            &name,
            "--label",
            &format!("traum-haft.app={app}"),
            "--label",
            &format!("traum-haft.version={v}"),
            "--network",
            &network,
            "--publish",
            &format!("127.0.0.1::{port}"),
            "--memory",
            &release.memory,
            "--cpus",
            "1",
            "--pids-limit",
            "1024",
            // Signals and zombie reaping for apps that are not built for PID 1.
            "--init",
            "--security-opt",
            "no-new-privileges",
            "--restart",
            "unless-stopped",
            "--volume",
            &format!("{}:/data:Z", data.display()),
            "--env",
            &format!("PORT={port}"),
            "--env",
            &format!("TRAUM_HAFT_APP={app}"),
            "--env",
            &format!("TRAUM_HAFT_URL={url}"),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // Values come from podman's environment, never from its arguments.
        for (k, _) in &secrets {
            args.push("--env".into());
            args.push(k.clone());
        }
        args.push(image);
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        self.podman_env(&argv, &secrets).await?;
        let name_ref = name.as_str();
        let fail = |why: String| async move {
            let logs = self
                .podman(&["logs", "--tail", "40", name_ref])
                .await
                .unwrap_or_default();
            let _ = self.podman(&["rm", "-f", name_ref]).await;
            Err(format!("{why}\n{logs}"))
        };
        let host_port = match self.host_port(&name, release.port).await {
            Ok(p) => p,
            Err(e) => return fail(e).await,
        };
        let health = format!("http://127.0.0.1:{host_port}{}", release.health);
        let deadline = tokio::time::Instant::now() + self.cfg.health_timeout;
        loop {
            if let Ok(r) = self.http.get(&health).send().await
                && r.status().as_u16() < 400
            {
                break;
            }
            let running = self
                .podman(&["inspect", "-f", "{{.State.Running}}", &name])
                .await
                .map(|s| s.trim() == "true")
                .unwrap_or(false);
            if !running {
                return fail(format!(
                    "Start fehlgeschlagen: Container v{v} hat sich beendet."
                ))
                .await;
            }
            if tokio::time::Instant::now() > deadline {
                return fail(format!(
                    "Start fehlgeschlagen: {} antwortet nicht innerhalb von {} s (lauscht die App auf 0.0.0.0:{port}?).",
                    release.health,
                    self.cfg.health_timeout.as_secs()
                ))
                .await;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let (old, before) = {
            let mut state = self.state.lock().await;
            let before = state.get(app).cloned();
            let a = state.entry(app.into()).or_default();
            let old = a.container.replace(name.clone());
            a.live = Some(v);
            a.host_port = Some(host_port);
            a.releases.insert(v, release);
            (old, before)
        };
        if let Err(e) = self.push_routes().await {
            // Traffic never moved: put the state back, drop the new container.
            let mut state = self.state.lock().await;
            match before {
                Some(b) => state.insert(app.into(), b),
                None => state.remove(app),
            };
            drop(state);
            let _ = self.podman(&["rm", "-f", &name]).await;
            let _ = self.push_routes().await;
            return Err(format!("Umschalten fehlgeschlagen: {e}"));
        }
        let snapshot = self.state.lock().await.clone();
        self.save(&snapshot).await?;
        if let Some(old) = old.filter(|o| o != &name) {
            let _ = self.podman(&["stop", "-t", "10", &old]).await;
            let _ = self.podman(&["rm", "-f", &old]).await;
        }
        self.prune(app).await;
        Ok(format!("{app} v{v} live"))
    }

    /// Keeps the newest `keep` releases (and the live one).
    async fn prune(&self, app: &str) {
        let (old, live): (Vec<u64>, Option<u64>) = {
            let state = self.state.lock().await;
            let Some(a) = state.get(app) else { return };
            let versions: Vec<u64> = a.releases.keys().rev().copied().collect();
            (versions.into_iter().skip(self.cfg.keep).collect(), a.live)
        };
        for v in old.into_iter().filter(|v| Some(*v) != live) {
            let _ = self
                .podman(&["rmi", "-f", &format!("localhost/werk/{app}:v{v}")])
                .await;
            let _ = tokio::fs::remove_dir_all(self.release_dir(app, v)).await;
            if let Some(a) = self.state.lock().await.get_mut(app) {
                a.releases.remove(&v);
            }
        }
        let snapshot = self.state.lock().await.clone();
        let _ = self.save(&snapshot).await;
    }

    pub async fn stop(&self, app: &str) -> Result<(), String> {
        let lock = self.lock_for(app);
        let _guard = lock.lock().await;
        let container = {
            let mut state = self.state.lock().await;
            let Some(a) = state.get_mut(app) else {
                return Ok(());
            };
            a.live = None;
            a.host_port = None;
            let c = a.container.take();
            let snapshot = state.clone();
            drop(state);
            self.save(&snapshot).await?;
            c
        };
        self.push_routes().await?;
        if let Some(c) = container {
            let _ = self.podman(&["stop", "-t", "10", &c]).await;
            let _ = self.podman(&["rm", "-f", &c]).await;
        }
        Ok(())
    }

    pub async fn logs(&self, app: &str, lines: u32) -> Result<String, String> {
        let container = self
            .state
            .lock()
            .await
            .get(app)
            .and_then(|a| a.container.clone());
        let Some(c) = container else {
            return Ok(String::new());
        };
        let out = Command::new(&self.cfg.podman)
            .args(["logs", "--timestamps", "--tail", &lines.to_string(), &c])
            .output()
            .await
            .map_err(|e| e.to_string())?;
        // Container stdout and stderr, interleaved by podman per stream.
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        Ok(text)
    }
}

pub fn router(runner: Arc<Runner>) -> Router {
    Router::new()
        .route("/identity", get(identity))
        .route("/apps", get(list))
        .route("/apps/{app}/releases/{v}/image", put(upload_image))
        .route("/apps/{app}/releases/{v}/secrets", put(upload_secrets))
        .route("/apps/{app}/releases/{v}/activate", post(activate))
        .route("/apps/{app}/stop", post(stop))
        .route("/apps/{app}/logs", get(logs))
        .with_state(runner)
}

fn authorized(r: &Runner, headers: &HeaderMap) -> bool {
    let expected = format!("Bearer {}", r.cfg.token);
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| constant_time_eq(v.as_bytes(), expected.as_bytes()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

macro_rules! guard {
    ($r:expr, $headers:expr, $app:expr) => {
        if !authorized(&$r, &$headers) {
            return err(StatusCode::UNAUTHORIZED, "bad token");
        }
        if !is_valid_app_name($app) {
            return err(StatusCode::BAD_REQUEST, "bad app name");
        }
    };
}

async fn identity(State(r): State<Arc<Runner>>, headers: HeaderMap) -> Response {
    if !authorized(&r, &headers) {
        return err(StatusCode::UNAUTHORIZED, "bad token");
    }
    match r.identity_recipient().await {
        Ok(recipient) => Json(json!({ "recipient": recipient })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn list(State(r): State<Arc<Runner>>, headers: HeaderMap) -> Response {
    if !authorized(&r, &headers) {
        return err(StatusCode::UNAUTHORIZED, "bad token");
    }
    Json(r.state.lock().await.clone()).into_response()
}

async fn store_body(path: &Path, body: Body, max: u64) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("part");
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| e.to_string())?;
    let mut stream = body.into_data_stream();
    let mut size = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        size += chunk.len() as u64;
        if size > max {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err("too large".into());
        }
        file.write_all(&chunk).await.map_err(|e| e.to_string())?;
    }
    file.flush().await.map_err(|e| e.to_string())?;
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|e| e.to_string())
}

async fn upload_image(
    State(r): State<Arc<Runner>>,
    UrlPath((app, v)): UrlPath<(String, u64)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    guard!(r, headers, &app);
    match store_body(&r.release_dir(&app, v).join("image.tar"), body, MAX_IMAGE).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn upload_secrets(
    State(r): State<Arc<Runner>>,
    UrlPath((app, v)): UrlPath<(String, u64)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    guard!(r, headers, &app);
    match store_body(
        &r.release_dir(&app, v).join("secrets.age"),
        body,
        MAX_SECRETS,
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

#[derive(Deserialize)]
struct ActivateBody {
    port: u16,
    health: String,
    memory: String,
}

async fn activate(
    State(r): State<Arc<Runner>>,
    UrlPath((app, v)): UrlPath<(String, u64)>,
    headers: HeaderMap,
    Json(b): Json<ActivateBody>,
) -> Response {
    guard!(r, headers, &app);
    // Same rules as traum-haft.toml; the platform checked them, the runner
    // does not rely on that.
    let valid = b.port != 0
        && b.health.starts_with('/')
        && !b
            .health
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        && traum_haft_common::werk::memory_bytes(&b.memory)
            .is_some_and(|m| (64 << 20..=4 << 30).contains(&m));
    if !valid {
        return err(StatusCode::BAD_REQUEST, "bad release settings");
    }
    let release = Release {
        port: b.port,
        health: b.health,
        memory: b.memory,
    };
    match r.activate(&app, v, release).await {
        Ok(msg) => Json(json!({ "ok": true, "message": msg })).into_response(),
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "ok": false, "message": e })),
        )
            .into_response(),
    }
}

async fn stop(
    State(r): State<Arc<Runner>>,
    UrlPath(app): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    guard!(r, headers, &app);
    match r.stop(&app).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize)]
struct LogsQuery {
    lines: Option<u32>,
}

async fn logs(
    State(r): State<Arc<Runner>>,
    UrlPath(app): UrlPath<String>,
    headers: HeaderMap,
    Query(q): Query<LogsQuery>,
) -> Response {
    guard!(r, headers, &app);
    match r.logs(&app, q.lines.unwrap_or(200).min(5000)).await {
        Ok(text) => text.into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}
