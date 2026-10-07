use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde_json::{Value, json};
use traum_haft_common::github::GitHubApp;

#[derive(Default)]
struct Seen {
    token_requests: Vec<Value>,
    issues: Vec<(String, String, Value)>,
}

fn rsa_pair() -> (String, String) {
    let dir = tempfile_dir();
    let key = format!("{dir}/k.pem");
    assert!(
        std::process::Command::new("openssl")
            .args(["genrsa", "-out", &key, "2048"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let pubk = std::process::Command::new("openssl")
        .args(["rsa", "-in", &key, "-pubout"])
        .output()
        .unwrap();
    (
        std::fs::read_to_string(&key).unwrap(),
        String::from_utf8(pubk.stdout).unwrap(),
    )
}
fn tempfile_dir() -> String {
    let d = std::env::temp_dir().join(format!("th-gh-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.display().to_string()
}

#[tokio::test]
async fn issue_is_created_with_a_token_scoped_to_one_repo() {
    let (private, public) = rsa_pair();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let decoding = Arc::new(DecodingKey::from_rsa_pem(public.as_bytes()).unwrap());
    let check_jwt = {
        let decoding = decoding.clone();
        move |h: &HeaderMap| {
            let token = h["authorization"]
                .to_str()
                .unwrap()
                .strip_prefix("Bearer ")
                .unwrap()
                .to_string();
            let mut v = Validation::new(Algorithm::RS256);
            v.set_required_spec_claims(&["exp", "iss"]);
            v.set_issuer(&["12345"]);
            jsonwebtoken::decode::<Value>(&token, &decoding, &v).is_ok()
        }
    };
    let c1 = check_jwt.clone();
    let c2 = check_jwt.clone();
    let app = Router::new()
        .route(
            "/orgs/{org}/installation",
            get(move |Path(org): Path<String>, h: HeaderMap| async move {
                assert_eq!(org, "isp-insoft-gmbh");
                assert!(c1(&h), "app JWT must be RS256, iss = app id");
                Json(json!({"id": 777}))
            }),
        )
        .route(
            "/app/installations/{id}/access_tokens",
            post(move |State(s): State<Arc<Mutex<Seen>>>, Path(id): Path<u64>, h: HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(id, 777);
                assert!(c2(&h));
                s.lock().unwrap().token_requests.push(body);
                Json(json!({"token": "ghs_scoped", "expires_at": "2099-01-01T00:00:00Z"}))
            }),
        )
        .route(
            "/repos/{org}/{repo}/issues",
            post(|State(s): State<Arc<Mutex<Seen>>>, Path((org, repo)): Path<(String, String)>, h: HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(h["authorization"], "Bearer ghs_scoped");
                s.lock().unwrap().issues.push((org, repo, body));
                Json(json!({"number": 7, "html_url": "https://github.com/isp-insoft-gmbh/app-foo/issues/7"}))
            }),
        )
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let gh = GitHubApp::new(
        &base,
        "isp-insoft-gmbh",
        "app-",
        "12345",
        &private,
        reqwest::Client::new(),
    )
    .unwrap();
    let issue = gh
        .create_issue(
            "foo",
            "Meldung: Knopf geht nicht",
            "body",
            &["visitor-report"],
        )
        .await
        .unwrap();
    assert_eq!(issue.number, 7);
    // second issue reuses the cached token
    gh.create_issue("foo", "zweite", "b", &["visitor-report"])
        .await
        .unwrap();

    let s = seen.lock().unwrap();
    assert_eq!(
        s.token_requests,
        vec![json!({"repositories": ["app-foo"]})],
        "one token, scoped to app-foo only"
    );
    assert_eq!(s.issues[0].1, "app-foo");
    assert_eq!(s.issues[0].2["labels"], json!(["visitor-report"]));
}
