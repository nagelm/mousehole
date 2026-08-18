//! The HTTP boundary: host allowlist (DNS-rebinding defense), the auth
//! ladder, origin check (CSRF defense), and the JSON content-type gate —
//! pure functions over header values, wired up per-route in api.rs. Failure
//! precedence is simply the order a route lists its checks: host → auth →
//! origin → json; the global body limit runs before all of them.
//! (Exact semantics: docs/rust-rewrite/config-auth-boundary.md §4.)

use crate::config::{AllowedHosts, AllowedOrigins, AuthConfig, Config};
use crate::error::{self, AppError};
use crate::session::{self, SessionStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    None,
    Session,
    Token,
}

/// §4.2 parseHostAndPort: trim, lowercase, parse as `http://value`; invalid
/// when parse fails, host is empty, a path or query was smuggled in. The
/// `http://` prefix makes URL parsing strip an explicit `:80`.
fn parse_host_and_port(value: &str) -> Option<(String, Option<u16>)> {
    let candidate = value.trim().to_lowercase();
    let url = url::Url::parse(&format!("http://{candidate}")).ok()?;
    let host = url.host_str()?.to_string();
    if host.is_empty() || url.path() != "/" || url.query().is_some() {
        return None;
    }
    Some((host, url.port()))
}

fn host_matches_rule(host: &(String, Option<u16>), rule: &str) -> bool {
    let Some((rule_host, rule_port)) = parse_host_and_port(rule) else {
        return false; // unparseable rule silently matches nothing
    };
    if host.0 != rule_host {
        return false;
    }
    match rule_port {
        None => true,                    // port-agnostic rule
        Some(p) => host.1 == Some(p),    // rule with port requires exact match
    }
}

/// §4.2 host check. `raw_host` is the Host header (or authority fallback).
pub fn host_allowed(raw_host: Option<&str>, config: &Config) -> Result<(), AppError> {
    if matches!(config.allowed_hosts, AllowedHosts::All) {
        return Ok(());
    }
    let raw = raw_host.unwrap_or("");
    if raw.is_empty() {
        return Err(error::host_not_allowed("Request Host header is required."));
    }
    let Some(parsed) = parse_host_and_port(raw) else {
        return Err(error::host_not_allowed(format!(
            "Request Host \"{raw}\" is invalid."
        )));
    };
    let AllowedHosts::Allowlist(rules) = &config.allowed_hosts else {
        return Ok(());
    };
    if rules.iter().any(|r| host_matches_rule(&parsed, r)) {
        Ok(())
    } else {
        Err(error::host_not_allowed(format!(
            "Host \"{raw}\" not allowed. (Add to MOUSEHOLE_ALLOWED_HOSTS to permit it.)"
        )))
    }
}

/// §2.2 Bearer parse: `/^Bearer\s+(.+)$/i` — case-insensitive scheme, 1+
/// whitespace, capture to end of line UNtrimmed (trailing spaces must
/// byte-match).
fn parse_bearer(header: &str) -> Option<&str> {
    let rest = header
        .get(..6)
        .filter(|p| p.eq_ignore_ascii_case("Bearer"))
        .map(|_| &header[6..])?;
    let token = rest.trim_start_matches(|c: char| c.is_whitespace());
    if token.len() == rest.len() || token.is_empty() {
        return None; // no whitespace separator, or nothing after it
    }
    Some(token)
}

/// §4.3 auth ladder: none-mode → session → token → 401 (message by what was
/// presented). Defensive 500 for unconfigured auth without the opt-out.
pub fn require_auth(
    config: &Config,
    sessions: &SessionStore,
    authorization: Option<&str>,
    cookie_header: Option<&str>,
) -> Result<AuthMethod, AppError> {
    match &config.auth {
        AuthConfig::None {
            insecure_allow_no_auth: true,
        } => return Ok(AuthMethod::None),
        AuthConfig::None {
            insecure_allow_no_auth: false,
        } => return Err(error::auth_not_configured()),
        AuthConfig::Configured { .. } => {}
    }

    // Session first (deliberate: a session-authorized request still gets the
    // origin/CSRF check; only a Token result waives it).
    if let Some(id) = session::extract_session_id(cookie_header) {
        if sessions.is_valid(&id) {
            return Ok(AuthMethod::Session);
        }
    }
    if let (Some(auth_header), Some(expected)) = (authorization, config.auth.token()) {
        if let Some(presented) = parse_bearer(auth_header) {
            if session::safe_equal(presented.as_bytes(), expected.as_bytes()) {
                return Ok(AuthMethod::Token);
            }
        }
    }

    let message = if authorization.is_some() {
        "Rejected Bearer token (wrong value, or MOUSEHOLE_AUTH_TOKEN not set)"
    } else if session::presented_session_cookie(cookie_header) {
        "Unknown or expired session cookie"
    } else {
        "No credentials presented"
    };
    Err(error::authentication_required(message))
}

