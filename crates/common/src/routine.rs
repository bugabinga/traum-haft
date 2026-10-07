//! Fires the platform's Claude Code routine that triages visitor reports.
//!
//! The routine's API trigger is a URL plus a bearer token (created on the
//! platform seat; see the admin checklist). Body format: `{"text": ...}`.
//! Unverified against the live API.

use serde_json::json;

pub struct Routine {
    url: String,
    token: String,
    http: reqwest::Client,
}

impl Routine {
    pub fn new(url: &str, token: &str, http: reqwest::Client) -> Self {
        Self {
            url: url.into(),
            token: token.into(),
            http,
        }
    }

    pub async fn fire(&self, text: &str) -> Result<(), String> {
        let res = self
            .http
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(&json!({ "text": text }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if res.status().is_success() {
            Ok(())
        } else {
            Err(format!("routine fire answered {}", res.status()))
        }
    }
}
