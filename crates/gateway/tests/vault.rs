//! Run through tests/vault/run.sh (needs a Vault dev server).

use serde_json::json;
use traum_haft_gateway::store::{SecretStore, VaultStore};

fn env(n: &str) -> String {
    std::env::var(n).unwrap_or_else(|_| panic!("{n} not set; run tests/vault/run.sh"))
}

#[tokio::test]
#[ignore]
async fn vault_store_round_trip_with_approle() {
    let http = reqwest::Client::new();
    let store = VaultStore::new(
        &env("TH_VAULT_ADDR"),
        "kv",
        "traum-haft",
        &env("TH_VAULT_ROLE_ID"),
        &env("TH_VAULT_SECRET_ID"),
        http.clone(),
    );
    let key = "users/google-alice/connections/atlassian";
    assert_eq!(
        store.get(key).await.unwrap(),
        None,
        "missing key reads as None"
    );
    let creds = json!({"access_token": "at", "refresh_token": "rt-1", "expires_at": 1});
    store.put(key, &creds).await.unwrap();
    assert_eq!(store.get(key).await.unwrap(), Some(creds));
    store
        .put(key, &json!({"refresh_token": "rt-2"}))
        .await
        .unwrap();
    assert_eq!(
        store.get(key).await.unwrap().unwrap()["refresh_token"],
        "rt-2",
        "rotation overwrites"
    );
    store.delete(key).await.unwrap();
    assert_eq!(store.get(key).await.unwrap(), None, "deleted");

    // The policy confines the gateway to kv/traum-haft/*.
    let outside = VaultStore::new(
        &env("TH_VAULT_ADDR"),
        "kv",
        "other",
        &env("TH_VAULT_ROLE_ID"),
        &env("TH_VAULT_SECRET_ID"),
        http,
    );
    assert!(
        outside.get("secret").await.is_err(),
        "reading outside the prefix must be denied"
    );
    assert!(
        outside.put("secret", &json!({"x": 1})).await.is_err(),
        "writing outside the prefix must be denied"
    );
    println!("vault store: round trip, rotation, delete and policy confinement ok");
}
