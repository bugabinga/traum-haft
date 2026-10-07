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
        Ok(BuildOutput {
            wasm,
            dist,
            _work: work,
        })
    }
}
