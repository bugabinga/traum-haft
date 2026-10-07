//! traum-haft gateway.
//!
//! Sits behind Caddy, which authenticates every request with oauth2-proxy and
//! sets `X-User-Sub`, `X-User-Email` and `X-App` (stripping client copies).
//! The gateway trusts those headers and nothing else from the client.

pub mod config;
pub mod feedback;
pub mod integrations;
pub mod keys;
pub mod store;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use jsonwebtoken::{Algorithm, Header, Validation};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::keys::SigningKey;

/// Platform hosts under the apps domain. Caddy's wildcard site uses
/// on-demand TLS, and its policy also covers these, so `tls-ask` approves them.
pub const PLATFORM_HOSTS: &[&str] = &["connect", "mcp"];

/// Subdomains of the apps domain that can never be app names.
pub const RESERVED_NAMES: &[&str] = &[
    "connect", "mcp", "www", "api", "admin", "platform", "static",
];

pub struct Config {
    /// Public issuer URL, e.g. `https://apps.isp-insoft.de`.
    pub issuer: String,
    /// e.g. `apps.isp-insoft.de`; apps live at `<app>.<apps_domain>`.
    pub apps_domain: String,
    /// Directory with one folder per deployed app.
    pub apps_dir: PathBuf,
    pub token_ttl_secs: u64,
}

pub struct AppState {
    pub config: Config,
    pub key: SigningKey,
    pub integrations: integrations::Integrations,
    pub feedback: feedback::Feedback,
}

/// Claims of a platform-issued app token.
#[derive(Debug, Serialize, Deserialize)]
pub struct AppClaims {
    pub iss: String,
    pub sub: String,
    pub aud: String,
    pub email: String,
    pub iat: u64,
    pub exp: u64,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/.well-known/jwks.json", get(jwks))
        .route("/_auth/token", get(issue_token))
        .route(
            "/_internal/stdb/verify",
            get(verify_stdb_token).post(verify_stdb_token),
        )
        .route("/_internal/tls-ask", get(tls_ask))
        .route(
            "/_api/{provider}/{*path}",
            axum::routing::any(integrations::api),
        )
        // connect.<apps_domain> (Caddy sends only that host's traffic here)
        .route("/c/{provider}", get(integrations::consent_page))
        .route(
            "/c/{provider}/approve",
            axum::routing::post(integrations::approve),
        )
        .route(
            "/c/{provider}/key",
            axum::routing::post(integrations::submit_key),
        )
        .route(
            "/oauth/{connection}/callback",
            get(integrations::oauth_callback),
        )
        .with_state(state)
}

async fn discovery(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let issuer = &state.config.issuer;
    Json(json!({
        "issuer": issuer,
        "jwks_uri": format!("{issuer}/.well-known/jwks.json"),
        "id_token_signing_alg_values_supported": ["ES256"],
        "subject_types_supported": ["public"],
        "response_types_supported": ["id_token"],
    }))
}

async fn jwks(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({ "keys": [state.key.jwk] }))
}

/// Identity set by Caddy. Missing or empty means the request did not come
/// through the authenticated edge, so it is refused.
pub struct Visitor {
    pub sub: String,
    pub email: String,
    pub app: String,
}

pub fn visitor(headers: &HeaderMap) -> Option<Visitor> {
    let get = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(String::from)
    };
    Some(Visitor {
        sub: get("x-user-sub")?,
        email: get("x-user-email")?,
        app: get("x-app")?,
    })
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `GET /_auth/token` on an app origin: a short-lived token for this visitor
/// and this app, used by the frontend as the SpacetimeDB token.
async fn issue_token(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let Some(v) = visitor(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if !is_valid_app_name(&v.app) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let iat = now();
    let claims = AppClaims {
        iss: state.config.issuer.clone(),
        sub: v.sub,
        aud: v.app,
        email: v.email,
        iat,
        exp: iat + state.config.token_ttl_secs,
    };
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(state.key.kid.clone());
    match jsonwebtoken::encode(&header, &claims, &state.key.encoding) {
        Ok(token) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({ "token": token, "expires_at": claims.exp })),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "signing app token failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// `forward_auth` target for the SpacetimeDB token exchange. Passes only a
/// bearer that this gateway signed, for this app (aud) and this visitor (sub).
async fn verify_stdb_token(State(state): State<Arc<AppState>>, headers: HeaderMap) -> StatusCode {
    let Some(v) = visitor(&headers) else {
        return StatusCode::UNAUTHORIZED;
    };
    let Some(token) = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
    else {
        return StatusCode::UNAUTHORIZED;
    };
    let mut validation = Validation::new(Algorithm::ES256);
    validation.set_issuer(&[&state.config.issuer]);
    validation.set_audience(&[&v.app]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.leeway = 30;
    match jsonwebtoken::decode::<AppClaims>(token, &state.key.decoding, &validation) {
        Ok(data) if data.claims.sub == v.sub => StatusCode::OK,
        Ok(_) => StatusCode::UNAUTHORIZED,
        Err(e) => {
            tracing::debug!(error = %e, app = %v.app, "token exchange refused");
            StatusCode::UNAUTHORIZED
        }
    }
}

#[derive(Deserialize)]
struct TlsAsk {
    domain: String,
}

/// Caddy's on-demand TLS `ask`: a certificate only for deployed apps.
async fn tls_ask(State(state): State<Arc<AppState>>, Query(q): Query<TlsAsk>) -> StatusCode {
    let suffix = format!(".{}", state.config.apps_domain);
    let Some(name) = q.domain.strip_suffix(&suffix) else {
        return StatusCode::NOT_FOUND;
    };
    if PLATFORM_HOSTS.contains(&name)
        || (is_valid_app_name(name) && state.config.apps_dir.join(name).is_dir())
    {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

/// One DNS label: lowercase letters, digits, single hyphens inside, at most
/// 40 characters, not reserved and not a preview suffix by itself.
pub fn is_valid_app_name(name: &str) -> bool {
    let len_ok = (1..=40).contains(&name.len());
    let chars_ok = name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    let edges_ok = !name.starts_with('-') && !name.ends_with('-');
    len_ok && chars_ok && edges_ok && !RESERVED_NAMES.contains(&name)
}