/// §4.4 normalizeOrigin: URL origin serialization (lowercase, default ports
/// stripped, opaque → "null"); unparseable values pass through raw so the
/// literal `Origin: null` can be allowlisted.
fn normalize_origin(origin: &str) -> String {
    match url::Url::parse(origin) {
        Ok(u) => u.origin().ascii_serialization(),
        Err(_) => origin.to_string(),
    }
}

/// §4.4 origin check (CSRF). Skipped entirely for token-authenticated
/// requests. `raw_host` feeds the same-origin synthesis (`http://` + Host —
/// forwarded headers deliberately ignored).
pub fn origin_allowed(
    auth_method: AuthMethod,
    origin_header: Option<&str>,
    raw_host: Option<&str>,
    config: &Config,
) -> Result<(), AppError> {
    if auth_method == AuthMethod::Token {
        return Ok(());
    }
    if matches!(config.allowed_origins, AllowedOrigins::All) {
        return Ok(());
    }
    let Some(origin) = origin_header else {
        return Ok(()); // origin-less requests pass
    };
    let request_origin = normalize_origin(origin);
    let allowed = match &config.allowed_origins {
        AllowedOrigins::All => true,
        AllowedOrigins::SameOrigin => {
            let own = normalize_origin(&format!("http://{}", raw_host.unwrap_or("")));
            request_origin == own
        }
        AllowedOrigins::Allowlist(list) => {
            list.iter().any(|o| normalize_origin(o) == request_origin)
        }
    };
    if allowed {
        Ok(())
    } else {
        Err(error::origin_not_allowed(&request_origin))
    }
}

