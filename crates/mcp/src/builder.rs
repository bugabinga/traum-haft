//! Builds an app: Rust module to wasm, TypeScript bindings, Vite frontend.
//!
//! Local builds run agent-written code (build.rs, proc macros, npm scripts)
//! on this machine, so they are for development and tests only. Production
//! builds run in GitHub Actions (see .github/workflows/build-app.yml).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

pub struct LocalBuilder {
    pub issuer: String,
    pub spacetime_cli: PathBuf,
    pub cache_dir: PathBuf,
}

pub struct BuildOutput {
    pub wasm: Vec<u8>,
    pub dist: PathBuf,
    /// Keeps the build directory alive until the release is installed.
    pub _work: tempdir::Dir,
}

pub mod tempdir {
    use std::path::{Path, PathBuf};
    /// A directory removed on drop.
    pub struct Dir(PathBuf);
    impl Dir {
        pub fn new(parent: &Path, name: &str) -> std::io::Result<Self> {
            let p = parent.join(format!(
                "{name}-{}",
                std::process::id() as u64 * 1_000_000 + super::nanos()
            ));
            std::fs::create_dir_all(&p)?;
            Ok(Self(p))
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
}

/// Last lines of a failed step, for the agent to read.
fn tail(s: &[u8], lines: usize) -> String {
    let text = String::from_utf8_lossy(s);
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

async fn run(step: &str, cmd: &mut Command, timeout: Duration) -> Result<(), String> {
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd
        .spawn()
        .map_err(|e| format!("{step}: cannot start: {e}"))?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Err(_) => Err(format!("{step}: timed out after {}s", timeout.as_secs())),
        Ok(Err(e)) => Err(format!("{step}: {e}")),
        Ok(Ok(out)) if out.status.success() => Ok(()),
        Ok(Ok(out)) => Err(format!(
            "{step} failed:\n{}\n{}",
            tail(&out.stdout, 20),
            tail(&out.stderr, 60)
        )),
    }
}

impl LocalBuilder {
    /// Builds the committed tree at `repo` (HEAD) as `app`, version `version`.
    pub async fn build(&self, repo: &Path, app: &str, version: u64) -> Result<BuildOutput, String> {
        std::fs::create_dir_all(&self.cache_dir).map_err(|e| e.to_string())?;
        let work = tempdir::Dir::new(&self.cache_dir, &format!("build-{app}"))
            .map_err(|e| e.to_string())?;
        let src = work.path().join("src");
        std::fs::create_dir_all(&src).map_err(|e| e.to_string())?;
        // Exactly the committed files: nothing from a dirty tree leaks in.
        run(
            "checkout",
            Command::new("sh").arg("-c").arg(format!(
                "git -C '{}' archive HEAD | tar -x -C '{}'",
                repo.display(),
                src.display()
            )),
            Duration::from_secs(60),
        )
        .await?;
        let target = self.cache_dir.join(format!("target-{app}"));
        run(
            "module build (cargo)",
            Command::new("cargo")
                .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
                .current_dir(src.join("module"))
                .env("TRAUM_HAFT_ISSUER", &self.issuer)
                .env("TRAUM_HAFT_APP", app)
                .env("CARGO_TARGET_DIR", &target),
            Duration::from_secs(600),
        )
        .await?;
        let wasm_path = std::fs::read_dir(target.join("wasm32-unknown-unknown/release"))
            .map_err(|e| e.to_string())?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().is_some_and(|x| x == "wasm"))
            .ok_or("module build produced no .wasm")?;
        let wasm = std::fs::read(&wasm_path).map_err(|e| e.to_string())?;
        run(
            "bindings (spacetime generate)",
            Command::new(&self.spacetime_cli)
                .args(["--root-dir"])
                .arg(self.cache_dir.join("spacetime-cli"))
                .args([
                    "generate",
                    "--lang",
                    "typescript",
                    "--out-dir",
                    "web/src/module_bindings",
                    "-y",
                    "--bin-path",
                ])
                .arg(&wasm_path)
                .current_dir(&src)
                .env("HOME", self.cache_dir.join("home")),
            Duration::from_secs(120),
        )
        .await?;
        run(
            "web dependencies (npm ci)",
            Command::new("npm")
                .args(["ci", "--no-audit", "--no-fund", "--silent"])
                .current_dir(src.join("web"))
                .env("npm_config_cache", self.cache_dir.join("npm")),
            Duration::from_secs(600),
        )
        .await?;
        run(
            "web build (npm run build)",
            Command::new("npm")
                .args(["run", "build", "--silent"])
                .current_dir(src.join("web"))
                .env("VITE_APP_VERSION", format!("v{version}")),
            Duration::from_secs(300),
        )
        .await?;
        let dist = src.join("web/dist");
        if !dist.join("index.html").exists() {
            return Err("web build produced no dist/index.html".into());
        }
        // Like the workflow's "Collect release": the committed app.toml, not the draft's.
        std::fs::copy(src.join("app.toml"), dist.join("app.toml"))
            .map_err(|e| format!("app.toml: {e}"))?;
        Ok(BuildOutput {
            wasm,
            dist,
            _work: work,
        })
    }
}

/// Production builds: the app repository's `build.yml` (which calls the
/// platform's reusable workflow) runs on GitHub's runners; the MCP waits for
/// it and downloads the `release` artifact.
pub struct ActionsBuilder {
    pub cache_dir: PathBuf,
    pub poll: Duration,
    pub timeout: Duration,
}

impl ActionsBuilder {
    pub async fn build(
        &self,
        gh: &traum_haft_common::github::GitHubApp,
        app: &str,
        target: &str,
        version: u64,
        sha: &str,
    ) -> Result<BuildOutput, String> {
        let zip = run_workflow(
            gh,
            &gh.repo_name(app),
            "build.yml",
            &serde_json::json!({ "version": version.to_string(), "target": target }),
            Some(sha),
            "release",
            self.poll,
            self.timeout,
        )
        .await?;
        std::fs::create_dir_all(&self.cache_dir).map_err(|e| e.to_string())?;
        let work = tempdir::Dir::new(&self.cache_dir, &format!("release-{target}"))
            .map_err(|e| e.to_string())?;
        let dist = work.path().join("release");
        unzip(&zip, &dist)?;
        let wasm = std::fs::read(dist.join("module.wasm"))
            .map_err(|_| "artifact has no module.wasm".to_string())?;
        for f in ["index.html", "app.toml"] {
            if !dist.join(f).exists() {
                return Err(format!("artifact has no {f}"));
            }
        }
        Ok(BuildOutput {
            wasm,
            dist,
            _work: work,
        })
    }
}

/// Starts `workflow` in `repo` (on main), waits for the run it created and
/// returns the zip of its `artifact`. Build errors carry the job log.
#[allow(clippy::too_many_arguments)]
pub async fn run_workflow(
    gh: &traum_haft_common::github::GitHubApp,
    repo: &str,
    workflow: &str,
    inputs: &serde_json::Value,
    sha: Option<&str>,
    artifact: &str,
    poll: Duration,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    let known: Vec<u64> = gh
        .dispatch_runs(repo, sha)
        .await
        .map_err(|e| e.to_string())?
        .iter()
        .filter_map(|r| r["id"].as_u64())
        .collect();
    gh.dispatch_workflow(repo, workflow, "main", inputs)
        .await
        .map_err(|e| format!("could not start the build: {e}"))?;
    let deadline = tokio::time::Instant::now() + timeout;
    // Wait for the run this dispatch created.
    let run_id = loop {
        if tokio::time::Instant::now() > deadline {
            return Err("the build did not start in time".into());
        }
        tokio::time::sleep(poll).await;
        let runs = gh
            .dispatch_runs(repo, sha)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(id) = runs
            .iter()
            .filter_map(|r| r["id"].as_u64())
            .find(|id| !known.contains(id))
        {
            break id;
        }
    };
    let run = loop {
        if tokio::time::Instant::now() > deadline {
            return Err(format!("the build (run {run_id}) did not finish in time"));
        }
        let run = gh.run(repo, run_id).await.map_err(|e| e.to_string())?;
        if run["status"] == "completed" {
            break run;
        }
        tokio::time::sleep(poll).await;
    };
    if run["conclusion"] != "success" {
        let log = gh.failed_job_log(repo, run_id).await.unwrap_or_default();
        return Err(format!(
            "build failed ({}): {}\n{}",
            run["conclusion"].as_str().unwrap_or("unknown"),
            run["html_url"].as_str().unwrap_or_default(),
            tail(log.as_bytes(), 60)
        ));
    }
    gh.artifact_zip(repo, run_id, artifact)
        .await
        .map_err(|e| e.to_string())
}

/// Extracts a zip, refusing entries that would land outside `to`.
pub fn unzip(bytes: &[u8], to: &Path) -> Result<(), String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| format!("artifact is not a zip: {e}"))?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let Some(rel) = entry.enclosed_name() else {
            return Err(format!(
                "artifact entry escapes its folder: {}",
                entry.name()
            ));
        };
        let out = to.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut file = std::fs::File::create(&out).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut file).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Where builds run.
pub enum Builder {
    /// Development and tests only: runs agent code on this machine.
    Local(LocalBuilder),
    Actions(ActionsBuilder),
}
