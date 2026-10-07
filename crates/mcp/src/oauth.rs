//! OAuth 2.1 authorization server for MCP clients (Claude Cowork, Claude
//! Code): RFC 8414 + RFC 9728 metadata, dynamic client registration
//! (RFC 7591), authorization code with PKCE S256 only, rotating refresh
//! tokens with reuse detection. Builders sign in through Google.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::{Algorithm, Header, Validation};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::AppState;
use crate::google::Builder;

pub const ACCESS_TTL: u64 = 3600;
const REFRESH_TTL: u64 = 60 * 60 * 24 * 90;

/// Redirect URIs Claude uses; loopback for Claude Code.
fn redirect_allowed(uri: &str) -> bool {
    const EXACT: &[&str] = &[
        "https://claude.ai/api/mcp/auth_callback",
        "https://claude.com/api/mcp/auth_callback",
    ];
    if EXACT.contains(&uri) {
        return true;
    }
    match url::Url::parse(uri) {
        Ok(u) => {
            u.scheme() == "http"
                && matches!(u.host_str(), Some("localhost") | Some("127.0.0.1"))
                && u.fragment().is_none()
        }
        Err(_) => false,
    }
}

fn random() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("random");
    URL_SAFE_NO_PAD.encode(b)
}
fn hash(s: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(s.as_bytes()))
}
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Serialize, Deserialize, Clone)]
struct Client {
    redirect_uris: Vec<String>,
    name: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct RefreshRecord {
    family: String,
    client_id: String,
    sub: String,
    email: String,
    name: String,
    expires: u64,
    used: bool,
}

/// What survives restarts (refresh tokens stored as hashes only).
#[derive(Serialize, Deserialize, Default)]
struct Persisted {
    clients: HashMap<String, Client>,
    refresh: HashMap<String, RefreshRecord>,
}

struct Pending {
    client_id: String,
    redirect_uri: String,
    client_state: Option<String>,
    challenge: String,
    google_verifier: String,
    nonce: String,
    expires: Instant,
}

struct Code {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    builder: Builder,
    expires: Instant,
}

pub struct OAuthServer {
    pub origin: String,
    file: PathBuf,
    data: tokio::sync::Mutex<Persisted>,
    pending: std::sync::Mutex<HashMap<String, Pending>>,
    codes: std::sync::Mutex<HashMap<String, Code>>,
}

/// Claims of an MCP access token.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AccessClaims {
    pub iss: String,
    pub aud: String,
    pub sub: String,
    pub email: String,
    pub name: String,
    pub client_id: String,
    pub iat: u64,
    pub exp: u64,
}

impl OAuthServer {
    pub fn load(origin: &str, data_dir: &std::path::Path) -> Self {
        let file = data_dir.join("oauth.json");
        let data = std::fs::read_to_string(&file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Self {
            origin: origin.trim_end_matches('/').into(),
            file,
            data: tokio::sync::Mutex::new(data),
            pending: Default::default(),
            codes: Default::default(),
        }
    }

    async fn save(&self, data: &Persisted) {
        let tmp = self.file.with_extension("tmp");
        if let Ok(text) = serde_json::to_string(data) {
            let ok = tokio::fs::write(&tmp, text).await.is_ok()
                && tokio::fs::rename(&tmp, &self.file).await.is_ok();
            if !ok {
                tracing::error!("could not persist OAuth state");
            }
        }
    }

    pub fn resource(&self) -> String {
        format!("{}/mcp", self.origin)
    }

    pub fn verify_access(&self, state: &AppState, token: &str) -> Option<AccessClaims> {
        let mut v = Validation::new(Algorithm::ES256);
        v.set_issuer(&[&self.origin]);
        v.set_audience(&[self.resource()]);
        jsonwebtoken::decode::<AccessClaims>(token, &state.key.decoding, &v)
            .ok()
            .map(|d| d.claims)
    }
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    (
        status,
        Json(json!({ "error": error, "error_description": description })),
    )
        .into_response()
}

pub async fn protected_resource(State(s): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "resource": s.oauth.resource(),
        "authorization_servers": [s.oauth.origin],
        "bearer_methods_supported": ["header"],
    }))
}

