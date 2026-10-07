//! /_api and the connect flows against mock upstreams.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Form, Query, State as AxState};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use base64::Engine;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::Digest;
use tower::ServiceExt;
use traum_haft_gateway::config::IntegrationsConfig;
use traum_haft_gateway::integrations::Integrations;
use traum_haft_gateway::keys::SigningKey;
use traum_haft_gateway::store::{MemoryStore, SecretStore};
use traum_haft_gateway::{AppState, Config, router};

const CONNECT: &str = "https://connect.apps.example.test";

#[derive(Default)]
struct Mock {
    challenge: Mutex<Option<String>>,
    refresh_count: Mutex<u32>,
    seen: Mutex<Vec<Value>>,
}

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn mock_oauth(m: Arc<Mock>) -> String {
    async fn token(
        AxState(m): AxState<Arc<Mock>>,
        Form(f): Form<HashMap<String, String>>,
    ) -> (StatusCode, Json<Value>) {
        assert_eq!(f.get("client_id").map(String::as_str), Some("cid"));
        assert_eq!(f.get("client_secret").map(String::as_str), Some("csecret"));
        match f.get("grant_type").map(String::as_str) {
            Some("authorization_code") => {
                let verifier = f.get("code_verifier").cloned().unwrap_or_default();
                let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(sha2::Sha256::digest(verifier.as_bytes()));
                if f.get("code").map(String::as_str) != Some("good")
                    || m.challenge.lock().unwrap().as_deref() != Some(challenge.as_str())
                {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": "invalid_grant"})),
                    );
                }
                (
                    StatusCode::OK,
                    Json(
                        json!({"access_token": "at-1", "refresh_token": "rt-1", "expires_in": 3600}),
                    ),
                )
            }
            Some("refresh_token") => {
                let mut n = m.refresh_count.lock().unwrap();
                if f.get("refresh_token") != Some(&format!("rt-{}", *n + 1)) {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": "invalid_grant"})),
                    );
                }
                *n += 1;
                (
                    StatusCode::OK,
                    Json(
                        json!({"access_token": format!("at-{}", *n + 1), "refresh_token": format!("rt-{}", *n + 1), "expires_in": 3600}),
                    ),
                )
            }
            _ => (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "unsupported"})),
            ),
        }
    }
    async fn resources() -> Json<Value> {
        Json(
            json!([{"id": "other-cloud", "url": "https://other.atlassian.net"}, {"id": "cloud-42", "url": "https://isp.atlassian.net"}]),
        )
    }
    async fn api(AxState(m): AxState<Arc<Mock>>, req: Request<Body>) -> Json<Value> {
        let seen = json!({
            "method": req.method().as_str(),
            "uri": req.uri().to_string(),
            "auth": req.headers().get("authorization").and_then(|v| v.to_str().ok()),
        });
        m.seen.lock().unwrap().push(seen.clone());
        Json(seen)
    }
    spawn(
        Router::new()
            .route("/token", post(token))
            .route("/resources", get(resources))
            .route("/ex/jira/{*rest}", any(api))
            .with_state(m),
    )
    .await
}

