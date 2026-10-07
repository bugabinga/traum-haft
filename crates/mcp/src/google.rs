//! Signs builders in with Google (OpenID Connect) for the MCP's own OAuth
//! server. Only Workspace accounts of the company domain (`hd`) pass.

use std::collections::HashMap;

use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::Value;

pub struct GoogleLogin {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub domain: String,
    http: reqwest::Client,
    discovery: tokio::sync::OnceCell<Discovery>,
}

#[derive(Deserialize, Clone)]
pub struct Discovery {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
}

#[derive(Debug, Clone)]
pub struct Builder {
    pub sub: String,
    pub email: String,
    pub name: String,
}

impl GoogleLogin {
    pub fn new(
        issuer: &str,
        client_id: &str,
        client_secret: &str,
        domain: &str,
        http: reqwest::Client,
    ) -> Self {
        Self {
            issuer: issuer.trim_end_matches('/').into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            domain: domain.into(),
            http,
            discovery: tokio::sync::OnceCell::new(),
        }
    }

    pub async fn discovery(&self) -> Result<&Discovery, String> {
        self.discovery
            .get_or_try_init(|| async {
                let url = format!("{}/.well-known/openid-configuration", self.issuer);
                self.http
                    .get(&url)
                    .send()
                    .await
                    .map_err(|e| e.to_string())?
                    .json::<Discovery>()
                    .await
                    .map_err(|e| e.to_string())
            })
            .await
    }

    /// Exchanges the code and checks the ID token: signature, issuer,
    /// audience, expiry, nonce, verified email and Workspace domain.
    pub async fn finish(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
        nonce: &str,
    ) -> Result<Builder, String> {
        let d = self.discovery().await?;
        let tokens: Value = self
            .http
            .post(&d.token_endpoint)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("client_id", &self.client_id),
                ("client_secret", &self.client_secret),
                ("code_verifier", verifier),
            ])
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        let id_token = tokens["id_token"]
            .as_str()
            .ok_or("no id_token from Google")?;
        let header = jsonwebtoken::decode_header(id_token).map_err(|e| e.to_string())?;
        let jwks: Value = self
            .http
            .get(&d.jwks_uri)
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        let jwk = jwks["keys"]
            .as_array()
            .and_then(|keys| {
                keys.iter()
                    .find(|k| k["kid"].as_str() == header.kid.as_deref())
            })
            .ok_or("signing key not found")?;
        let key = DecodingKey::from_rsa_components(
            jwk["n"].as_str().unwrap_or_default(),
            jwk["e"].as_str().unwrap_or_default(),
        )
        .map_err(|e| e.to_string())?;
        let mut v = Validation::new(Algorithm::RS256);
        v.set_audience(&[&self.client_id]);
        // Google uses both spellings of its issuer.
        let bare = self.issuer.trim_start_matches("https://").to_string();
        v.set_issuer(&[self.issuer.as_str(), bare.as_str()]);
        let claims: HashMap<String, Value> = jsonwebtoken::decode(id_token, &key, &v)
            .map_err(|e| e.to_string())?
            .claims;
        if claims.get("nonce").and_then(|n| n.as_str()) != Some(nonce) {
            return Err("nonce mismatch".into());
        }
        if claims.get("email_verified").and_then(|n| n.as_bool()) != Some(true) {
            return Err("email not verified".into());
        }
        if claims.get("hd").and_then(|n| n.as_str()) != Some(self.domain.as_str()) {
            return Err(format!("only {} Workspace accounts", self.domain));
        }
        let get = |k: &str| {
            claims
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        Ok(Builder {
            sub: get("sub"),
            email: get("email"),
            name: get("name"),
        })
    }
}
