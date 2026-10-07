//! Where per-visitor credentials and consents live.
//!
//! Production: Vault (or OpenBao) KV v2 through AppRole. Tests and local
//! runs: in memory. Keys look like `users/<sub>/connections/<name>`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("secret store unavailable: {0}")]
    Unavailable(String),
}

#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<Value>, StoreError>;
    async fn put(&self, key: &str, value: &Value) -> Result<(), StoreError>;
    async fn delete(&self, key: &str) -> Result<(), StoreError>;
}

#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<String, Value>>);

#[async_trait]
impl SecretStore for MemoryStore {
    async fn get(&self, key: &str) -> Result<Option<Value>, StoreError> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    async fn put(&self, key: &str, value: &Value) -> Result<(), StoreError> {
        self.0.lock().unwrap().insert(key.into(), value.clone());
        Ok(())
    }
    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

/// Vault / OpenBao KV v2, authenticated with AppRole.
pub struct VaultStore {
    addr: String,
    mount: String,
    prefix: String,
    role_id: String,
    secret_id: String,
    http: reqwest::Client,
    token: tokio::sync::Mutex<Option<(String, Instant)>>,
}

impl VaultStore {
    pub fn new(
        addr: &str,
        mount: &str,
        prefix: &str,
        role_id: &str,
        secret_id: &str,
        http: reqwest::Client,
    ) -> Self {
        Self {
            addr: addr.trim_end_matches('/').into(),
            mount: mount.into(),
            prefix: prefix.trim_matches('/').into(),
            role_id: role_id.into(),
            secret_id: secret_id.into(),
            http,
            token: tokio::sync::Mutex::new(None),
        }
    }

    async fn token(&self) -> Result<String, StoreError> {
        let mut guard = self.token.lock().await;
        if let Some((t, until)) = guard.as_ref()
            && Instant::now() < *until {
                return Ok(t.clone());
            }
        let res = self
            .http
            .post(format!("{}/v1/auth/approle/login", self.addr))
            .json(&json!({ "role_id": self.role_id, "secret_id": self.secret_id }))
            .send()
            .await
            .map_err(|e| StoreError::Unavailable(e.to_string()))?;
        if !res.status().is_success() {
            return Err(StoreError::Unavailable(format!(
                "approle login: {}",
                res.status()
            )));
        }
        let body: Value = res
            .json()
            .await
            .map_err(|e| StoreError::Unavailable(e.to_string()))?;
        let token = body["auth"]["client_token"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let lease = body["auth"]["lease_duration"].as_u64().unwrap_or(300);
        // Renew well before the lease ends.
        *guard = Some((
            token.clone(),
            Instant::now() + Duration::from_secs(lease.saturating_sub(30).max(10)),
        ));
        Ok(token)
    }

    fn url(&self, key: &str) -> String {
        format!(
            "{}/v1/{}/data/{}/{}",
            self.addr, self.mount, self.prefix, key
        )
    }
}

#[async_trait]
impl SecretStore for VaultStore {
    async fn get(&self, key: &str) -> Result<Option<Value>, StoreError> {
        let res = self
            .http
            .get(self.url(key))
            .header("X-Vault-Token", self.token().await?)
            .send()
            .await
            .map_err(|e| StoreError::Unavailable(e.to_string()))?;
        match res.status().as_u16() {
            404 => Ok(None),
            s if (200..300).contains(&s) => {
                let body: Value = res
                    .json()
                    .await
                    .map_err(|e| StoreError::Unavailable(e.to_string()))?;
                Ok(Some(body["data"]["data"].clone()).filter(|v| !v.is_null()))
            }
            s => Err(StoreError::Unavailable(format!("vault read: {s}"))),
        }
    }

    async fn put(&self, key: &str, value: &Value) -> Result<(), StoreError> {
        let res = self
            .http
            .post(self.url(key))
            .header("X-Vault-Token", self.token().await?)
            .json(&json!({ "data": value }))
            .send()
            .await
            .map_err(|e| StoreError::Unavailable(e.to_string()))?;
        if res.status().is_success() {
            Ok(())
        } else {
            Err(StoreError::Unavailable(format!(
                "vault write: {}",
                res.status()
            )))
        }
    }

    async fn delete(&self, key: &str) -> Result<(), StoreError> {
        let res = self
            .http
            .delete(self.url(key))
            .header("X-Vault-Token", self.token().await?)
            .send()
            .await
            .map_err(|e| StoreError::Unavailable(e.to_string()))?;
        if res.status().is_success() || res.status().as_u16() == 404 {
            Ok(())
        } else {
            Err(StoreError::Unavailable(format!(
                "vault delete: {}",
                res.status()
            )))
        }
    }
}
