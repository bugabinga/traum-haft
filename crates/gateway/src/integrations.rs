//! Calls to Jira, CRM Plus and friends as the visitor, plus the pages on
//! `connect.<apps_domain>` where visitors connect accounts and consent.
//!
//! Order of checks for `/_api/<provider>/<path>` on an app origin:
//! 1. the app declares `<provider>:<scope>` in its app.toml,
//! 2. the scope allows this method (OAuth2) or operation (CRM Plus),
//! 3. the visitor consented to this app using this provider with this scope,
//! 4. the visitor has connected the account.
//!
//! Missing 3 or 4 answers 401 with a `connect_url`; the app shows a button.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Form, Path, Query, RawQuery, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use html_escape::encode_text as esc;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::{ConnectionConfig, CrmPlusConfig, IntegrationsConfig, OAuth2Config, TokenAuth};
use crate::store::SecretStore;
use crate::{AppState, now, visitor};

pub struct Integrations {
    pub config: IntegrationsConfig,
    pub store: Arc<dyn SecretStore>,
    pub http: reqwest::Client,
    /// `https://connect.<apps_domain>`
    pub connect_origin: String,
    pending: std::sync::Mutex<HashMap<String, PendingAuth>>,
    crm_sessions: std::sync::Mutex<HashMap<String, (String, Instant)>>,
    refresh_lock: tokio::sync::Mutex<()>,
}

struct PendingAuth {
    sub: String,
    connection: String,
    verifier: String,
    return_to: String,
    expires: Instant,
}

impl Integrations {
    pub fn new(
        config: IntegrationsConfig,
        store: Arc<dyn SecretStore>,
        http: reqwest::Client,
        connect_origin: String,
    ) -> Self {
        Self {
            config,
            store,
            http,
            connect_origin,
            pending: Default::default(),
            crm_sessions: Default::default(),
            refresh_lock: tokio::sync::Mutex::new(()),
        }
    }
}

/// Store keys use only safe characters, whatever the identity provider sends.
fn safe(s: &str) -> String {
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && !s.is_empty()
    {
        s.into()
    } else {
        format!(
            "x{}",
            s.bytes().map(|b| format!("{b:02x}")).collect::<String>()
        )
    }
}
fn conn_key(sub: &str, conn: &str) -> String {
    format!("users/{}/connections/{}", safe(sub), safe(conn))
}
fn consent_key(sub: &str, app: &str, provider: &str) -> String {
    format!(
        "users/{}/consents/{}/{}",
        safe(sub),
        safe(app),
        safe(provider)
    )
}

fn random_token() -> String {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("random");
    URL_SAFE_NO_PAD.encode(b)
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({ "error": msg }))).into_response()
}

/// The scope an app declares for a provider in its deployed app.toml.
pub fn declared_scope(apps_dir: &std::path::Path, app: &str, provider: &str) -> Option<String> {
    let text = std::fs::read_to_string(apps_dir.join(app).join("current").join("app.toml")).ok()?;
    let manifest: toml::Table = toml::from_str(&text).ok()?;
    manifest
        .get("integrations")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str())
        .find_map(|entry| {
            let (p, scope) = entry.split_once(':')?;
            (p == provider).then(|| scope.to_string())
        })
}

fn app_origin(state: &AppState, app: &str) -> String {
    format!("https://{app}.{}", state.config.apps_domain)
}

fn connect_url(state: &AppState, provider: &str, app: &str) -> String {
    let ret = format!("{}/", app_origin(state, app));
    let mut url = url::Url::parse(&format!(
        "{}/c/{provider}",
        state.integrations.connect_origin
    ))
    .expect("connect origin");
    url.query_pairs_mut()
        .append_pair("app", app)
        .append_pair("return", &ret);
    url.into()
}

// ---------------------------------------------------------------- /_api

