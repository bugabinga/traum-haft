//! Apps on disk: one git repository per app (the draft is its working
//! tree), and immutable releases that Caddy serves.
//!
//! ```text
//! repos/<app>/                 git, branch main; uncommitted changes = draft
//! apps/<app>/meta.json         owner, title, version
//! apps/<app>/releases/<n>/     dist files + app.toml + module.wasm
//! apps/<app>/current -> releases/<n>
//! ```

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::process::Command;

pub const MAX_FILE: usize = 512 * 1024;
pub const MAX_PAGE_CHARS: usize = 90_000;

/// Files as (path, content), plus the cursor of the next page.
pub type SourcePage = (Vec<(String, String)>, Option<usize>);

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Meta {
    pub owner_sub: String,
    pub owner_email: String,
    pub title: String,
    pub created_at: u64,
    pub version: u64,
    #[serde(default)]
    pub deployed_at: u64,
}

pub struct AppStore {
    pub apps_dir: PathBuf,
    pub repos_dir: PathBuf,
    pub template_dir: PathBuf,
    /// TOML value for the `traum-haft-module` dependency in app repos.
    pub module_crate_dep: String,
    pub issuer: String,
}

/// Paths the agent may write: relative, inside the repo, not git or CI.
pub fn checked_path(path: &str) -> Result<PathBuf, String> {
    let p = Path::new(path);
    if path.is_empty() || p.is_absolute() || path.contains('\\') {
        return Err(format!("{path}: use a relative path with /"));
    }
    if !p.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(format!("{path}: no .. or . segments"));
    }
    let first = p
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .unwrap_or_default();
    if matches!(first.as_str(), ".git" | ".github") {
        return Err(format!("{path}: {first}/ is managed by the platform"));
    }
    Ok(p.to_path_buf())
}