pub async fn authorization_server(State(s): State<Arc<AppState>>) -> Json<Value> {
    let o = &s.oauth.origin;
    Json(json!({
        "issuer": o,
        "authorization_endpoint": format!("{o}/authorize"),
        "token_endpoint": format!("{o}/token"),
        "registration_endpoint": format!("{o}/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": ["traum-haft"],
    }))
}

/// RFC 7591 dynamic client registration (public clients only).
pub async fn register(State(s): State<Arc<AppState>>, Json(req): Json<Value>) -> Response {
    let uris: Vec<String> = req["redirect_uris"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|u| u.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if uris.is_empty() || !uris.iter().all(|u| redirect_allowed(u)) {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "redirect URI not allowed",
        );
    }
    let name = req["client_name"]
        .as_str()
        .unwrap_or("MCP client")
        .chars()
        .take(100)
        .collect::<String>();
    let id = random();
    let mut data = s.oauth.data.lock().await;
    data.clients.insert(
        id.clone(),
        Client {
            redirect_uris: uris.clone(),
            name: name.clone(),
        },
    );
    s.oauth.save(&data).await;
    tracing::info!(client = %name, "client registered");
    (
        StatusCode::CREATED,
        Json(json!({
            "client_id": id,
            "client_id_issued_at": now(),
            "client_name": name,
            "redirect_uris": uris,
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct AuthorizeQuery {
    response_type: Option<String>,
    client_id: String,
    redirect_uri: String,
    state: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    resource: Option<String>,
}

pub async fn authorize(
    State(s): State<Arc<AppState>>,
    Query(q): Query<AuthorizeQuery>,
) -> Response {
    let client = s.oauth.data.lock().await.clients.get(&q.client_id).cloned();
    let Some(client) = client else {
        return oauth_error(StatusCode::BAD_REQUEST, "invalid_client", "unknown client");
    };
    if !client.redirect_uris.contains(&q.redirect_uri) {
        // Never redirect to an unregistered URI.
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "redirect_uri not registered",
        );
    }
    let fail = |err: &str| {
        let mut u = url::Url::parse(&q.redirect_uri).expect("registered URI");
        u.query_pairs_mut().append_pair("error", err);
        if let Some(st) = &q.state {
            u.query_pairs_mut().append_pair("state", st);
        }
        Redirect::to(u.as_str()).into_response()
    };
    if q.response_type.as_deref() != Some("code") {
        return fail("unsupported_response_type");
    }
    let (Some(challenge), Some("S256")) =
        (q.code_challenge.clone(), q.code_challenge_method.as_deref())
    else {
        return fail("invalid_request");
    };
    if q.resource
        .as_deref()
        .is_some_and(|r| r != s.oauth.resource() && r != s.oauth.origin)
    {
        return fail("invalid_target");
    }
    let Ok(d) = s.google.discovery().await else {
        return fail("temporarily_unavailable");
    };
    let our_state = random();
    let verifier = random();
    let nonce = random();
    s.oauth.pending.lock().unwrap().insert(
        our_state.clone(),
        Pending {
            client_id: q.client_id,
            redirect_uri: q.redirect_uri,
            client_state: q.state,
            challenge,
            google_verifier: verifier.clone(),
            nonce: nonce.clone(),
            expires: Instant::now() + Duration::from_secs(600),
        },
    );
    let mut u = url::Url::parse(&d.authorization_endpoint).expect("google authorize URL");
    u.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &s.google.client_id)
        .append_pair(
            "redirect_uri",
            &format!("{}/login/callback", s.oauth.origin),
        )
        .append_pair("scope", "openid email profile")
        .append_pair("state", &our_state)
        .append_pair("nonce", &nonce)
        .append_pair(
            "code_challenge",
            &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        )
        .append_pair("code_challenge_method", "S256")
        .append_pair("hd", &s.google.domain)
        .append_pair("prompt", "select_account");
    Redirect::to(u.as_str()).into_response()
}

#[derive(Deserialize)]
pub struct LoginCallback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

pub async fn login_callback(
    State(s): State<Arc<AppState>>,
    Query(q): Query<LoginCallback>,
) -> Response {
    let pending = q
        .state
        .as_deref()
        .and_then(|st| s.oauth.pending.lock().unwrap().remove(st));
    let Some(p) = pending.filter(|p| p.expires > Instant::now()) else {
        return (
            StatusCode::BAD_REQUEST,
            Html("Anmeldung abgelaufen. Bitte in Claude neu verbinden."),
        )
            .into_response();
    };
    let back = |params: &[(&str, &str)]| {
        let mut u = url::Url::parse(&p.redirect_uri).expect("registered URI");
        for (k, v) in params {
            u.query_pairs_mut().append_pair(k, v);
        }
        if let Some(st) = &p.client_state {
            u.query_pairs_mut().append_pair("state", st);
        }
        Redirect::to(u.as_str()).into_response()
    };
    if q.error.is_some() {
        return back(&[("error", "access_denied")]);
    }
    let redirect = format!("{}/login/callback", s.oauth.origin);
    let builder = match s
        .google
        .finish(
            q.code.as_deref().unwrap_or_default(),
            &redirect,
            &p.google_verifier,
            &p.nonce,
        )
        .await
    {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "builder login refused");
            return back(&[("error", "access_denied"), ("error_description", &e)]);
        }
    };
    tracing::info!(target: "audit", sub = %builder.sub, email = %builder.email, "builder signed in");
    let code = random();
    s.oauth.codes.lock().unwrap().insert(
        hash(&code),
        Code {
            client_id: p.client_id.clone(),
            redirect_uri: p.redirect_uri.clone(),
            challenge: p.challenge.clone(),
            builder,
            expires: Instant::now() + Duration::from_secs(60),
        },
    );
    back(&[("code", &code)])
}

#[derive(Deserialize)]
pub struct TokenForm {
    grant_type: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    client_id: Option<String>,
    refresh_token: Option<String>,
}

fn issue_access(s: &AppState, b: &Builder, client_id: &str) -> Result<String, Box<Response>> {
    let claims = AccessClaims {
        iss: s.oauth.origin.clone(),
        aud: s.oauth.resource(),
        sub: b.sub.clone(),
        email: b.email.clone(),
        name: b.name.clone(),
        client_id: client_id.into(),
        iat: now(),
        exp: now() + ACCESS_TTL,
    };
    let mut h = Header::new(Algorithm::ES256);
    h.kid = Some(s.key.kid.clone());
    jsonwebtoken::encode(&h, &claims, &s.key.encoding).map_err(|_| {
        Box::new(oauth_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "server_error",
            "signing",
        ))
    })
}

pub async fn token(State(s): State<Arc<AppState>>, Form(f): Form<TokenForm>) -> Response {
    let no_store = [(header::CACHE_CONTROL, "no-store")];
    match f.grant_type.as_str() {
        "authorization_code" => {
            let code = f
                .code
                .as_deref()
                .map(hash)
                .and_then(|h| s.oauth.codes.lock().unwrap().remove(&h));
            let Some(c) = code.filter(|c| c.expires > Instant::now()) else {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "unknown or expired code",
                );
            };
            let verifier_ok = f
                .code_verifier
                .as_deref()
                .map(|v| URL_SAFE_NO_PAD.encode(Sha256::digest(v.as_bytes())))
                == Some(c.challenge.clone());
            if f.client_id.as_deref() != Some(c.client_id.as_str())
                || f.redirect_uri.as_deref() != Some(c.redirect_uri.as_str())
                || !verifier_ok
            {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "client, redirect_uri or PKCE mismatch",
                );
            }
            let access = match issue_access(&s, &c.builder, &c.client_id) {
                Ok(a) => a,
                Err(r) => return *r,
            };
            let refresh = random();
            let mut data = s.oauth.data.lock().await;
            data.refresh.insert(
                hash(&refresh),
                RefreshRecord {
                    family: random(),
                    client_id: c.client_id,
                    sub: c.builder.sub,
                    email: c.builder.email,
                    name: c.builder.name,
                    expires: now() + REFRESH_TTL,
                    used: false,
                },
            );
            s.oauth.save(&data).await;
            (no_store, Json(json!({ "access_token": access, "token_type": "Bearer", "expires_in": ACCESS_TTL, "refresh_token": refresh, "scope": "traum-haft" }))).into_response()
        }
        "refresh_token" => {
            let Some(rt) = f.refresh_token.as_deref() else {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "refresh_token missing",
                );
            };
            let mut data = s.oauth.data.lock().await;
            let Some(rec) = data.refresh.get(&hash(rt)).cloned() else {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "unknown refresh token",
                );
            };
            if rec.used {
                // Reuse of a rotated token: someone has a copy. Kill the family.
                data.refresh.retain(|_, r| r.family != rec.family);
                s.oauth.save(&data).await;
                tracing::warn!(target: "audit", sub = %rec.sub, "refresh token reuse; family revoked");
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "refresh token reused",
                );
            }
            if rec.expires < now() || f.client_id.as_deref() != Some(rec.client_id.as_str()) {
                return oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "expired or wrong client",
                );
            }
            let builder = Builder {
                sub: rec.sub.clone(),
                email: rec.email.clone(),
                name: rec.name.clone(),
            };
            let access = match issue_access(&s, &builder, &rec.client_id) {
                Ok(a) => a,
                Err(r) => return *r,
            };
            let new_rt = random();
            if let Some(old) = data.refresh.get_mut(&hash(rt)) {
                old.used = true;
            }
            data.refresh.insert(
                hash(&new_rt),
                RefreshRecord {
                    used: false,
                    expires: now() + REFRESH_TTL,
                    ..rec
                },
            );
            s.oauth.save(&data).await;
            (no_store, Json(json!({ "access_token": access, "token_type": "Bearer", "expires_in": ACCESS_TTL, "refresh_token": new_rt, "scope": "traum-haft" }))).into_response()
        }
        _ => oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "use authorization_code or refresh_token",
        ),
    }
}

/// 401 that tells MCP clients where to find the authorization server.
pub fn challenge(s: &AppState) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            format!(
                r#"Bearer resource_metadata="{}/.well-known/oauth-protected-resource""#,
                s.oauth.origin
            ),
        )],
        Json(json!({ "error": "unauthorized" })),
    )
        .into_response()
}

pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}