pub async fn api(
    State(state): State<Arc<AppState>>,
    method: Method,
    Path((provider, path)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(v) = visitor(&headers) else {
        return err(StatusCode::UNAUTHORIZED, "not signed in");
    };
    if provider == "_platform" {
        return crate::feedback::handle(&state, &v, &method, &path, &body).await;
    }
    let integ = &state.integrations;
    let Some(pcfg) = integ.config.providers.get(&provider) else {
        return err(StatusCode::NOT_FOUND, "unknown provider");
    };
    let Some(scope) = declared_scope(&state.config.apps_dir, &v.app, &provider) else {
        return err(StatusCode::FORBIDDEN, "provider not declared in app.toml");
    };
    let Some(rule) = pcfg.scopes.get(&scope) else {
        return err(StatusCode::FORBIDDEN, "scope not offered by this provider");
    };
    let conn_cfg = &integ.config.connections[&pcfg.connection];
    // 2. what the scope allows
    let allowed = match conn_cfg {
        ConnectionConfig::Oauth2(_) => rule
            .methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case(method.as_str())),
        ConnectionConfig::Crmplus(_) => {
            let op = path.split('/').next().unwrap_or_default();
            rule.operations.iter().any(|o| o == op)
        }
    };
    if !allowed {
        tracing::warn!(app = %v.app, %provider, %scope, %method, %path, "refused by scope");
        return err(StatusCode::FORBIDDEN, "not allowed by the declared scope");
    }
    // 3. consent, 4. connection
    let consent = integ
        .store
        .get(&consent_key(&v.sub, &v.app, &provider))
        .await;
    let creds = integ.store.get(&conn_key(&v.sub, &pcfg.connection)).await;
    let (consent, creds) = match (consent, creds) {
        (Ok(c), Ok(k)) => (c, k),
        _ => return err(StatusCode::SERVICE_UNAVAILABLE, "secret store unavailable"),
    };
    let consented = consent.as_ref().and_then(|c| c["scope"].as_str()) == Some(scope.as_str());
    let (true, Some(creds)) = (consented, creds) else {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({ "error": "connect_required", "provider": provider, "connect_url": connect_url(&state, &provider, &v.app) })),
        )
            .into_response();
    };
    tracing::info!(target: "audit", sub = %v.sub, app = %v.app, %provider, %method, %path, "integration call");
    match conn_cfg {
        ConnectionConfig::Oauth2(o) => {
            oauth2_call(
                &state,
                &v.sub,
                &pcfg.connection,
                o,
                pcfg.base_url.as_deref(),
                creds,
                method,
                &path,
                query,
                &headers,
                body,
            )
            .await
        }
        ConnectionConfig::Crmplus(c) => {
            crm_call(
                &state,
                &v.sub,
                &pcfg.connection,
                c,
                creds,
                &method,
                &path,
                query,
                body,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn oauth2_call(
    state: &AppState,
    sub: &str,
    conn: &str,
    cfg: &OAuth2Config,
    base_url: Option<&str>,
    creds: Value,
    method: Method,
    path: &str,
    query: Option<String>,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    if path.split('/').any(|seg| seg == ".." || seg == ".") {
        return err(StatusCode::BAD_REQUEST, "invalid path");
    }
    let creds = match fresh_token(state, sub, conn, cfg, creds).await {
        Ok(c) => c,
        Err(_) => {
            return err(
                StatusCode::UNAUTHORIZED,
                "connection expired, connect again",
            );
        }
    };
    let rid = creds["resource_id"].as_str().unwrap_or_default();
    let base = base_url.unwrap_or_default().replace("{resource_id}", rid);
    let mut url = format!("{}/{}", base.trim_end_matches('/'), path);
    if let Some(q) = query {
        url = format!("{url}?{q}");
    }
    let mut req = state
        .integrations
        .http
        .request(method, &url)
        .bearer_auth(creds["access_token"].as_str().unwrap_or_default())
        .header(
            header::ACCEPT,
            headers
                .get(header::ACCEPT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/json"),
        );
    if let Some(ct) = headers.get(header::CONTENT_TYPE) {
        req = req.header(header::CONTENT_TYPE, ct);
    }
    if !body.is_empty() {
        req = req.body(body);
    }
    passthrough(req.send().await).await
}

async fn passthrough(res: Result<reqwest::Response, reqwest::Error>) -> Response {
    match res {
        Ok(r) => {
            let status =
                StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let ct = r
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_string();
            match r.bytes().await {
                Ok(b) => (status, [(header::CONTENT_TYPE, ct)], b).into_response(),
                Err(_) => err(StatusCode::BAD_GATEWAY, "upstream read failed"),
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "upstream call failed");
            err(StatusCode::BAD_GATEWAY, "upstream unreachable")
        }
    }
}

/// Returns credentials with a valid access token, refreshing (and storing
/// the rotated refresh token) when needed.
async fn fresh_token(
    state: &AppState,
    sub: &str,
    conn: &str,
    cfg: &OAuth2Config,
    creds: Value,
) -> Result<Value, ()> {
    if creds["expires_at"].as_u64().unwrap_or(0) > now() + 60 {
        return Ok(creds);
    }
    let integ = &state.integrations;
    let _guard = integ.refresh_lock.lock().await;
    // Another request may have refreshed while we waited.
    let current = integ
        .store
        .get(&conn_key(sub, conn))
        .await
        .map_err(|_| ())?
        .ok_or(())?;
    if current["expires_at"].as_u64().unwrap_or(0) > now() + 60 {
        return Ok(current);
    }
    let refresh = current["refresh_token"].as_str().ok_or(())?;
    let tokens = token_request(
        integ,
        cfg,
        &[("grant_type", "refresh_token"), ("refresh_token", refresh)],
    )
    .await?;
    let mut updated = current.clone();
    updated["access_token"] = tokens["access_token"].clone();
    updated["expires_at"] = json!(now() + tokens["expires_in"].as_u64().unwrap_or(3600));
    if tokens["refresh_token"].is_string() {
        updated["refresh_token"] = tokens["refresh_token"].clone();
    }
    integ
        .store
        .put(&conn_key(sub, conn), &updated)
        .await
        .map_err(|_| ())?;
    Ok(updated)
}

async fn token_request(
    integ: &Integrations,
    cfg: &OAuth2Config,
    params: &[(&str, &str)],
) -> Result<Value, ()> {
    let mut all: Vec<(&str, &str)> = params.to_vec();
    let req = integ
        .http
        .post(&cfg.token_url)
        .header(header::ACCEPT, "application/json");
    let req = match cfg.token_auth {
        TokenAuth::Form => {
            all.push(("client_id", &cfg.client_id));
            all.push(("client_secret", &cfg.client_secret));
            req.form(&all)
        }
        TokenAuth::Basic => req
            .basic_auth(&cfg.client_id, Some(&cfg.client_secret))
            .form(&all),
        TokenAuth::Json => {
            let mut obj: serde_json::Map<String, Value> =
                all.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
            obj.insert("client_id".into(), json!(cfg.client_id));
            obj.insert("client_secret".into(), json!(cfg.client_secret));
            req.json(&obj)
        }
    };
    let res = req
        .send()
        .await
        .map_err(|e| tracing::warn!(error = %e, "token endpoint unreachable"))?;
    if !res.status().is_success() {
        tracing::warn!(status = %res.status(), "token endpoint refused");
        return Err(());
    }
    let body: Value = res.json().await.map_err(|_| ())?;
    if body["access_token"].is_string() {
        Ok(body)
    } else {
        Err(())
    }
}

// ---------------------------------------------------------------- CRM Plus

const CRM_SESSION_TTL: Duration = Duration::from_secs(20 * 60);

#[allow(clippy::too_many_arguments)]
async fn crm_call(
    state: &AppState,
    sub: &str,
    conn: &str,
    cfg: &CrmPlusConfig,
    creds: Value,
    method: &Method,
    path: &str,
    query: Option<String>,
    body: Bytes,
) -> Response {
    let op = path.split('/').next().unwrap_or_default().to_string();
    // Parameters from the app, minus anything that would change who or what.
    let parse = |s: &str| -> Vec<(String, String)> {
        url::form_urlencoded::parse(s.as_bytes())
            .into_owned()
            .filter(|(k, _)| {
                !matches!(
                    k.to_ascii_lowercase().as_str(),
                    "operation" | "sessionname" | "username" | "accesskey"
                )
            })
            .collect()
    };
    let params = if *method == Method::POST {
        parse(std::str::from_utf8(&body).unwrap_or_default())
    } else {
        parse(query.as_deref().unwrap_or_default())
    };
    let cache_key = format!("{}/{}", safe(sub), conn);
    for attempt in 0..2 {
        let session = match crm_session(state, &cache_key, cfg, &creds, attempt > 0).await {
            Ok(s) => s,
            Err(msg) => return err(StatusCode::UNAUTHORIZED, msg),
        };
        let url = format!("{}/webservice.php", cfg.base_url.trim_end_matches('/'));
        let mut all = vec![
            ("operation".to_string(), op.clone()),
            ("sessionName".to_string(), session),
        ];
        all.extend(params.iter().cloned());
        let req = if *method == Method::POST {
            state.integrations.http.post(&url).form(&all)
        } else {
            state.integrations.http.get(&url).query(&all)
        };
        let res = match req.send().await {
            Ok(r) => r,
            Err(_) => return err(StatusCode::BAD_GATEWAY, "CRM Plus unreachable"),
        };
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let expired = parsed["success"] == json!(false)
            && parsed["error"]["code"]
                .as_str()
                .is_some_and(|c| c.contains("INVALID_SESSIONID") || c.contains("SESSION"));
        if expired && attempt == 0 {
            state
                .integrations
                .crm_sessions
                .lock()
                .unwrap()
                .remove(&cache_key);
            continue;
        }
        return (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            [(header::CONTENT_TYPE, "application/json")],
            text,
        )
            .into_response();
    }
    err(StatusCode::BAD_GATEWAY, "CRM Plus session failed")
}

async fn crm_session(
    state: &AppState,
    cache_key: &str,
    cfg: &CrmPlusConfig,
    creds: &Value,
    force: bool,
) -> Result<String, &'static str> {
    if !force
        && let Some((s, until)) = state
            .integrations
            .crm_sessions
            .lock()
            .unwrap()
            .get(cache_key)
            && Instant::now() < *until {
                return Ok(s.clone());
            }
    let user = creds["username"].as_str().ok_or("CRM Plus not connected")?;
    let key = creds["access_key"]
        .as_str()
        .ok_or("CRM Plus not connected")?;
    let session = crm_login(&state.integrations.http, &cfg.base_url, user, key).await?;
    state.integrations.crm_sessions.lock().unwrap().insert(
        cache_key.into(),
        (session.clone(), Instant::now() + CRM_SESSION_TTL),
    );
    Ok(session)
}

/// getchallenge + login with md5(token + accessKey); returns sessionName.
pub async fn crm_login(
    http: &reqwest::Client,
    base_url: &str,
    username: &str,
    access_key: &str,
) -> Result<String, &'static str> {
    let url = format!("{}/webservice.php", base_url.trim_end_matches('/'));
    let challenge: Value = http
        .get(&url)
        .query(&[("operation", "getchallenge"), ("username", username)])
        .send()
        .await
        .map_err(|_| "CRM Plus unreachable")?
        .json()
        .await
        .map_err(|_| "CRM Plus answered unexpectedly")?;
    let token = challenge["result"]["token"]
        .as_str()
        .ok_or("CRM Plus refused the user name")?;
    let digest: String = md5::Md5::digest(format!("{token}{access_key}").as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let login: Value = http
        .post(&url)
        .form(&[
            ("operation", "login"),
            ("username", username),
            ("accessKey", digest.as_str()),
        ])
        .send()
        .await
        .map_err(|_| "CRM Plus unreachable")?
        .json()
        .await
        .map_err(|_| "CRM Plus answered unexpectedly")?;
    login["result"]["sessionName"]
        .as_str()
        .map(String::from)
        .ok_or("CRM Plus refused the access key")
}

// ---------------------------------------------------------------- connect pages

#[derive(Deserialize)]
pub struct ConnectQuery {
    app: String,
    #[serde(rename = "return")]
    return_to: String,
}

fn page(title: &str, body: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html><html lang="de"><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{t}</title><style>body{{font-family:system-ui,sans-serif;max-width:34rem;margin:3rem auto;padding:0 1rem}}button{{padding:.5rem 1rem}}input{{width:100%;padding:.4rem;margin:.2rem 0 .8rem}}</style>
<h1>{t}</h1>{body}</html>"#,
        t = esc(title)
    ))
}

/// The return target must be the app's own origin.
fn valid_return(state: &AppState, app: &str, return_to: &str) -> bool {
    crate::is_valid_app_name(app) && return_to.starts_with(&format!("{}/", app_origin(state, app)))
}

fn same_origin_post(state: &AppState, headers: &HeaderMap) -> bool {
    headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
        == Some(state.integrations.connect_origin.as_str())
}

/// `GET /c/<provider>?app=&return=`: consent page.
pub async fn consent_page(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
    Query(q): Query<ConnectQuery>,
    headers: HeaderMap,
) -> Response {
    if visitor_sub(&headers).is_none() {
        return err(StatusCode::UNAUTHORIZED, "not signed in");
    }
    let Some(pcfg) = state.integrations.config.providers.get(&provider) else {
        return err(StatusCode::NOT_FOUND, "unknown provider");
    };
    if !valid_return(&state, &q.app, &q.return_to) {
        return err(StatusCode::BAD_REQUEST, "invalid app or return address");
    }
    let Some(scope) = declared_scope(&state.config.apps_dir, &q.app, &provider) else {
        return err(StatusCode::FORBIDDEN, "this app does not use this provider");
    };
    let label = pcfg
        .scopes
        .get(&scope)
        .map(|r| r.label.as_str())
        .unwrap_or(&scope);
    let body = format!(
        r#"<p>Die App <b>{app}</b> möchte <b>{title}</b> in deinem Namen <b>{label}</b>.</p>
<p>Sie sieht dabei nie dein Passwort oder deinen Schlüssel. Du kannst das jederzeit auf dieser Seite widerrufen.</p>
<form method="post" action="/c/{provider}/approve">
<input type="hidden" name="app" value="{app}"><input type="hidden" name="return" value="{ret}">
<button type="submit">Erlauben</button> <a href="{ret}">Abbrechen</a></form>"#,
        app = esc(&q.app),
        title = esc(&pcfg.title),
        label = esc(label),
        provider = esc(&provider),
        ret = html_escape::encode_double_quoted_attribute(&q.return_to),
    );
    page(&format!("{} verbinden", pcfg.title), &body).into_response()
}

fn visitor_sub(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-user-sub")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// `POST /c/<provider>/approve`: records consent, then connects if needed.
pub async fn approve(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    Form(q): Form<ConnectQuery>,
) -> Response {
    let Some(sub) = visitor_sub(&headers) else {
        return err(StatusCode::UNAUTHORIZED, "not signed in");
    };
    if !same_origin_post(&state, &headers) {
        return err(StatusCode::FORBIDDEN, "cross-site request refused");
    }
    let integ = &state.integrations;
    let Some(pcfg) = integ.config.providers.get(&provider) else {
        return err(StatusCode::NOT_FOUND, "unknown provider");
    };
    if !valid_return(&state, &q.app, &q.return_to) {
        return err(StatusCode::BAD_REQUEST, "invalid app or return address");
    }
    let Some(scope) = declared_scope(&state.config.apps_dir, &q.app, &provider) else {
        return err(StatusCode::FORBIDDEN, "this app does not use this provider");
    };
    if integ
        .store
        .put(
            &consent_key(&sub, &q.app, &provider),
            &json!({ "scope": scope, "at": now() }),
        )
        .await
        .is_err()
    {
        return err(StatusCode::SERVICE_UNAVAILABLE, "secret store unavailable");
    }
    tracing::info!(target: "audit", %sub, app = %q.app, %provider, %scope, "consent granted");
    if matches!(
        integ.store.get(&conn_key(&sub, &pcfg.connection)).await,
        Ok(Some(_))
    ) {
        return Redirect::to(&q.return_to).into_response();
    }
    match &integ.config.connections[&pcfg.connection] {
        ConnectionConfig::Oauth2(cfg) => {
            let state_token = random_token();
            let verifier = random_token();
            let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
            integ.pending.lock().unwrap().insert(
                state_token.clone(),
                PendingAuth {
                    sub,
                    connection: pcfg.connection.clone(),
                    verifier,
                    return_to: q.return_to,
                    expires: Instant::now() + Duration::from_secs(600),
                },
            );
            let mut url = match url::Url::parse(&cfg.authorize_url) {
                Ok(u) => u,
                Err(_) => return err(StatusCode::INTERNAL_SERVER_ERROR, "provider misconfigured"),
            };
            {
                let mut qp = url.query_pairs_mut();
                qp.append_pair("response_type", "code")
                    .append_pair("client_id", &cfg.client_id)
                    .append_pair(
                        "redirect_uri",
                        &format!(
                            "{}/oauth/{}/callback",
                            integ.connect_origin, pcfg.connection
                        ),
                    )
                    .append_pair("scope", &cfg.scopes.join(" "))
                    .append_pair("state", &state_token)
                    .append_pair("code_challenge", &challenge)
                    .append_pair("code_challenge_method", "S256");
                for (k, v) in &cfg.authorize_params {
                    qp.append_pair(k, v);
                }
            }
            Redirect::to(url.as_str()).into_response()
        }
        ConnectionConfig::Crmplus(cfg) => {
            let body = format!(
                r#"<p>Melde <b>{app}</b> bei {title} an: Benutzername und Zugriffsschlüssel aus <i>Meine Einstellungen</i> → Zugriffsschlüssel.</p>
<form method="post" action="/c/{provider}/key">
<input type="hidden" name="app" value="{app}"><input type="hidden" name="return" value="{ret}">
<label>Benutzername<input name="username" required autocomplete="username"></label>
<label>Zugriffsschlüssel<input name="access_key" required type="password" autocomplete="off"></label>
<button type="submit">Verbinden</button></form>"#,
                app = esc(&q.app),
                title = esc(&cfg.title),
                provider = esc(&provider),
                ret = html_escape::encode_double_quoted_attribute(&q.return_to),
            );
            page(&format!("{} verbinden", cfg.title), &body).into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// `GET /oauth/<connection>/callback`
pub async fn oauth_callback(
    State(state): State<Arc<AppState>>,
    Path(connection): Path<String>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    let Some(sub) = visitor_sub(&headers) else {
        return err(StatusCode::UNAUTHORIZED, "not signed in");
    };
    let integ = &state.integrations;
    let pending = q
        .state
        .as_deref()
        .and_then(|s| integ.pending.lock().unwrap().remove(s));
    let Some(p) = pending.filter(|p| p.expires > Instant::now() && p.connection == connection)
    else {
        return err(StatusCode::BAD_REQUEST, "unknown or expired login attempt");
    };
    // The account is stored for whoever started the flow, and only them.
    if p.sub != sub {
        return err(
            StatusCode::FORBIDDEN,
            "login attempt belongs to another user",
        );
    }
    if let Some(e) = q.error {
        return page(
            "Nicht verbunden",
            &format!("<p>Abgebrochen: {}</p>", esc(&e)),
        )
        .into_response();
    }
    let Some(ConnectionConfig::Oauth2(cfg)) = integ.config.connections.get(&connection) else {
        return err(StatusCode::NOT_FOUND, "unknown connection");
    };
    let redirect_uri = format!("{}/oauth/{connection}/callback", integ.connect_origin);
    let Ok(tokens) = token_request(
        integ,
        cfg,
        &[
            ("grant_type", "authorization_code"),
            ("code", q.code.as_deref().unwrap_or_default()),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", &p.verifier),
        ],
    )
    .await
    else {
        return err(StatusCode::BAD_GATEWAY, "token exchange failed");
    };
    let mut creds = json!({
        "access_token": tokens["access_token"],
        "refresh_token": tokens["refresh_token"],
        "expires_at": now() + tokens["expires_in"].as_u64().unwrap_or(3600),
    });
    if let Some(lookup) = &cfg.resource_lookup {
        let id = async {
            let list: Value = integ
                .http
                .get(&lookup.url)
                .bearer_auth(tokens["access_token"].as_str()?)
                .send()
                .await
                .ok()?
                .json()
                .await
                .ok()?;
            list.as_array()?
                .iter()
                .find(|r| r[&lookup.match_field].as_str() == Some(lookup.match_value.as_str()))?
                [&lookup.id_field]
                .as_str()
                .map(String::from)
        }
        .await;
        match id {
            Some(id) => creds["resource_id"] = json!(id),
            None => {
                return page(
                    "Nicht verbunden",
                    "<p>Dein Konto hat keinen Zugriff auf die Firmen-Instanz.</p>",
                )
                .into_response();
            }
        }
    }
    if integ
        .store
        .put(&conn_key(&sub, &connection), &creds)
        .await
        .is_err()
    {
        return err(StatusCode::SERVICE_UNAVAILABLE, "secret store unavailable");
    }
    tracing::info!(target: "audit", %sub, %connection, "account connected");
    Redirect::to(&p.return_to).into_response()
}

#[derive(Deserialize)]
pub struct KeyForm {
    app: String,
    #[serde(rename = "return")]
    return_to: String,
    username: String,
    access_key: String,
}

/// `POST /c/<provider>/key`: CRM Plus login name + access key.
pub async fn submit_key(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    Form(f): Form<KeyForm>,
) -> Response {
    let Some(sub) = visitor_sub(&headers) else {
        return err(StatusCode::UNAUTHORIZED, "not signed in");
    };
    if !same_origin_post(&state, &headers) {
        return err(StatusCode::FORBIDDEN, "cross-site request refused");
    }
    if !valid_return(&state, &f.app, &f.return_to) {
        return err(StatusCode::BAD_REQUEST, "invalid app or return address");
    }
    let integ = &state.integrations;
    let Some(pcfg) = integ.config.providers.get(&provider) else {
        return err(StatusCode::NOT_FOUND, "unknown provider");
    };
    let Some(ConnectionConfig::Crmplus(cfg)) = integ.config.connections.get(&pcfg.connection)
    else {
        return err(StatusCode::BAD_REQUEST, "not a key-based provider");
    };
    // A successful login proves the key belongs to this login name.
    if let Err(msg) = crm_login(
        &integ.http,
        &cfg.base_url,
        f.username.trim(),
        f.access_key.trim(),
    )
    .await
    {
        return page(
            "Nicht verbunden",
            &format!(
                "<p>{}</p><p><a href=\"javascript:history.back()\">Zurück</a></p>",
                esc(msg)
            ),
        )
        .into_response();
    }
    let creds = json!({ "username": f.username.trim(), "access_key": f.access_key.trim() });
    if integ
        .store
        .put(&conn_key(&sub, &pcfg.connection), &creds)
        .await
        .is_err()
    {
        return err(StatusCode::SERVICE_UNAVAILABLE, "secret store unavailable");
    }
    tracing::info!(target: "audit", %sub, connection = %pcfg.connection, "account connected");
    Redirect::to(&f.return_to).into_response()
}