async fn git(repo: &Path, args: &[&str], author: Option<(&str, &str)>) -> Result<String, String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args);
    cmd.env("GIT_COMMITTER_NAME", "traum-haft[bot]")
        .env("GIT_COMMITTER_EMAIL", "traum-haft@isp-insoft.de");
    if let Some((name, email)) = author {
        // Google accounts may have no display name; git needs one.
        let name = if name.trim().is_empty() {
            email.split('@').next().unwrap_or(email)
        } else {
            name
        };
        cmd.env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", email);
    }
    let out = cmd.output().await.map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        Err(format!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

impl AppStore {
    pub fn repo(&self, app: &str) -> PathBuf {
        self.repos_dir.join(app)
    }

    pub fn exists(&self, app: &str) -> bool {
        self.repo(app).join(".git").is_dir()
    }

    pub fn meta(&self, app: &str) -> Option<Meta> {
        serde_json::from_str(
            &std::fs::read_to_string(self.apps_dir.join(app).join("meta.json")).ok()?,
        )
        .ok()
    }

    pub fn save_meta(&self, app: &str, meta: &Meta) -> Result<(), String> {
        let dir = self.apps_dir.join(app);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let tmp = dir.join("meta.json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(meta).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        std::fs::rename(tmp, dir.join("meta.json")).map_err(|e| e.to_string())
    }

    pub fn list(&self) -> Vec<(String, Meta)> {
        let mut out: Vec<_> = std::fs::read_dir(&self.apps_dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                self.meta(&name).map(|m| (name, m))
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// New repo from the template, first commit authored by the builder.
    pub async fn create(
        &self,
        app: &str,
        title: &str,
        owner_sub: &str,
        owner_email: &str,
        owner_name: &str,
    ) -> Result<Meta, String> {
        let repo = self.repo(app);
        if repo.exists() {
            return Err(format!("{app} exists already"));
        }
        let result = self.scaffold(app, title, owner_name, owner_email).await;
        if let Err(e) = result {
            // Leave nothing behind, so the name stays usable.
            let _ = std::fs::remove_dir_all(&repo);
            return Err(e);
        }
        let meta = Meta {
            owner_sub: owner_sub.into(),
            owner_email: owner_email.into(),
            title: title.into(),
            created_at: crate::oauth::now(),
            version: 0,
            deployed_at: 0,
        };
        self.save_meta(app, &meta)?;
        Ok(meta)
    }

    async fn scaffold(
        &self,
        app: &str,
        title: &str,
        owner_name: &str,
        owner_email: &str,
    ) -> Result<(), String> {
        let repo = self.repo(app);
        std::fs::create_dir_all(&repo).map_err(|e| e.to_string())?;
        copy_template(&self.template_dir, &repo, &|text: &str| {
            text.replace("{{app}}", app)
                .replace("{{title}}", title)
                .replace("{{issuer}}", &self.issuer)
                .replace(
                    r#"traum-haft-module = { path = "../../crates/traum-haft-module" }"#,
                    &format!("traum-haft-module = {}", self.module_crate_dep),
                )
                .replace(
                    r#"TRAUM_HAFT_ISSUER = "https://apps.isp-insoft.de""#,
                    &format!(r#"TRAUM_HAFT_ISSUER = "{}""#, self.issuer),
                )
        })?;
        git(&repo, &["init", "-q", "-b", "main"], None).await?;
        git(&repo, &["add", "-A"], None).await?;
        git(
            &repo,
            &[
                "commit",
                "-q",
                "-m",
                &format!("Create {app} from the traum-haft template"),
            ],
            Some((owner_name, owner_email)),
        )
        .await?;
        Ok(())
    }

    /// All text files of the draft, sorted, paginated by size.
    pub fn source_page(&self, app: &str, cursor: usize) -> Result<SourcePage, String> {
        let repo = self.repo(app);
        let mut paths = Vec::new();
        walk(&repo, &repo, &mut paths);
        paths.sort();
        let mut files = Vec::new();
        let mut size = 0;
        for (i, rel) in paths.iter().enumerate().skip(cursor) {
            let Ok(content) = std::fs::read_to_string(repo.join(rel)) else {
                continue;
            };
            if size > 0 && size + content.len() > MAX_PAGE_CHARS {
                return Ok((files, Some(i)));
            }
            size += content.len();
            files.push((rel.clone(), content));
        }
        Ok((files, None))
    }

    pub fn write(&self, app: &str, path: &str, content: &str) -> Result<(), String> {
        if content.len() > MAX_FILE {
            return Err(format!("{path}: larger than {} KB", MAX_FILE / 1024));
        }
        let full = self.repo(app).join(checked_path(path)?);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(full, content).map_err(|e| e.to_string())
    }

    pub fn delete(&self, app: &str, path: &str) -> Result<(), String> {
        let full = self.repo(app).join(checked_path(path)?);
        std::fs::remove_file(&full).map_err(|e| format!("{path}: {e}"))
    }

    /// Commits the draft (if changed) as the builder; returns the commit.
    pub async fn commit(
        &self,
        app: &str,
        message: &str,
        author: (&str, &str),
    ) -> Result<String, String> {
        let repo = self.repo(app);
        git(&repo, &["add", "-A"], None).await?;
        let dirty = !git(&repo, &["status", "--porcelain"], None)
            .await?
            .trim()
            .is_empty();
        if dirty {
            git(&repo, &["commit", "-q", "-m", message], Some(author)).await?;
        }
        Ok(git(&repo, &["rev-parse", "--short", "HEAD"], None)
            .await?
            .trim()
            .to_string())
    }

    pub async fn tag(&self, app: &str, version: u64) -> Result<(), String> {
        git(
            &self.repo(app),
            &["tag", "-f", &format!("v{version}")],
            None,
        )
        .await
        .map(|_| ())
    }

    /// Pushes main and tags to the app's GitHub repository. The URL carries
    /// a short-lived token and is never stored in the repo's config.
    pub async fn push(&self, app: &str, url: &str) -> Result<(), String> {
        let repo = self.repo(app);
        git(&repo, &["push", "-q", url, "HEAD:refs/heads/main"], None)
            .await
            .map_err(|e| redact(&e, url))?;
        git(&repo, &["push", "-q", url, "--tags"], None)
            .await
            .map_err(|e| redact(&e, url))?;
        Ok(())
    }

    pub async fn head(&self, app: &str) -> Result<String, String> {
        Ok(git(&self.repo(app), &["rev-parse", "HEAD"], None)
            .await?
            .trim()
            .to_string())
    }

    pub async fn history(&self, app: &str) -> Result<String, String> {
        git(
            &self.repo(app),
            &[
                "log",
                "--max-count=30",
                "--format=%h %ad %an: %s%d",
                "--date=short",
                "--decorate=short",
            ],
            None,
        )
        .await
    }

    pub fn release_dir(&self, app: &str, version: u64) -> PathBuf {
        self.apps_dir
            .join(app)
            .join("releases")
            .join(version.to_string())
    }

    /// Copies a build into `releases/<version>` and switches `current`.
    pub fn install(&self, app: &str, version: u64, dist: &Path, wasm: &[u8]) -> Result<(), String> {
        let dir = self.release_dir(app, version);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
        }
        // The build output carries the app.toml it was built from.
        if !dist.join("app.toml").is_file() {
            return Err("build output has no app.toml".into());
        }
        copy_dir(dist, &dir)?;
        std::fs::write(dir.join("module.wasm"), wasm).map_err(|e| e.to_string())?;
        self.switch(app, version)
    }

    /// Atomically points `current` at an installed release.
    pub fn switch(&self, app: &str, version: u64) -> Result<(), String> {
        let base = self.apps_dir.join(app);
        if !self.release_dir(app, version).is_dir() {
            return Err(format!("version {version} is not installed"));
        }
        let tmp = base.join("current.tmp");
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(format!("releases/{version}"), &tmp)
            .map_err(|e| e.to_string())?;
        std::fs::rename(tmp, base.join("current")).map_err(|e| e.to_string())
    }
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if matches!(
            name.as_str(),
            ".git" | "node_modules" | "target" | "dist" | "module_bindings"
        ) {
            continue;
        }
        if p.is_dir() {
            walk(root, &p, out);
        } else if let Ok(rel) = p.strip_prefix(root) {
            out.push(rel.to_string_lossy().to_string());
        }
    }
}

fn copy_template(from: &Path, to: &Path, fill: &dyn Fn(&str) -> String) -> Result<(), String> {
    for entry in std::fs::read_dir(from)
        .map_err(|e| e.to_string())?
        .flatten()
    {
        let name = entry.file_name().to_string_lossy().to_string();
        if matches!(name.as_str(), "node_modules" | "target" | "dist") {
            continue;
        }
        let (src, dst) = (entry.path(), to.join(&name));
        if src.is_dir() {
            std::fs::create_dir_all(&dst).map_err(|e| e.to_string())?;
            copy_template(&src, &dst, fill)?;
        } else {
            match std::fs::read_to_string(&src) {
                Ok(text) => std::fs::write(&dst, fill(&text)).map_err(|e| e.to_string())?,
                Err(_) => {
                    std::fs::copy(&src, &dst).map_err(|e| e.to_string())?;
                }
            }
        }
    }
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(from)
        .map_err(|e| e.to_string())?
        .flatten()
    {
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if src.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Error text without the token-bearing URL.
/// Hides the push URL and, in case git prints it in another form, its
/// credentials.
fn redact(err: &str, url: &str) -> String {
    let mut out = err.replace(url, "<app repository>");
    if let Some((_, rest)) = url.split_once("://")
        && let Some((userinfo, _)) = rest.split_once('@')
    {
        for secret in [userinfo, userinfo.rsplit(':').next().unwrap_or_default()] {
            if !secret.is_empty() {
                out = out.replace(secret, "***");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn redact_hides_token_in_any_form() {
        let url = "https://x-access-token:ghs_SECRET@github.com/o/app-x.git";
        let err = format!("fatal: {url}\nremote: https://github.com/o/app-x.git?t=ghs_SECRET");
        let out = super::redact(&err, url);
        assert!(!out.contains("ghs_SECRET"), "{out}");
        assert!(out.contains("<app repository>"));
    }
}