async fn mock_crm(m: Arc<Mock>) -> String {
    async fn ws_get(
        AxState(m): AxState<Arc<Mock>>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Json<Value> {
        m.seen.lock().unwrap().push(json!(q));
        match q.get("operation").map(String::as_str) {
            Some("getchallenge") => Json(
                json!({"success": true, "result": {"token": "chal123", "serverTime": 0, "expireTime": 300}}),
            ),
            Some(op) if q.get("sessionName").map(String::as_str) == Some("sess-1") => {
                Json(json!({"success": true, "result": {"op": op}}))
            }
            _ => Json(
                json!({"success": false, "error": {"code": "INVALID_SESSIONID", "message": "no"}}),
            ),
        }
    }
    async fn ws_post(
        AxState(m): AxState<Arc<Mock>>,
        Form(f): Form<HashMap<String, String>>,
    ) -> Json<Value> {
        m.seen.lock().unwrap().push(json!(f));
        let expected: String = md5::Md5::digest(b"chal123secret-key")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        match f.get("operation").map(String::as_str) {
            Some("login")
                if f.get("username").map(String::as_str) == Some("alice")
                    && f.get("accessKey") == Some(&expected) =>
            {
                Json(
                    json!({"success": true, "result": {"sessionName": "sess-1", "userId": "19x5"}}),
                )
            }
            Some("login") => Json(
                json!({"success": false, "error": {"code": "INVALID_AUTH_TOKEN", "message": "bad"}}),
            ),
            Some(op) if f.get("sessionName").map(String::as_str) == Some("sess-1") => {
                Json(json!({"success": true, "result": {"op": op}}))
            }
            _ => Json(
                json!({"success": false, "error": {"code": "INVALID_SESSIONID", "message": "no"}}),
            ),
        }
    }
    spawn(
        Router::new()
            .route("/webservice.php", get(ws_get).post(ws_post))
            .with_state(m),
    )
    .await
}

struct H {
    state: Arc<AppState>,
    store: Arc<MemoryStore>,
    oauth: Arc<Mock>,
    crm: Arc<Mock>,
    dir: tempfile::TempDir,
}

async fn harness(integrations_line: &str) -> H {
    let oauth = Arc::new(Mock::default());
    let crm = Arc::new(Mock::default());
    let oauth_url = mock_oauth(oauth.clone()).await;
    let crm_url = mock_crm(crm.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    write_manifest(&dir, integrations_line);
    let providers = IntegrationsConfig::parse(&format!(
        r#"
[connections.atlassian]
kind = "oauth2"
title = "Atlassian"
authorize_url = "{oauth_url}/authorize"
token_url = "{oauth_url}/token"
client_id = "cid"
client_secret = "csecret"
scopes = ["read:jira-work", "offline_access"]
authorize_params = {{ audience = "api.atlassian.com", prompt = "consent" }}
resource_lookup = {{ url = "{oauth_url}/resources", match_field = "url", match_value = "https://isp.atlassian.net", id_field = "id" }}

[providers.jira]
connection = "atlassian"
title = "Jira"
base_url = "{oauth_url}/ex/jira/{{resource_id}}"
scopes.read = {{ label = "lesen", methods = ["GET"] }}
scopes.write = {{ label = "lesen und ändern", methods = ["GET", "POST", "PUT"] }}

[connections.crm]
kind = "crmplus"
title = "CRM Plus"
base_url = "{crm_url}"

[providers.crmplus]
connection = "crm"
title = "CRM Plus"
scopes.read = {{ label = "lesen", operations = ["query", "retrieve", "describe"] }}
"#
    ))
    .unwrap();
    let store = Arc::new(MemoryStore::default());
    let config = Config {
        issuer: "https://apps.example.test".into(),
        apps_domain: "apps.example.test".into(),
        apps_dir: dir.path().join("apps"),
        token_ttl_secs: 600,
    };
    let key = SigningKey::load_or_create(&dir.path().join("key.pem")).unwrap();
    let integrations = Integrations::new(
        providers,
        store.clone(),
        reqwest::Client::new(),
        CONNECT.into(),
    );
    H {
        state: Arc::new(AppState {
            config,
            key,
            integrations,
            feedback: Default::default(),
        }),
        store,
        oauth,
        crm,
        dir,
    }
}

fn write_manifest(dir: &tempfile::TempDir, integrations_line: &str) {
    let release = dir.path().join("apps/foo/current");
    std::fs::create_dir_all(&release).unwrap();
    std::fs::write(
        release.join("app.toml"),
        format!("name = \"foo\"\n{integrations_line}\n"),
    )
    .unwrap();
}

struct Res {
    status: StatusCode,
    headers: HeaderMap,
    text: String,
}
impl Res {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or(Value::Null)
    }
    fn location(&self) -> String {
        self.headers
            .get("location")
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default()
    }
}

async fn send(h: &H, req: Request<Body>) -> Res {
    let res = router(h.state.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let text =
        String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap();
    Res {
        status,
        headers,
        text,
    }
}

fn app_req(method: &str, uri: &str, sub: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("x-user-sub", sub)
        .header("x-user-email", format!("{sub}@x"))
        .header("x-app", "foo")
}
fn connect_req(method: &str, uri: &str, sub: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("x-user-sub", sub)
        .header("x-user-email", format!("{sub}@x"))
        .header("origin", CONNECT)
}
const FORM: &str = "application/x-www-form-urlencoded";
const RET: &str = "app=foo&return=https%3A%2F%2Ffoo.apps.example.test%2F";

/// Runs consent + OAuth for `sub`; returns the stored connection.
async fn connect_jira(h: &H, sub: &str) -> Value {
    let r = send(
        h,
        connect_req("POST", "/c/jira/approve", sub)
            .header("content-type", FORM)
            .body(Body::from(RET))
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.text);
    let auth = url::Url::parse(&r.location()).unwrap();
    let q: HashMap<_, _> = auth.query_pairs().into_owned().collect();
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["audience"], "api.atlassian.com");
    assert_eq!(
        q["redirect_uri"],
        format!("{CONNECT}/oauth/atlassian/callback")
    );
    *h.oauth.challenge.lock().unwrap() = Some(q["code_challenge"].clone());
    let cb = format!("/oauth/atlassian/callback?code=good&state={}", q["state"]);
    let r = send(h, connect_req("GET", &cb, sub).body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.text);
    assert_eq!(r.location(), "https://foo.apps.example.test/");
    h.store
        .get(&format!("users/{sub}/connections/atlassian"))
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn undeclared_provider_is_refused() {
    let h = harness("integrations = []").await;
    let r = send(
        &h,
        app_req("GET", "/_api/jira/rest/api/3/myself", "alice")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unconnected_visitor_gets_connect_url() {
    let h = harness(r#"integrations = ["jira:read"]"#).await;
    let r = send(
        &h,
        app_req("GET", "/_api/jira/rest/api/3/myself", "alice")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.json()["connect_url"], format!("{CONNECT}/c/jira?{RET}"));
}

#[tokio::test]
async fn consent_page_names_app_and_scope() {
    let h = harness(r#"integrations = ["jira:read"]"#).await;
    let r = send(
        &h,
        connect_req("GET", &format!("/c/jira?{RET}"), "alice")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        r.text.contains("<b>foo</b>")
            && r.text.contains("<b>Jira</b>")
            && r.text.contains("<b>lesen</b>"),
        "{}",
        r.text
    );
    // Return address must be the app's own origin.
    let evil = "/c/jira?app=foo&return=https%3A%2F%2Fevil.test%2F";
    assert_eq!(
        send(
            &h,
            connect_req("GET", evil, "alice")
                .body(Body::empty())
                .unwrap()
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn approve_requires_same_origin() {
    let h = harness(r#"integrations = ["jira:read"]"#).await;
    let req = Request::post("/c/jira/approve")
        .header("x-user-sub", "alice")
        .header("x-user-email", "a@x")
        .header("origin", "https://foo.apps.example.test")
        .header("content-type", FORM)
        .body(Body::from(RET))
        .unwrap();
    assert_eq!(send(&h, req).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn oauth_flow_then_calls_as_visitor() {
    let h = harness(r#"integrations = ["jira:read"]"#).await;
    let creds = connect_jira(&h, "alice").await;
    assert_eq!(creds["resource_id"], "cloud-42");
    let r = send(
        &h,
        app_req(
            "GET",
            "/_api/jira/rest/api/3/search?jql=project%3DX",
            "alice",
        )
        .body(Body::empty())
        .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(
        r.json()["uri"],
        "/ex/jira/cloud-42/rest/api/3/search?jql=project%3DX"
    );
    assert_eq!(r.json()["auth"], "Bearer at-1");
    // read scope: no writes
    let r = send(
        &h,
        app_req("POST", "/_api/jira/rest/api/3/issue", "alice")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // bob has not connected
    let r = send(
        &h,
        app_req("GET", "/_api/jira/rest/api/3/myself", "bob")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn callback_only_for_the_user_who_started() {
    let h = harness(r#"integrations = ["jira:read"]"#).await;
    let r = send(
        &h,
        connect_req("POST", "/c/jira/approve", "alice")
            .header("content-type", FORM)
            .body(Body::from(RET))
            .unwrap(),
    )
    .await;
    let state = url::Url::parse(&r.location())
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .to_string();
    let cb = format!("/oauth/atlassian/callback?code=good&state={state}");
    let r = send(
        &h,
        connect_req("GET", &cb, "mallory")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(
        h.store
            .get("users/mallory/connections/atlassian")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        h.store
            .get("users/alice/connections/atlassian")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn expired_token_is_refreshed_and_rotation_stored() {
    let h = harness(r#"integrations = ["jira:read"]"#).await;
    let mut creds = connect_jira(&h, "alice").await;
    creds["expires_at"] = json!(1);
    h.store
        .put("users/alice/connections/atlassian", &creds)
        .await
        .unwrap();
    let r = send(
        &h,
        app_req("GET", "/_api/jira/x", "alice")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.json()["auth"], "Bearer at-2", "{}", r.text);
    let stored = h
        .store
        .get("users/alice/connections/atlassian")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored["refresh_token"], "rt-2",
        "rotated refresh token must be kept"
    );
}

#[tokio::test]
async fn scope_change_requires_new_consent() {
    let h = harness(r#"integrations = ["jira:read"]"#).await;
    connect_jira(&h, "alice").await;
    write_manifest(&h.dir, r#"integrations = ["jira:write"]"#);
    let r = send(
        &h,
        app_req("POST", "/_api/jira/rest/api/3/issue", "alice")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::UNAUTHORIZED,
        "consent was for read only"
    );
}

#[tokio::test]
async fn path_traversal_refused() {
    let h = harness(r#"integrations = ["jira:read"]"#).await;
    connect_jira(&h, "alice").await;
    let r = send(
        &h,
        app_req("GET", "/_api/jira/../../other", "alice")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_ne!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn crm_key_flow_and_operation_allowlist() {
    let h = harness(r#"integrations = ["crmplus:read"]"#).await;
    let r = send(
        &h,
        connect_req("POST", "/c/crmplus/approve", "alice")
            .header("content-type", FORM)
            .body(Body::from(RET))
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.text.contains("Zugriffsschlüssel"));
    // wrong key: nothing stored
    let bad = format!("{RET}&username=alice&access_key=wrong");
    let r = send(
        &h,
        connect_req("POST", "/c/crmplus/key", "alice")
            .header("content-type", FORM)
            .body(Body::from(bad))
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        h.store
            .get("users/alice/connections/crm")
            .await
            .unwrap()
            .is_none()
    );
    // right key
    let good = format!("{RET}&username=alice&access_key=secret-key");
    let r = send(
        &h,
        connect_req("POST", "/c/crmplus/key", "alice")
            .header("content-type", FORM)
            .body(Body::from(good))
            .unwrap(),
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.text);
    // allowed operation, with an attempt to smuggle another one
    let r = send(&h, app_req("GET", "/_api/crmplus/query?query=SELECT%20*%20FROM%20Accounts%3B&operation=deleteuser&sessionName=x", "alice").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.json()["result"]["op"], "query");
    let last = h.crm.seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last["operation"], "query");
    assert_eq!(last["sessionName"], "sess-1");
    // dangerous operations never pass
    for op in [
        "deleteuser",
        "changepassword",
        "updateaccess",
        "delete",
        "create",
    ] {
        let r = send(
            &h,
            app_req("POST", &format!("/_api/crmplus/{op}"), "alice")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{op}");
    }
}
