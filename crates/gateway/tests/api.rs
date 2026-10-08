use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde_json::Value;
use tower::ServiceExt;
use traum_haft_gateway::keys::SigningKey;
use traum_haft_gateway::{AppClaims, AppState, Config, router};

const ISSUER: &str = "https://apps.example.test";

struct Harness {
    state: Arc<AppState>,
    _dir: tempfile::TempDir,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("apps/foo")).unwrap();
    let key = SigningKey::load_or_create(&dir.path().join("key.pem")).unwrap();
    let config = Config {
        issuer: ISSUER.into(),
        apps_domain: "apps.example.test".into(),
        apps_dir: dir.path().join("apps"),
        token_ttl_secs: 600,
        werk_domain: None,
    };
    let integrations = traum_haft_gateway::integrations::Integrations::new(
        Default::default(),
        Arc::new(traum_haft_gateway::store::MemoryStore::default()),
        reqwest::Client::new(),
        "https://connect.apps.example.test".into(),
    );
    Harness {
        state: Arc::new(AppState {
            config,
            key,
            integrations,
            feedback: Default::default(),
        }),
        _dir: dir,
    }
}

async fn call(h: &Harness, req: Request<Body>) -> (StatusCode, Value) {
    let res = router(h.state.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn as_visitor(path: &str, app: &str, sub: &str) -> axum::http::request::Builder {
    Request::get(path)
        .header("x-user-sub", sub)
        .header("x-user-email", format!("{sub}@example.test"))
        .header("x-app", app)
}

async fn token_for(h: &Harness, app: &str, sub: &str) -> String {
    let (status, body) = call(
        h,
        as_visitor("/_auth/token", app, sub)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    body["token"].as_str().unwrap().to_string()
}

async fn verify(h: &Harness, app: &str, sub: &str, bearer: Option<&str>) -> StatusCode {
    let mut req = as_visitor("/_internal/stdb/verify", app, sub);
    if let Some(b) = bearer {
        req = req.header("authorization", format!("Bearer {b}"));
    }
    call(h, req.body(Body::empty()).unwrap()).await.0
}

#[tokio::test]
async fn discovery_and_jwks_describe_the_signing_key() {
    let h = harness();
    let (s, disco) = call(
        &h,
        Request::get("/.well-known/openid-configuration")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(disco["issuer"], ISSUER);
    assert_eq!(disco["jwks_uri"], format!("{ISSUER}/.well-known/jwks.json"));
    let (_, jwks) = call(
        &h,
        Request::get("/.well-known/jwks.json")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(jwks["keys"][0]["kid"], h.state.key.kid.as_str());
    assert_eq!(jwks["keys"][0]["crv"], "P-256");
    assert!(
        jwks["keys"][0].get("d").is_none(),
        "private key must never be published"
    );
}

#[tokio::test]
async fn token_requires_identity_headers() {
    let h = harness();
    let (s, _) = call(
        &h,
        Request::get("/_auth/token").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let req = Request::get("/_auth/token")
        .header("x-user-sub", "alice")
        .header("x-app", "foo")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        call(&h, req).await.0,
        StatusCode::UNAUTHORIZED,
        "email missing"
    );
    let req = as_visitor("/_auth/token", "Bad_Name", "alice")
        .body(Body::empty())
        .unwrap();
    assert_eq!(call(&h, req).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn token_carries_visitor_and_app() {
    let h = harness();
    let token = token_for(&h, "foo", "alice").await;
    let mut v = jsonwebtoken::Validation::new(Algorithm::ES256);
    v.set_audience(&["foo"]);
    let data = jsonwebtoken::decode::<AppClaims>(&token, &h.state.key.decoding, &v).unwrap();
    assert_eq!(data.claims.sub, "alice");
    assert_eq!(data.claims.aud, "foo");
    assert_eq!(data.claims.iss, ISSUER);
    assert_eq!(data.claims.exp - data.claims.iat, 600);
    assert_eq!(data.header.kid.as_deref(), Some(h.state.key.kid.as_str()));
}

#[tokio::test]
async fn exchange_accepts_own_token() {
    let h = harness();
    let token = token_for(&h, "foo", "alice").await;
    assert_eq!(
        verify(&h, "foo", "alice", Some(&token)).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn exchange_refuses_other_app_other_user_and_garbage() {
    let h = harness();
    let token = token_for(&h, "foo", "alice").await;
    assert_eq!(
        verify(&h, "bar", "alice", Some(&token)).await,
        StatusCode::UNAUTHORIZED,
        "other app"
    );
    assert_eq!(
        verify(&h, "foo", "bob", Some(&token)).await,
        StatusCode::UNAUTHORIZED,
        "other visitor"
    );
    assert_eq!(
        verify(&h, "foo", "alice", None).await,
        StatusCode::UNAUTHORIZED,
        "no bearer"
    );
    assert_eq!(
        verify(&h, "foo", "alice", Some("not.a.jwt")).await,
        StatusCode::UNAUTHORIZED,
        "garbage"
    );
    let req = Request::get("/_internal/stdb/verify")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        call(&h, req).await.0,
        StatusCode::UNAUTHORIZED,
        "no identity headers"
    );
}

#[tokio::test]
async fn exchange_refuses_tokens_signed_elsewhere() {
    let h = harness();
    let claims = AppClaims {
        iss: ISSUER.into(),
        sub: "alice".into(),
        aud: "foo".into(),
        email: "alice@example.test".into(),
        iat: 0,
        exp: u64::MAX / 2,
    };
    // Another ES256 key with the right claims and even the right kid.
    let other = tempfile::tempdir().unwrap();
    let other_key = SigningKey::load_or_create(&other.path().join("k.pem")).unwrap();
    let mut header = Header::new(Algorithm::ES256);
    header.kid = Some(h.state.key.kid.clone());
    let forged = jsonwebtoken::encode(&header, &claims, &other_key.encoding).unwrap();
    assert_eq!(
        verify(&h, "foo", "alice", Some(&forged)).await,
        StatusCode::UNAUTHORIZED,
        "foreign key"
    );
    // Algorithm confusion: HS256 with the public key as the secret.
    let hs = jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(h.state.key.jwk.to_string().as_bytes()),
    )
    .unwrap();
    assert_eq!(
        verify(&h, "foo", "alice", Some(&hs)).await,
        StatusCode::UNAUTHORIZED,
        "HS256"
    );
    // alg=none.
    let none = format!(
        "{}.{}.",
        base64_url(r#"{"alg":"none","typ":"JWT"}"#),
        base64_url(&serde_json::to_string(&claims).unwrap())
    );
    assert_eq!(
        verify(&h, "foo", "alice", Some(&none)).await,
        StatusCode::UNAUTHORIZED,
        "alg none"
    );
}

#[tokio::test]
async fn exchange_refuses_expired_and_wrong_issuer() {
    let h = harness();
    let sign = |iss: &str, exp: u64| {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(h.state.key.kid.clone());
        let claims = AppClaims {
            iss: iss.into(),
            sub: "alice".into(),
            aud: "foo".into(),
            email: "alice@example.test".into(),
            iat: 0,
            exp,
        };
        jsonwebtoken::encode(&header, &claims, &h.state.key.encoding).unwrap()
    };
    assert_eq!(
        verify(&h, "foo", "alice", Some(&sign(ISSUER, 1_000))).await,
        StatusCode::UNAUTHORIZED,
        "expired"
    );
    assert_eq!(
        verify(
            &h,
            "foo",
            "alice",
            Some(&sign("https://evil.test", u64::MAX / 2))
        )
        .await,
        StatusCode::UNAUTHORIZED,
        "issuer"
    );
}

#[tokio::test]
async fn tls_ask_only_for_deployed_apps() {
    let h = harness();
    let ask = |d: &str| {
        Request::get(format!("/_internal/tls-ask?domain={d}"))
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        call(&h, ask("foo.apps.example.test")).await.0,
        StatusCode::OK
    );
    // Platform hosts fall under the wildcard's on-demand policy too.
    assert_eq!(
        call(&h, ask("connect.apps.example.test")).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&h, ask("mcp.apps.example.test")).await.0,
        StatusCode::OK
    );
    for d in [
        "bar.apps.example.test",
        "www.apps.example.test",
        "x.foo.apps.example.test",
        "foo.evil.test",
        "apps.example.test",
        "..%2Ffoo.apps.example.test",
    ] {
        assert_eq!(call(&h, ask(d)).await.0, StatusCode::NOT_FOUND, "{d}");
    }
}

#[test]
fn key_is_persisted_and_private() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("key.pem");
    let a = SigningKey::load_or_create(&path).unwrap();
    let b = SigningKey::load_or_create(&path).unwrap();
    assert_eq!(a.kid, b.kid, "same key after restart");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

fn base64_url(s: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s)
}

/// Developer apps (werk) live on their own domain; each kind of app only
/// answers on its own host, and the werk marker is what decides.
#[tokio::test]
async fn werk_apps_only_on_the_werk_domain() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("apps/foo")).unwrap();
    std::fs::create_dir_all(dir.path().join("apps/tool")).unwrap();
    std::fs::write(dir.path().join("apps/tool/werk.json"), "{}").unwrap();
    // A user app shipping a marker inside its release changes nothing.
    std::fs::create_dir_all(dir.path().join("apps/foo/current")).unwrap();
    std::fs::write(dir.path().join("apps/foo/current/werk.json"), "{}").unwrap();
    let key = SigningKey::load_or_create(&dir.path().join("key.pem")).unwrap();
    let h = Harness {
        state: Arc::new(AppState {
            config: Config {
                issuer: ISSUER.into(),
                apps_domain: "apps.example.test".into(),
                apps_dir: dir.path().join("apps"),
                token_ttl_secs: 600,
                werk_domain: Some("werk.example.test".into()),
            },
            key,
            integrations: traum_haft_gateway::integrations::Integrations::new(
                Default::default(),
                Arc::new(traum_haft_gateway::store::MemoryStore::default()),
                reqwest::Client::new(),
                "https://connect.apps.example.test".into(),
            ),
            feedback: Default::default(),
        }),
        _dir: dir,
    };
    let ask = |d: &str| {
        Request::get(format!("/_internal/tls-ask?domain={d}"))
            .body(Body::empty())
            .unwrap()
    };
    for (d, ok) in [
        ("werk.example.test", true),
        ("tool.werk.example.test", true),
        ("foo.werk.example.test", false),
        ("tool.apps.example.test", false),
        ("foo.apps.example.test", true),
        ("nope.werk.example.test", false),
    ] {
        let s = call(&h, ask(d)).await.0;
        assert_eq!(s == StatusCode::OK, ok, "{d}: {s}");
    }
    let token = |app: &str, host: &str| {
        as_visitor("/_auth/token", app, "alice")
            .header("host", host)
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        call(&h, token("tool", "tool.werk.example.test")).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&h, token("foo", "foo.apps.example.test:443")).await.0,
        StatusCode::OK
    );
    for (app, host) in [
        ("tool", "tool.apps.example.test"),
        ("foo", "foo.werk.example.test"),
        ("foo", "tool.werk.example.test"),
    ] {
        assert_eq!(
            call(&h, token(app, host)).await.0,
            StatusCode::UNAUTHORIZED,
            "{app} via {host}"
        );
    }
}