/// §4.5 content-type check: parameterless, lowercased match.
pub fn require_json_body(content_type: Option<&str>) -> Result<(), AppError> {
    let raw = content_type.unwrap_or("");
    let essence = raw.split(';').next().unwrap_or("").trim().to_lowercase();
    if essence == "application/json" {
        Ok(())
    } else {
        Err(error::unsupported_media_type(raw))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn cfg(pairs: &[(&str, &str)]) -> Config {
        let env: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_env(&env, &|_| {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"))
        })
        .unwrap()
    }

    fn sessions() -> SessionStore {
        SessionStore::new(60, Arc::new(|_| {}))
    }

    #[test]
    fn default_hosts_match_any_port() {
        let c = cfg(&[("MOUSEHOLE_AUTH_TOKEN", "t")]);
        assert!(host_allowed(Some("localhost:9999"), &c).is_ok());
        assert!(host_allowed(Some("127.0.0.1"), &c).is_ok());
        assert!(host_allowed(Some("[::1]:5010"), &c).is_ok());
        let e = host_allowed(Some("evil.example"), &c).unwrap_err();
        assert_eq!(
            e.body.message,
            "Host \"evil.example\" not allowed. (Add to MOUSEHOLE_ALLOWED_HOSTS to permit it.)"
        );
    }

    #[test]
    fn rule_with_port_80_is_port_agnostic_but_8080_is_exact() {
        let c = cfg(&[
            ("MOUSEHOLE_ALLOWED_HOSTS", "myhost:8080,porty:80"),
            ("MOUSEHOLE_AUTH_TOKEN", "t"),
        ]);
        assert!(host_allowed(Some("myhost:8080"), &c).is_ok());
        assert!(host_allowed(Some("myhost"), &c).is_err());
        assert!(host_allowed(Some("myhost:80"), &c).is_err());
        // :80 stripped from the rule ⇒ any port allowed
        assert!(host_allowed(Some("porty:1234"), &c).is_ok());
        assert!(host_allowed(Some("PORTY"), &c).is_ok()); // case-insensitive
    }

    #[test]
    fn invalid_and_missing_hosts() {
        let c = cfg(&[("MOUSEHOLE_AUTH_TOKEN", "t")]);
        assert_eq!(
            host_allowed(None, &c).unwrap_err().body.message,
            "Request Host header is required."
        );
        let e = host_allowed(Some("bad/path"), &c).unwrap_err();
        assert_eq!(e.body.message, "Request Host \"bad/path\" is invalid.");
    }

    #[test]
    fn bearer_parse_semantics() {
        assert_eq!(parse_bearer("Bearer abc"), Some("abc"));
        assert_eq!(parse_bearer("bearer abc"), Some("abc"));
        assert_eq!(parse_bearer("BEARER   a b c"), Some("a b c"));
        assert_eq!(parse_bearer("Bearer abc "), Some("abc ")); // untrimmed
        assert_eq!(parse_bearer("Bearerabc"), None);
        assert_eq!(parse_bearer("Basic abc"), None);
        assert_eq!(parse_bearer("Bearer "), None);
    }

    #[test]
    fn auth_ladder_messages() {
        let c = cfg(&[("MOUSEHOLE_AUTH_TOKEN", "sekrit")]);
        let s = sessions();
        // token works, lowercase scheme too
        assert_eq!(
            require_auth(&c, &s, Some("bearer sekrit"), None).unwrap(),
            AuthMethod::Token
        );
        // wrong token / non-Bearer header
        let e = require_auth(&c, &s, Some("Basic x"), None).unwrap_err();
        assert_eq!(
            e.body.message,
            "Rejected Bearer token (wrong value, or MOUSEHOLE_AUTH_TOKEN not set)"
        );
        // stale cookie
        let e = require_auth(&c, &s, None, Some("mousehole-session=stale")).unwrap_err();
        assert_eq!(e.body.message, "Unknown or expired session cookie");
        // nothing presented
        let e = require_auth(&c, &s, None, None).unwrap_err();
        assert_eq!(e.body.message, "No credentials presented");
        assert_eq!(e.status, axum::http::StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn no_auth_modes() {
        let c = cfg(&[("MOUSEHOLE_INSECURE_ALLOW_NO_AUTH", "true")]);
        assert_eq!(
            require_auth(&c, &sessions(), None, None).unwrap(),
            AuthMethod::None
        );
        let mut c2 = cfg(&[("MOUSEHOLE_AUTH_TOKEN", "t")]);
        c2.auth = crate::config::AuthConfig::None {
            insecure_allow_no_auth: false,
        };
        let e = require_auth(&c2, &sessions(), None, None).unwrap_err();
        assert_eq!(e.body.error_type, "auth-not-configured");
        assert_eq!(e.status, axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn origin_check_semantics() {
        let c = cfg(&[("MOUSEHOLE_AUTH_PASSWORD", "pw")]); // same-origin mode
        // token skips entirely
        assert!(origin_allowed(
            AuthMethod::Token,
            Some("https://evil.example"),
            Some("localhost:5010"),
            &c
        )
        .is_ok());
        // no header passes
        assert!(origin_allowed(AuthMethod::Session, None, Some("localhost:5010"), &c).is_ok());
        // same-origin passes (default ports normalize away)
        assert!(origin_allowed(
            AuthMethod::Session,
            Some("HTTP://LOCALHOST:80"),
            Some("localhost:80"),
            &c
        )
        .is_ok());
        // cross-origin rejected with the normalized origin in the message
        let e = origin_allowed(
            AuthMethod::Session,
            Some("https://Evil.example:443"),
            Some("localhost:5010"),
            &c,
        )
        .unwrap_err();
        assert_eq!(
            e.body.message,
            "Origin \"https://evil.example\" is not allowed. (Add to MOUSEHOLE_ALLOWED_ORIGINS to permit it.)"
        );
        // Origin: null — rejected in same-origin mode, allowlistable
        assert!(origin_allowed(AuthMethod::Session, Some("null"), Some("h"), &c).is_err());
        let c2 = cfg(&[
            ("MOUSEHOLE_ALLOWED_ORIGINS", "null"),
            ("MOUSEHOLE_AUTH_PASSWORD", "pw"),
        ]);
        assert!(origin_allowed(AuthMethod::Session, Some("null"), Some("h"), &c2).is_ok());
    }

    #[test]
    fn json_content_type() {
        assert!(require_json_body(Some("application/json")).is_ok());
        assert!(require_json_body(Some("Application/JSON; charset=utf-8")).is_ok());
        let e = require_json_body(Some("text/plain")).unwrap_err();
        assert_eq!(
            e.body.message,
            "Unsupported content type \"text/plain\", must be \"application/json\""
        );
        let e = require_json_body(None).unwrap_err();
        assert_eq!(
            e.body.message,
            "Unsupported content type \"\", must be \"application/json\""
        );
    }
}
