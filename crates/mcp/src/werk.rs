//! Developer apps ("werk"): any repository in the org with a
//! `traum-haft.toml` on main, deployed on every push without anyone asking.
//!
//! Which repositories count is decided by GitHub, not by us: those the
//! deploy App can see minus those the writer App can see. The writer is
//! installed on "only select repositories", i.e. the user-app repos it
//! created, so nothing an agent writes into a user app can make it a
//! developer app. If the writer were ever given all repositories, no
//! repository would count (fail closed).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use traum_haft_common::github::GitHubApp;
use traum_haft_common::werk::{Manifest, app_toml};

use crate::AppState;

const MANIFEST: &str = "traum-haft.toml";
const WORKFLOW: &str = "traum-haft.yml";
const HISTORY: usize = 30;

pub struct Werk {
    /// The read-only "traum-haft-deploy" App, installed on all repositories.
    pub deployer: GitHubApp,
    pub runner_url: String,
    pub runner_token: String,
    /// e.g. `werk.isp-insoft.de`
    pub domain: String,
    pub mail_domain: String,
    pub poll: Duration,
    pub build_poll: Duration,
    pub build_timeout: Duration,
    pub http: reqwest::Client,
    file: PathBuf,
    state: tokio::sync::Mutex<WerkState>,
    busy: std::sync::Mutex<std::collections::HashSet<String>>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct WerkState {
    /// Last push seen per repository (from the installation list).
    #[serde(default)]
    pub seen: BTreeMap<String, String>,
    /// Last commit handled per repository.
    #[serde(default)]
    pub handled: BTreeMap<String, String>,
    #[serde(default)]
    pub apps: BTreeMap<String, DevApp>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DevApp {
    pub repo: String,
    pub manifest: Manifest,
    /// Last release number given out (live or not).
    pub version: u64,
    pub live_version: Option<u64>,
    pub live_sha: Option<String>,
    /// building, live, failed, stopped
    pub state: String,
    pub message: String,
    pub events: Vec<Event>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Event {
    pub at: u64,
    pub sha: String,
    pub text: String,
}

impl Werk {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        deployer: GitHubApp,
        runner_url: String,
        runner_token: String,
        domain: String,
        mail_domain: String,
        poll: Duration,
        build_poll: Duration,
        build_timeout: Duration,
        data_dir: &Path,
        http: reqwest::Client,
    ) -> Self {
        let file = data_dir.join("werk.json");
        let state = std::fs::read_to_string(&file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Self {
            deployer,
            runner_url: runner_url.trim_end_matches('/').into(),
            runner_token,
            domain,
            mail_domain,
            poll,
            build_poll,
            build_timeout,
            http,
            file,
            state: tokio::sync::Mutex::new(state),
            busy: Default::default(),
        }
    }

    pub fn app_url(&self, app: &str) -> String {
        format!("https://{app}.{}/", self.domain)
    }

    pub fn logs_url(&self, app: &str) -> String {
        format!("https://{}/logs/{app}", self.domain)
    }

    pub async fn app(&self, name: &str) -> Option<DevApp> {
        self.state.lock().await.apps.get(name).cloned()
    }

    pub async fn apps(&self) -> BTreeMap<String, DevApp> {
        self.state.lock().await.apps.clone()
    }

    async fn update<R>(&self, f: impl FnOnce(&mut WerkState) -> R) -> R {
        let mut state = self.state.lock().await;
        let r = f(&mut state);
        let tmp = self.file.with_extension("tmp");
        if let Ok(text) = serde_json::to_vec_pretty(&*state)
            && std::fs::write(&tmp, text).is_ok()
        {
            let _ = std::fs::rename(&tmp, &self.file);
        }
        r
    }

    async fn event(&self, app: &str, sha: &str, state: &str, text: &str) {
        let (app, sha, state, text) = (
            app.to_string(),
            sha.to_string(),
            state.to_string(),
            text.to_string(),
        );
        self.update(move |s| {
            if let Some(a) = s.apps.get_mut(&app) {
                a.state = state;
                a.message = text.clone();
                a.events.push(Event {
                    at: crate::oauth::now(),
                    sha,
                    text,
                });
                let n = a.events.len();
                if n > HISTORY {
                    a.events.drain(..n - HISTORY);
                }
            }
        })
        .await;
    }

    async fn runner(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.runner_url))
            .bearer_auth(&self.runner_token)
    }

    pub async fn container_logs(&self, app: &str, lines: u32) -> Result<String, String> {
        let res = self
            .runner(
                reqwest::Method::GET,
                &format!("/apps/{app}/logs?lines={lines}"),
            )
            .await
            .send()
            .await
            .map_err(|e| format!("worker unreachable: {e}"))?;
        res.text().await.map_err(|e| e.to_string())
    }

    async fn recipient(&self) -> Result<String, String> {
        let v: Value = self
            .runner(reqwest::Method::GET, "/identity")
            .await
            .send()
            .await
            .map_err(|e| format!("worker unreachable: {e}"))?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        v["recipient"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| "worker has no identity".into())
    }
}

/// Polls GitHub forever; one repository is handled at a time per repo.
pub async fn run(s: Arc<AppState>) {
    let Some(w) = s.werk.as_ref() else { return };
    loop {
        if let Err(e) = tick(&s).await {
            tracing::warn!(error = %e, "werk poll failed");
        }
        tokio::time::sleep(w.poll).await;
    }
}

/// One pass: find developer repositories that changed and handle them.
pub async fn tick(s: &Arc<AppState>) -> Result<(), String> {
    let w = s.werk.as_ref().ok_or("werk is not configured")?;
    let writer = s
        .github
        .as_ref()
        .ok_or("the writer GitHub App is not configured")?;
    let mine: std::collections::HashSet<String> = writer
        .installed_repos()
        .await
        .map_err(|e| format!("writer installation: {e}"))?
        .into_iter()
        .map(|r| r.name)
        .collect();
    let all = w
        .deployer
        .installed_repos()
        .await
        .map_err(|e| format!("deploy installation: {e}"))?;
    let candidates: Vec<_> = all
        .into_iter()
        .filter(|r| !mine.contains(&r.name))
        .collect();
    // Repositories that went away (deleted, archived, or now a user app).
    let gone: Vec<(String, DevApp)> = {
        let names: std::collections::HashSet<&str> =
            candidates.iter().map(|r| r.name.as_str()).collect();
        w.apps()
            .await
            .into_iter()
            .filter(|(_, a)| a.state != "stopped" && !names.contains(a.repo.as_str()))
            .collect()
    };
    for (name, a) in gone {
        stop(
            s,
            w,
            &name,
            &a.repo,
            "",
            "Repository nicht mehr verfügbar (gelöscht, archiviert oder keine Entwickler-App).",
        )
        .await;
    }
    let seen = w.state.lock().await.seen.clone();
    for repo in candidates {
        if seen.get(&repo.name) == Some(&repo.pushed_at) {
            continue;
        }
        if !w.busy.lock().unwrap().insert(repo.name.clone()) {
            continue;
        }
        let s2 = s.clone();
        tokio::spawn(async move {
            let w = s2.werk.as_ref().unwrap();
            let done = handle(&s2, w, &repo.name).await;
            if done {
                let (n, p) = (repo.name.clone(), repo.pushed_at.clone());
                w.update(move |st| st.seen.insert(n, p)).await;
            }
            w.busy.lock().unwrap().remove(&repo.name);
        });
    }
    Ok(())
}

/// Handles the head of main of one repository. Returns false to retry on
/// the next poll (GitHub unreachable), true when the commit is dealt with.
async fn handle(s: &Arc<AppState>, w: &Werk, repo: &str) -> bool {
    let sha = match w.deployer.branch_head(repo, "main").await {
        Ok(sha) => sha,
        Err(e) => {
            tracing::debug!(%repo, error = %e, "no main branch");
            return true;
        }
    };
    if w.state.lock().await.handled.get(repo) == Some(&sha) {
        return true;
    }
    let text = match w.deployer.file_at(repo, MANIFEST, &sha).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(%repo, error = %e, "reading traum-haft.toml failed");
            return false;
        }
    };
    let previous: Option<(String, DevApp)> =
        w.apps().await.into_iter().find(|(_, a)| a.repo == repo);
    let Some(text) = text else {
        // Not (or no longer) a developer app.
        if let Some((name, a)) = previous
            && a.state != "stopped"
        {
            stop(
                s,
                w,
                &name,
                repo,
                &sha,
                "traum-haft.toml entfernt: App ist offline, /data bleibt erhalten.",
            )
            .await;
        }
        mark(w, repo, &sha).await;
        return true;
    };
    let status = |state: &'static str, msg: String, url: Option<String>| {
        let sha = sha.clone();
        async move {
            if let Err(e) = w
                .deployer
                .set_status(repo, &sha, state, &msg, url.as_deref())
                .await
            {
                tracing::warn!(%repo, error = %e, "commit status failed");
            }
        }
    };
    let manifest = match traum_haft_common::werk::parse(&text, &w.mail_domain) {
        Ok(m) => m,
        Err(e) => {
            status("failure", e, None).await;
            mark(w, repo, &sha).await;
            return true;
        }
    };
    let name = manifest.name.clone();
    // One name, one app: user apps and other repositories keep theirs.
    let taken_by_user_app = s.store.apps_dir.join(&name).exists()
        && !w.state.lock().await.apps.contains_key(&name)
        || s.store.exists(&name);
    let taken_by_other_repo = w.app(&name).await.is_some_and(|a| a.repo != repo);
    if taken_by_user_app || taken_by_other_repo {
        status(
            "failure",
            format!("Name vergeben: {name} gehört schon einer anderen App."),
            None,
        )
        .await;
        mark(w, repo, &sha).await;
        return true;
    }
    match w
        .deployer
        .file_at(repo, &format!(".github/workflows/{WORKFLOW}"), &sha)
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => {
            status(
                "failure",
                format!("Workflow fehlt: .github/workflows/{WORKFLOW} anlegen (siehe Anleitung)."),
                None,
            )
            .await;
            mark(w, repo, &sha).await;
            return true;
        }
        Err(_) => return false,
    }
    // Renamed: the old name goes offline.
    if let Some((old, a)) = &previous
        && old != &name
        && a.state != "stopped"
    {
        stop(s, w, old, repo, &sha, &format!("umbenannt in {name}")).await;
    }
    let version = w
        .update({
            let (name, repo, manifest) = (name.clone(), repo.to_string(), manifest.clone());
            move |st| {
                let a = st.apps.entry(name).or_insert_with(|| DevApp {
                    repo: repo.clone(),
                    manifest: manifest.clone(),
                    version: 0,
                    live_version: None,
                    live_sha: None,
                    state: "building".into(),
                    message: String::new(),
                    events: vec![],
                });
                a.repo = repo;
                a.manifest = manifest;
                a.version += 1;
                a.version
            }
        })
        .await;
    let logs = w.logs_url(&name);
    status(
        "pending",
        format!("Build v{version} läuft"),
        Some(logs.clone()),
    )
    .await;
    w.event(
        &name,
        &sha,
        "building",
        &format!("v{version}: Build gestartet ({})", short(&sha)),
    )
    .await;
    match deploy(s, w, repo, &sha, &manifest, version).await {
        Ok(()) => {
            let url = w.app_url(&name);
            status("success", format!("v{version} live"), Some(url.clone())).await;
            w.event(
                &name,
                &sha,
                "live",
                &format!("v{version} live ({})", short(&sha)),
            )
            .await;
            let (n, sha2) = (name.clone(), sha.clone());
            w.update(move |st| {
                if let Some(a) = st.apps.get_mut(&n) {
                    a.live_version = Some(version);
                    a.live_sha = Some(sha2);
                }
            })
            .await;
            tracing::info!(target: "audit", app = %name, %repo, version, %sha, "developer app deployed");
        }
        Err(e) => {
            status("failure", first_line(&e), Some(logs)).await;
            w.event(&name, &sha, "failed", &format!("v{version}: {e}"))
                .await;
            notify_owners(s, &manifest, &format!("{name}: Deploy v{version} fehlgeschlagen"), &format!(
                "Commit {sha} in {repo} ist nicht online gegangen; die bisherige Version läuft weiter.\n\n{e}\n\nLogs: {}\n",
                w.logs_url(&name)
            ))
            .await;
        }
    }
    mark(w, repo, &sha).await;
    true
}

