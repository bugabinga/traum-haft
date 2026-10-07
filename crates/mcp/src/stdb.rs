//! The MCP's admin client for SpacetimeDB. The MCP has its own identity and
//! owns every app database; nobody else can publish (the edge blocks it).

use std::path::Path;

use serde_json::Value;

pub struct Spacetime {
    url: String,
    http: reqwest::Client,
    token: String,
}

pub enum Plan {
    /// Database does not exist yet.
    New,
    /// Automatic migration; `token` confirms client-breaking changes.
    Auto {
        plan: String,
        break_clients: bool,
        token: String,
    },
    /// Needs the data deleted.
    Manual { reason: String },
}

impl Spacetime {
    /// Loads the identity token from `token_file`, or creates an identity.
    pub async fn connect(
        url: &str,
        token_file: &Path,
        http: reqwest::Client,
    ) -> Result<Self, String> {
        let url = url.trim_end_matches('/').to_string();
        let token = match tokio::fs::read_to_string(token_file).await {
            Ok(t) => t.trim().to_string(),
            Err(_) => {
                let res: Value = http
                    .post(format!("{url}/v1/identity"))
                    .send()
                    .await
                    .map_err(|e| e.to_string())?
                    .json()
                    .await
                    .map_err(|e| e.to_string())?;
                let t = res["token"]
                    .as_str()
                    .ok_or("SpacetimeDB issued no token")?
                    .to_string();
                tokio::fs::write(token_file, &t)
                    .await
                    .map_err(|e| e.to_string())?;
                t
            }
        };
        Ok(Self { url, http, token })
    }

    pub async fn plan(&self, db: &str, wasm: &[u8]) -> Result<Plan, String> {
        let res = self
            .http
            .post(format!("{}/v1/database/{db}/pre_publish", self.url))
            .bearer_auth(&self.token)
            .query(&[("style", "NoColor")])
            .body(wasm.to_vec())
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if res.status().as_u16() == 404 {
            return Ok(Plan::New);
        }
        let status = res.status();
        let text = res.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!(
                "migration check failed ({status}): {}",
                text.chars().take(500).collect::<String>()
            ));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        if let Some(a) = v.get("AutoMigrate") {
            return Ok(Plan::Auto {
                plan: a["migrate_plan"].as_str().unwrap_or_default().into(),
                break_clients: a["break_clients"].as_bool().unwrap_or(false),
                token: a["token"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| a["token"].to_string()),
            });
        }
        if let Some(m) = v.get("ManualMigrate") {
            return Ok(Plan::Manual {
                reason: m["reason"]
                    .as_str()
                    .unwrap_or("schema change needs data deletion")
                    .into(),
            });
        }
        Err(format!("unexpected migration answer: {text}"))
    }

    /// `clear` deletes all data; `break_token` confirms client-breaking changes.
    pub async fn publish(
        &self,
        db: &str,
        wasm: &[u8],
        clear: bool,
        break_token: Option<&str>,
    ) -> Result<(), String> {
        let mut req = self
            .http
            .put(format!("{}/v1/database/{db}", self.url))
            .bearer_auth(&self.token)
            .header("content-type", "application/wasm");
        if clear {
            req = req.query(&[("clear", "true")]);
        }
        if let Some(t) = break_token {
            req = req.query(&[("policy", "BreakClients"), ("token", t)]);
        }
        let res = req
            .body(wasm.to_vec())
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if res.status().is_success() {
            Ok(())
        } else {
            Err(format!(
                "publish failed ({}): {}",
                res.status(),
                res.text()
                    .await
                    .unwrap_or_default()
                    .chars()
                    .take(800)
                    .collect::<String>()
            ))
        }
    }

    pub async fn logs(&self, db: &str, lines: u32) -> Result<String, String> {
        let res = self
            .http
            .get(format!("{}/v1/database/{db}/logs", self.url))
            .bearer_auth(&self.token)
            .query(&[("num_lines", lines.to_string())])
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if res.status().is_success() {
            res.text().await.map_err(|e| e.to_string())
        } else {
            Err(format!("logs: {}", res.status()))
        }
    }
}
