//! Integration providers, loaded from a TOML file (`GATEWAY_PROVIDERS`).
//!
//! A *connection* is what a visitor connects once (an Atlassian account, a
//! CRM Plus login). A *provider* is what apps call (`jira`, `confluence`,
//! `crmplus`); several providers can share one connection. Each provider
//! defines scopes, and each scope what it allows.
//!
//! Values of the form `env:NAME` are read from the environment, so client
//! secrets stay out of the file.

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
pub struct IntegrationsConfig {
    #[serde(default)]
    pub connections: BTreeMap<String, ConnectionConfig>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ConnectionConfig {
    Oauth2(Box<OAuth2Config>),
    Crmplus(CrmPlusConfig),
}

impl ConnectionConfig {
    pub fn title(&self) -> &str {
        match self {
            Self::Oauth2(c) => &c.title,
            Self::Crmplus(c) => &c.title,
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TokenAuth {
    /// client_id/client_secret in the form body (most providers).
    #[default]
    Form,
    /// JSON body (Atlassian).
    Json,
    /// HTTP Basic.
    Basic,
}

#[derive(Debug, Deserialize)]
pub struct OAuth2Config {
    pub title: String,
    pub authorize_url: String,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub authorize_params: BTreeMap<String, String>,
    #[serde(default)]
    pub token_auth: TokenAuth,
    /// For APIs addressed per site, e.g. Atlassian's cloud id.
    pub resource_lookup: Option<ResourceLookup>,
    /// Send a PKCE challenge (default). Off for providers that reject it.
    #[serde(default = "yes")]
    pub pkce: bool,
    /// Some providers (Tempo) want `redirect_uri` on refresh too.
    #[serde(default)]
    pub refresh_with_redirect_uri: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct ResourceLookup {
    pub url: String,
    pub match_field: String,
    pub match_value: String,
    pub id_field: String,
}

#[derive(Debug, Deserialize)]
pub struct CrmPlusConfig {
    pub title: String,
    /// e.g. `https://example.brain-app.com`; `webservice.php` is appended.
    pub base_url: String,
}

#[derive(Debug, Deserialize)]
pub struct ProviderConfig {
    pub connection: String,
    pub title: String,
    /// OAuth2 providers: API root; `{resource_id}` is replaced per visitor.
    pub base_url: Option<String>,
    pub scopes: BTreeMap<String, ScopeRule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeRule {
    /// Human wording for the consent page, e.g. "lesen".
    pub label: String,
    /// HTTP methods allowed on any path (OAuth2 providers).
    #[serde(default)]
    pub methods: Vec<String>,
    /// Requests allowed, `"METHOD /path"`; a trailing `*` matches the rest
    /// of the path, e.g. `"GET /4/*"`, `"POST /4/worklogs/search"`.
    #[serde(default)]
    pub requests: Vec<String>,
    /// Webservice operations allowed (CRM Plus).
    #[serde(default)]
    pub operations: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {0}: {1}")]
    Read(String, std::io::Error),
    #[error("parsing providers: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("environment variable {0} is not set")]
    MissingEnv(String),
    #[error("provider {0} refers to unknown connection {1}")]
    UnknownConnection(String, String),
}

impl IntegrationsConfig {
    pub fn load(path: &std::path::Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Read(path.display().to_string(), e))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut config: Self = toml::from_str(text)?;
        for conn in config.connections.values_mut() {
            if let ConnectionConfig::Oauth2(c) = conn {
                c.client_id = resolve_env(&c.client_id)?;
                c.client_secret = resolve_env(&c.client_secret)?;
            }
        }
        for (name, p) in &config.providers {
            if !config.connections.contains_key(&p.connection) {
                return Err(ConfigError::UnknownConnection(
                    name.clone(),
                    p.connection.clone(),
                ));
            }
        }
        Ok(config)
    }
}

fn resolve_env(value: &str) -> Result<String, ConfigError> {
    match value.strip_prefix("env:") {
        Some(name) => std::env::var(name).map_err(|_| ConfigError::MissingEnv(name.into())),
        None => Ok(value.into()),
    }
}

impl ScopeRule {
    /// Whether an OAuth2 call is allowed. `path` has no leading slash and is
    /// already checked by [`safe_path`].
    pub fn allows(&self, method: &str, path: &str) -> bool {
        if self.methods.iter().any(|m| m.eq_ignore_ascii_case(method)) {
            return true;
        }
        self.requests.iter().any(|r| {
            let Some((m, pattern)) = r.split_once(' ') else {
                return false;
            };
            let pattern = pattern.trim_start_matches('/');
            m.eq_ignore_ascii_case(method)
                && match pattern.strip_suffix('*') {
                    Some(prefix) => path.starts_with(prefix),
                    None => path == pattern,
                }
        })
    }
}

/// An API path an app may send upstream: plain segments only, so neither
/// `..` nor a smuggled `?`/`#` can widen what a scope rule allowed.
pub fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && path.split('/').all(|seg| {
            !seg.is_empty()
                && seg != ".."
                && seg != "."
                && seg.bytes().all(|b| {
                    b.is_ascii_alphanumeric()
                        || matches!(
                            b,
                            b'-' | b'_' | b'.' | b'~' | b':' | b'@' | b',' | b'=' | b'+'
                        )
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(requests: &[&str]) -> ScopeRule {
        ScopeRule {
            label: String::new(),
            methods: vec![],
            requests: requests.iter().map(|s| s.to_string()).collect(),
            operations: vec![],
        }
    }

    #[test]
    fn request_rules() {
        let r = rule(&["GET /*", "POST /worklogs/search", "DELETE /worklogs/*"]);
        assert!(r.allows("GET", "accounts"));
        assert!(r.allows("post", "worklogs/search"));
        assert!(!r.allows("POST", "worklogs/search/x"));
        assert!(!r.allows("POST", "worklogs"));
        assert!(r.allows("DELETE", "worklogs/7"));
        assert!(
            !r.allows("DELETE", "worklogsx"),
            "prefix must stop at the slash"
        );
    }

    #[test]
    fn safe_paths() {
        for ok in [
            "worklogs",
            "worklogs/user/557058:abc-1",
            "rest/api/3/issue/ISP-1",
        ] {
            assert!(safe_path(ok), "{ok}");
        }
        for bad in [
            "", "a/../b", "./a", "a?b=1", "a#x", "a b", "a%2e", "a\\b", "a//b",
        ] {
            assert!(!safe_path(bad), "{bad}");
        }
    }
}