async fn mark(w: &Werk, repo: &str, sha: &str) {
    let (r, sha) = (repo.to_string(), sha.to_string());
    w.update(move |st| st.handled.insert(r, sha)).await;
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or_default().to_string()
}

async fn deploy(
    s: &Arc<AppState>,
    w: &Werk,
    repo: &str,
    sha: &str,
    m: &Manifest,
    version: u64,
) -> Result<(), String> {
    let recipient = w.recipient().await?;
    let zip = crate::builder::run_workflow(
        &w.deployer,
        repo,
        WORKFLOW,
        &json!({ "sha": sha, "version": version.to_string(), "recipient": recipient }),
        None,
        "image",
        w.build_poll,
        w.build_timeout,
    )
    .await
    .map_err(|e| format!("Build fehlgeschlagen: {e}"))?;
    let dir = tempfile_dir(&s.data_dir.join("werk-tmp"))?;
    let result = async {
        crate::builder::unzip(&zip, &dir)?;
        let image = dir.join("image.tar");
        if !image.is_file() {
            return Err("Build fehlgeschlagen: das Artefakt enthält kein image.tar".to_string());
        }
        let secrets = dir.join("secrets.age");
        if !m.secrets.is_empty() && !secrets.is_file() {
            return Err("Secrets fehlen im Build (secrets: inherit im Workflow?)".to_string());
        }
        let base = format!("/apps/{}/releases/{version}", m.name);
        upload(w, &format!("{base}/image"), &image).await?;
        if secrets.is_file() {
            upload(w, &format!("{base}/secrets"), &secrets).await?;
        }
        let res = w
            .runner(reqwest::Method::POST, &format!("{base}/activate"))
            .await
            .timeout(Duration::from_secs(300))
            .json(&json!({ "port": m.port, "health": m.health, "memory": m.memory }))
            .send()
            .await
            .map_err(|e| format!("worker unreachable: {e}"))?;
        let body: Value = res.json().await.map_err(|e| e.to_string())?;
        if body["ok"] != true {
            return Err(body["message"]
                .as_str()
                .unwrap_or("Start fehlgeschlagen")
                .to_string());
        }
        Ok(())
    }
    .await;
    let _ = std::fs::remove_dir_all(&dir);
    result?;
    // What the gateway and the edge read: the declared integrations, and
    // the marker "developer app" for TLS and routing. The marker sits next
    // to `current`, outside any release, where no user-app build output
    // can reach.
    let rel = s.store.release_dir(&m.name, version);
    std::fs::create_dir_all(&rel).map_err(|e| e.to_string())?;
    std::fs::write(rel.join("app.toml"), app_toml(m)).map_err(|e| e.to_string())?;
    s.store.switch(&m.name, version)?;
    std::fs::write(
        s.store.apps_dir.join(&m.name).join("werk.json"),
        serde_json::to_vec(&json!({ "repo": repo, "sha": sha, "version": version }))
            .unwrap_or_default(),
    )
    .map_err(|e| e.to_string())
}

