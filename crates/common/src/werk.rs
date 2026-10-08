//! Developer apps ("werk"): the `traum-haft.toml` a developer puts into
//! their repository. Messages are German: they end up on the commit status.

use serde::{Deserialize, Serialize};

use crate::names::is_creatable_app_name;

/// Environment names the platform sets itself.
const RESERVED_ENV: &[&str] = &["PORT", "TRAUM_HAFT_APP", "TRAUM_HAFT_URL", "GITHUB_TOKEN"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub name: String,
    pub port: u16,
    pub owners: Vec<String>,
    #[serde(default = "root")]
    pub health: String,
    #[serde(default = "default_memory")]
    pub memory: String,
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub integrations: Vec<String>,
}

fn root() -> String {
    "/".into()
}
fn default_memory() -> String {
    "512m".into()
}

/// Parses and checks a manifest; `domain` is the company mail domain.
pub fn parse(text: &str, domain: &str) -> Result<Manifest, String> {
    let m: Manifest = toml::from_str(text).map_err(|e| {
        let msg = e.message().to_string();
        format!("traum-haft.toml ungültig: {msg}")
    })?;
    let bad = |what: &str| Err(format!("traum-haft.toml ungültig: {what}"));
    if !is_creatable_app_name(&m.name) {
        return bad(
            "name: Kleinbuchstaben, Ziffern, einzelne Bindestriche, höchstens 40 Zeichen, kein reservierter Name",
        );
    }
    if m.port == 0 {
        return bad("port fehlt oder ist 0");
    }
    let suffix = format!("@{domain}");
    if m.owners.is_empty()
        || m.owners.iter().any(|o| {
            !o.ends_with(&suffix)
                || o.len() == suffix.len()
                || o.chars()
                    .any(|c| c.is_whitespace() || c.is_ascii_uppercase())
        })
    {
        return bad(&format!(
            "owners: mindestens eine Adresse, alle klein geschrieben auf {suffix}"
        ));
    }
    if !m.health.starts_with('/')
        || m.health
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        || m.health.len() > 200
    {
        return bad("health: Pfad, der mit / beginnt");
    }
    if memory_bytes(&m.memory).is_none_or(|b| !(64 << 20..=4 << 30).contains(&b)) {
        return bad("memory: z. B. 512m oder 2g, zwischen 64m und 4g");
    }
    for s in &m.secrets {
        let ok = !s.is_empty()
            && s.len() <= 100
            && s.bytes()
                .next()
                .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
            && s.bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            && !RESERVED_ENV.contains(&s.as_str())
            && !s.starts_with("TRAUM_HAFT_")
            && !s.starts_with("GITHUB_");
        if !ok {
            return bad(&format!(
                "secrets: {s:?} ist kein erlaubter Name (GROSSBUCHSTABEN_UND_ZIFFERN, nicht PORT, TRAUM_HAFT_*, GITHUB_*)"
            ));
        }
    }
    if m.secrets.len() > 50 {
        return bad("secrets: höchstens 50");
    }
    for i in &m.integrations {
        let ok = i.split_once(':').is_some_and(|(p, s)| {
            !p.is_empty()
                && !s.is_empty()
                && i.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b':' || b == b'-')
        });
        if !ok {
            return bad(&format!(
                "integrations: {i:?}, erwartet z. B. \"jira:read\""
            ));
        }
    }
    Ok(m)
}

/// `512m`, `2g` → bytes.
pub fn memory_bytes(s: &str) -> Option<u64> {
    let (num, unit) = s.split_at(s.len().checked_sub(1)?);
    let n: u64 = num.parse().ok()?;
    match unit {
        "m" => n.checked_mul(1 << 20),
        "g" => n.checked_mul(1 << 30),
        _ => None,
    }
}

/// The gateway's view of a developer app: name + declared integrations,
/// same shape as a user app's app.toml.
pub fn app_toml(m: &Manifest) -> String {
    let list = m
        .integrations
        .iter()
        .map(|i| format!("{i:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("name = {:?}\nintegrations = [{list}]\n", m.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"
name = "datacloak"
port = 8080
owners = ["o.krylow@isp-insoft.de"]
secrets = ["OPENAI_API_KEY"]
integrations = ["jira:read"]
"#;

    #[test]
    fn parses_with_defaults() {
        let m = parse(OK, "isp-insoft.de").unwrap();
        assert_eq!(m.health, "/");
        assert_eq!(m.memory, "512m");
        assert_eq!(
            app_toml(&m),
            "name = \"datacloak\"\nintegrations = [\"jira:read\"]\n"
        );
        assert_eq!(
            toml::from_str::<toml::Table>(&app_toml(&m)).unwrap()["name"].as_str(),
            Some("datacloak")
        );
    }

    #[test]
    fn refuses_bad_fields() {
        for (from, to) in [
            ("name = \"datacloak\"", "name = \"Data_Cloak\""),
            ("name = \"datacloak\"", "name = \"connect\""),
            ("name = \"datacloak\"", "name = \"x-preview\""),
            ("port = 8080", "port = 0"),
            ("port = 8080", "port = 70000"),
            ("o.krylow@isp-insoft.de", "eve@gmail.com"),
            ("o.krylow@isp-insoft.de", "@isp-insoft.de"),
            ("o.krylow@isp-insoft.de", "O.Krylow@isp-insoft.de"),
            ("[\"OPENAI_API_KEY\"]", "[\"PORT\"]"),
            ("[\"OPENAI_API_KEY\"]", "[\"TRAUM_HAFT_X\"]"),
            ("[\"OPENAI_API_KEY\"]", "[\"lower\"]"),
            ("[\"jira:read\"]", "[\"jira\"]"),
            ("[\"jira:read\"]", "[\"jira:read\\nx = 1\"]"),
            ("port = 8080", "port = 8080\nprivileged = true"),
            ("port = 8080", "port = 8080\nmemory = \"64g\""),
            ("port = 8080", "port = 8080\nhealth = \"healthz\""),
        ] {
            let text = OK.replace(from, to);
            assert_ne!(text, OK, "{from}");
            assert!(parse(&text, "isp-insoft.de").is_err(), "accepted {to}");
        }
    }

    #[test]
    fn memory() {
        assert_eq!(memory_bytes("512m"), Some(512 << 20));
        assert_eq!(memory_bytes("2g"), Some(2 << 30));
        assert_eq!(memory_bytes("2"), None);
        assert_eq!(memory_bytes(""), None);
    }
}
