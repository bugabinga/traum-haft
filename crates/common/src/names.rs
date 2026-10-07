//! App names: one DNS label under the apps domain, shared by gateway and MCP.

/// Platform hosts under the apps domain.
pub const PLATFORM_HOSTS: &[&str] = &["connect", "mcp"];

/// Subdomains that can never be app names.
pub const RESERVED_NAMES: &[&str] = &[
    "connect", "mcp", "www", "api", "admin", "platform", "static",
];

/// Suffix of an app's preview deployment, e.g. `notes--preview`.
pub const PREVIEW_SUFFIX: &str = "--preview";

/// Lowercase letters, digits, single hyphens inside, at most 40 characters,
/// not reserved. Preview names (`<app>--preview`) are valid too.
pub fn is_valid_app_name(name: &str) -> bool {
    let len_ok = (1..=50).contains(&name.len());
    let chars_ok = name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    let edges_ok = !name.starts_with('-') && !name.ends_with('-');
    len_ok && chars_ok && edges_ok && !RESERVED_NAMES.contains(&name)
}

/// Names a builder may create: like above, but no `--` (kept for previews).
pub fn is_creatable_app_name(name: &str) -> bool {
    name.len() <= 40 && is_valid_app_name(name) && !name.contains("--")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        for ok in ["notes", "jira-report", "a1", "notes--preview"] {
            assert!(is_valid_app_name(ok), "{ok}");
        }
        for bad in [
            "",
            "Notes",
            "-x",
            "x-",
            "a_b",
            "a.b",
            "connect",
            "mcp",
            &"x".repeat(51),
        ] {
            assert!(!is_valid_app_name(bad), "{bad}");
        }
        assert!(is_creatable_app_name("jira-report"));
        assert!(!is_creatable_app_name("notes--preview"));
    }
}