async fn upload(w: &Werk, path: &str, file: &Path) -> Result<(), String> {
    let f = tokio::fs::File::open(file)
        .await
        .map_err(|e| e.to_string())?;
    let len = f.metadata().await.map_err(|e| e.to_string())?.len();
    let res = w
        .runner(reqwest::Method::PUT, path)
        .await
        .timeout(Duration::from_secs(600))
        .header("content-length", len)
        .body(reqwest::Body::wrap_stream(tokio_util_stream(f)))
        .send()
        .await
        .map_err(|e| format!("worker unreachable: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("worker refused {path}: {}", res.status()));
    }
    Ok(())
}

fn tokio_util_stream(
    f: tokio::fs::File,
) -> impl futures_util::Stream<Item = Result<Vec<u8>, std::io::Error>> {
    futures_util::stream::unfold(f, |mut f| async move {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 1 << 20];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok(buf), f))
            }
            Err(e) => Some((Err(e), f)),
        }
    })
}

fn tempfile_dir(parent: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    let dir = parent.join(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>());
    std::fs::create_dir(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

async fn stop(s: &Arc<AppState>, w: &Werk, name: &str, repo: &str, sha: &str, why: &str) {
    let res = w
        .runner(reqwest::Method::POST, &format!("/apps/{name}/stop"))
        .await
        .send()
        .await;
    if let Err(e) = res {
        tracing::warn!(app = %name, error = %e, "stopping failed; retried on the next change");
        return;
    }
    // Offline for the edge too; the name stays with this repository.
    let _ = std::fs::remove_file(s.store.apps_dir.join(name).join("werk.json"));
    let _ = std::fs::remove_file(s.store.apps_dir.join(name).join("current"));
    w.event(name, sha, "stopped", why).await;
    if !sha.is_empty() {
        let _ = w.deployer.set_status(repo, sha, "success", why, None).await;
    }
    tracing::info!(target: "audit", app = %name, %repo, "developer app stopped");
}

async fn notify_owners(s: &AppState, m: &Manifest, subject: &str, body: &str) {
    let Some(mailer) = &s.mailer else { return };
    for owner in &m.owners {
        if let Err(e) = mailer.send(owner, subject, body).await {
            tracing::warn!(error = %e, "owner mail failed");
        }
    }
}

/// Status of a developer app for its owners and the platform agent.
pub fn status_json(w: &Werk, name: &str, a: &DevApp) -> Value {
    json!({
        "app": name,
        "kind": "developer",
        "repo": a.repo,
        "url": w.app_url(name),
        "logs": w.logs_url(name),
        "state": a.state,
        "message": a.message,
        "live_version": a.live_version,
        "live_commit": a.live_sha,
        "owners": a.manifest.owners,
        "recent": a.events.iter().rev().take(10).map(|e| &e.text).collect::<Vec<_>>(),
    })
}

pub fn may_read(a: &DevApp, email: &str, agent: bool) -> bool {
    agent || a.manifest.owners.iter().any(|o| o == email)
}
