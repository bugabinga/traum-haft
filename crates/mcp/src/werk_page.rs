//! `https://werk.<domain>/logs/<app>`: logs for people without an agent.
//! Behind the werk login; the edge passes the visitor's identity and its
//! shared secret, so requests that bypass the edge are refused.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::AppState;

fn page(status: StatusCode, title: &str, body: &str) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
            (header::HeaderName::from_static("content-security-policy"), "default-src 'none'; style-src 'unsafe-inline'"),
        ],
        format!(
            "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width'><title>{t}</title>\
<style>body{{font:15px system-ui;margin:2rem auto;max-width:72rem;padding:0 1rem}}pre{{background:#111;color:#ddd;padding:1rem;overflow:auto;font-size:13px;white-space:pre-wrap}}</style>\
<h1>{t}</h1>{body}",
            t = html_escape::encode_text(title)
        ),
    )
        .into_response()
}

pub async fn logs(
    State(s): State<Arc<AppState>>,
    Path(app): Path<String>,
    headers: HeaderMap,
) -> Response {
    let header = |n: &str| {
        headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    };
    let edge_ok = s
        .edge_secret
        .as_deref()
        .is_some_and(|e| !e.is_empty() && header("x-traum-haft-edge") == e);
    let email = header("x-user-email");
    if !edge_ok || email.is_empty() {
        return page(
            StatusCode::FORBIDDEN,
            "Kein Zugriff",
            "<p>Nur über werk mit Anmeldung.</p>",
        );
    }
    let Some(w) = s.werk.as_ref() else {
        return page(StatusCode::NOT_FOUND, "Unbekannt", "");
    };
    let agent = s.platform_agents.iter().any(|a| a == email);
    let Some(a) = w
        .app(&app)
        .await
        .filter(|a| crate::werk::may_read(a, email, agent))
    else {
        // Same answer for "no such app" and "not yours".
        return page(
            StatusCode::NOT_FOUND,
            "Keine App mit diesem Namen",
            "<p>Oder du stehst nicht in <code>owners</code>.</p>",
        );
    };
    let events: String = a
        .events
        .iter()
        .rev()
        .map(|e| format!("{}  {}\n", fmt_time(e.at), e.text))
        .collect();
    let logs = w
        .container_logs(&app, 500)
        .await
        .unwrap_or_else(|e| format!("({e})"));
    let body = format!(
        "<p>{state}: {msg} · <a href='{url}'>{url}</a> · Repository <code>{repo}</code></p>\
<h2>Plattform</h2><pre>{events}</pre><h2>App (stdout/stderr, neueste unten)</h2><pre>{logs}</pre>",
        state = html_escape::encode_text(&a.state),
        msg = html_escape::encode_text(&a.message),
        url = html_escape::encode_double_quoted_attribute(&w.app_url(&app)),
        repo = html_escape::encode_text(&a.repo),
        events = html_escape::encode_text(&events),
        logs = html_escape::encode_text(&logs),
    );
    page(StatusCode::OK, &format!("{app} – Logs"), &body)
}

fn fmt_time(secs: u64) -> String {
    // UTC, without a date library.
    let (d, s) = (secs / 86_400, secs % 86_400);
    let (y, m, day) = civil(d as i64);
    format!(
        "{y:04}-{m:02}-{day:02} {:02}:{:02} UTC",
        s / 3600,
        s % 3600 / 60
    )
}

/// Days since 1970-01-01 → (year, month, day); Howard Hinnant's algorithm.
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

const GUIDE: &str = include_str!("../../../docs/werk.md");
const LLMS: &str = include_str!("../../../docs/llms-dev.txt");

/// `https://werk.<domain>/` (the developer guide) and `/llms-dev.txt`;
/// public, no secrets in either.
pub async fn docs(path: Option<Path<String>>) -> Response {
    match path.as_ref().map(|p| p.0.as_str()).unwrap_or("") {
        "" | "index.html" => {
            let mut html = String::new();
            let opts = pulldown_cmark::Options::ENABLE_TABLES;
            pulldown_cmark::html::push_html(
                &mut html,
                pulldown_cmark::Parser::new_ext(GUIDE, opts),
            );
            (
                [
                    (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                    (header::HeaderName::from_static("content-security-policy"), "default-src 'none'; style-src 'unsafe-inline'"),
                ],
                format!(
                    "<!doctype html><html lang=de><meta charset=utf-8><meta name=viewport content='width=device-width'>\
<title>traum-haft werk</title><style>body{{font:16px/1.5 system-ui;margin:2rem auto;max-width:48rem;padding:0 1rem}}\
pre{{background:#f4f4f4;padding:.8rem;overflow:auto}}code{{font-size:.92em}}table{{border-collapse:collapse}}\
td,th{{border:1px solid #ccc;padding:.3rem .5rem;text-align:left}}\
@media (prefers-color-scheme:dark){{body{{background:#161616;color:#ddd}}pre{{background:#222}}a{{color:#8ab4f8}}}}</style>{html}"
                ),
            )
                .into_response()
        }
        "llms-dev.txt" => {
            ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], LLMS).into_response()
        }
        _ => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn time() {
        assert_eq!(super::fmt_time(0), "1970-01-01 00:00 UTC");
        assert_eq!(super::fmt_time(951_782_400), "2000-02-29 00:00 UTC");
        assert_eq!(super::fmt_time(1_791_500_000), "2026-10-08 22:53 UTC");
    }
}
