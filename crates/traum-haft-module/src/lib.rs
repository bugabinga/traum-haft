//! Who is calling this traum-haft app?
//!
//! SpacetimeDB accepts tokens from any issuer and does not check `aud`. The
//! edge already restricts what reaches a database; this crate is the second
//! layer inside the module: a caller counts only if the platform issued the
//! token for exactly this app.
//!
//! The expected issuer and app name are fixed at build time through
//! `TRAUM_HAFT_ISSUER` and `TRAUM_HAFT_APP` (set by the template's
//! `.cargo/config.toml` and by the platform build).
//!
//! ```ignore
//! #[spacetimedb::reducer(client_connected)]
//! pub fn on_connect(ctx: &ReducerContext) -> Result<(), String> {
//!     traum_haft_module::visitor(ctx).map(|_| ())
//! }
//! ```

use spacetimedb::{Identity, ReducerContext};

/// Issuer this module trusts.
pub const ISSUER: &str = env!("TRAUM_HAFT_ISSUER");
/// Name of this app; tokens must carry it as audience.
pub const APP: &str = env!("TRAUM_HAFT_APP");

/// A signed-in coworker, as vouched for by the platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Visitor {
    /// Stable SpacetimeDB identity; use this as the key in your tables.
    pub identity: Identity,
    /// Google account email, e.g. for display.
    pub email: String,
}

/// Returns the caller, or an error if the platform did not sign them in for
/// this app. Call it at the start of every reducer that should require login,
/// and in `client_connected` to refuse foreign connections.
pub fn visitor(ctx: &ReducerContext) -> Result<Visitor, String> {
    let jwt = ctx.sender_auth().jwt().ok_or("not signed in")?;
    let email = check_claims(jwt.raw_payload(), ISSUER, APP)?;
    Ok(Visitor {
        identity: ctx.sender(),
        email,
    })
}

/// Checks a token payload; returns the email claim. Separate from
/// [`visitor`] so it can be tested without a running database.
pub fn check_claims(payload: &str, issuer: &str, app: &str) -> Result<String, String> {
    let claims: serde_json::Value =
        serde_json::from_str(payload).map_err(|_| "unreadable token")?;
    if claims.get("iss").and_then(|v| v.as_str()) != Some(issuer) {
        return Err("token not issued by traum-haft".into());
    }
    let aud_ok = match claims.get("aud") {
        Some(serde_json::Value::String(a)) => a == app,
        Some(serde_json::Value::Array(list)) => list.iter().any(|a| a.as_str() == Some(app)),
        _ => false,
    };
    if !aud_ok {
        return Err("token issued for another app".into());
    }
    claims
        .get("email")
        .and_then(|v| v.as_str())
        .filter(|e| !e.is_empty())
        .map(String::from)
        .ok_or_else(|| "token has no email".into())
}

#[cfg(test)]
mod tests {
    use super::check_claims;

    const ISS: &str = "https://apps.example.test";

    #[test]
    fn accepts_platform_token_for_this_app() {
        let p = r#"{"iss":"https://apps.example.test","sub":"s","aud":"foo","email":"a@x"}"#;
        assert_eq!(check_claims(p, ISS, "foo"), Ok("a@x".into()));
        let p =
            r#"{"iss":"https://apps.example.test","sub":"s","aud":["bar","foo"],"email":"a@x"}"#;
        assert_eq!(check_claims(p, ISS, "foo"), Ok("a@x".into()));
    }

    #[test]
    fn refuses_foreign_issuer_other_app_and_missing_claims() {
        let other_iss = r#"{"iss":"https://evil.test","sub":"s","aud":"foo","email":"a@x"}"#;
        assert!(check_claims(other_iss, ISS, "foo").is_err());
        let other_app =
            r#"{"iss":"https://apps.example.test","sub":"s","aud":"bar","email":"a@x"}"#;
        assert!(check_claims(other_app, ISS, "foo").is_err());
        let no_aud = r#"{"iss":"https://apps.example.test","sub":"s","email":"a@x"}"#;
        assert!(check_claims(no_aud, ISS, "foo").is_err());
        let no_email = r#"{"iss":"https://apps.example.test","sub":"s","aud":"foo"}"#;
        assert!(check_claims(no_email, ISS, "foo").is_err());
        assert!(check_claims("not json", ISS, "foo").is_err());
    }
}
