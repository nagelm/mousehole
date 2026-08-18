//! Env var parsing. Built exactly once at startup; any validation failure
//! throws before the listener binds — the process refuses to start rather
//! than run half-configured. `Config::from_env` is a pure function of an env
//! map plus an injected file reader (the `_FILE` seam), so tests stay
//! hermetic. The error strings are user-facing and documented — don't reword
//! them. (Full matrix: docs/rust-rewrite/config-auth-boundary.md §1.)

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::logger::{self, LogLevel};

/// Secret wrapper with a redacting Debug so credentials cannot leak via logs.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    #[cfg(test)]
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

#[derive(Debug, Clone)]
pub enum AuthConfig {
    /// Invariant: at least one of password/token is Some.
    Configured {
        password: Option<Secret>,
        token: Option<Secret>,
    },
    None {
        insecure_allow_no_auth: bool,
    },
}

impl AuthConfig {
    pub fn is_configured(&self) -> bool {
        matches!(self, AuthConfig::Configured { .. })
    }
    pub fn password(&self) -> Option<&Secret> {
        match self {
            AuthConfig::Configured { password, .. } => password.as_ref(),
            AuthConfig::None { .. } => None,
        }
    }
    pub fn token(&self) -> Option<&Secret> {
        match self {
            AuthConfig::Configured { token, .. } => token.as_ref(),
            AuthConfig::None { .. } => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum AllowedHosts {
    All,
    Allowlist(Vec<String>),
}

#[derive(Debug, Clone)]
pub enum AllowedOrigins {
    SameOrigin,
    All,
    Allowlist(Vec<String>),
}

#[derive(Debug, Clone)]
pub struct Config {
    pub log_level: LogLevel,
    pub state_dir_path: PathBuf,
    pub update_interval_seconds: f64,
    pub mam_request_timeout_seconds: f64,
    pub session_duration_seconds: u64,
    pub port: u16,
    pub https_only_cookies: bool,
    pub auth: AuthConfig,
    pub allowed_hosts: AllowedHosts,
    pub allowed_origins: AllowedOrigins,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

fn invalid(name: &str, raw: &str, message: &str) -> ConfigError {
    ConfigError(format!(
        "Invalid environment variable {name}=\"{raw}\": {message}"
    ))
}

/// Trimmed read; empty or whitespace-only is identical to unset.
fn get_env<'a>(env: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    env.get(name).map(|v| v.trim()).filter(|v| !v.is_empty())
}

fn parse_bool_flag(env: &HashMap<String, String>, name: &str) -> Result<bool, ConfigError> {
    match get_env(env, name) {
        None => Ok(false),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(raw) => Err(invalid(name, raw, "must be \"true\" or \"false\"")),
    }
}

// Validation messages below are zod's exact wording — the originals are
// user-facing and the parity harness compares them byte for byte.

fn parse_positive_number(
    env: &HashMap<String, String>,
    name: &str,
    default: f64,
) -> Result<f64, ConfigError> {
    match get_env(env, name) {
        None => Ok(default),
        Some(raw) => {
            let n: f64 = raw
                .parse()
                .map_err(|_| invalid(name, raw, "Invalid input: expected number, received NaN"))?;
            if n.is_finite() && n > 0.0 {
                Ok(n)
            } else {
                Err(invalid(name, raw, "Too small: expected number to be >0"))
            }
        }
    }
}

fn parse_positive_int(
    env: &HashMap<String, String>,
    name: &str,
    default: u64,
) -> Result<u64, ConfigError> {
    match get_env(env, name) {
        None => Ok(default),
        Some(raw) => {
            // zod coerces with Number(...) then requires an integer > 0, so
            // "12.0" passes; mirror that.
            let n: f64 = raw
                .parse()
                .map_err(|_| invalid(name, raw, "Invalid input: expected number, received NaN"))?;
            if !(n.is_finite() && n <= u64::MAX as f64) {
                Err(invalid(name, raw, "Invalid input: expected number, received NaN"))
            } else if n.fract() != 0.0 {
                Err(invalid(name, raw, "Invalid input: expected int, received number"))
            } else if n <= 0.0 {
                Err(invalid(name, raw, "Too small: expected number to be >0"))
            } else {
                Ok(n as u64)
            }
        }
    }
}

fn parse_port(env: &HashMap<String, String>, name: &str, default: u16) -> Result<u16, ConfigError> {
    match get_env(env, name) {
        None => Ok(default),
        Some(raw) => {
            let n: f64 = raw
                .parse()
                .map_err(|_| invalid(name, raw, "Invalid input: expected number, received NaN"))?;
            if !n.is_finite() {
                Err(invalid(name, raw, "Invalid input: expected number, received NaN"))
            } else if n.fract() != 0.0 {
                Err(invalid(name, raw, "Invalid input: expected int, received number"))
            } else if n < 1.0 {
                Err(invalid(name, raw, "Too small: expected number to be >=1"))
            } else if n > 65535.0 {
                Err(invalid(name, raw, "Too big: expected number to be <=65535"))
            } else {
                Ok(n as u16)
            }
        }
    }
}

/// The `_FILE` secret variant per §1.3: `_FILE` set ⇒ plain var ignored
/// entirely; unreadable file is fatal; empty (trimmed) file ⇒ no credential.
fn resolve_secret(
    env: &HashMap<String, String>,
    name: &str,
    read_file: &dyn Fn(&Path) -> io::Result<String>,
) -> Result<Option<Secret>, ConfigError> {
    let file_var = format!("{name}_FILE");
    match get_env(env, &file_var) {
        None => Ok(get_env(env, name).map(|v| Secret(v.to_string()))),
        Some(path) => {
            let contents = read_file(Path::new(path)).map_err(|e| {
                ConfigError(format!(
                    "Invalid environment variable {file_var}=\"{path}\": could not read file ({e})"
                ))
            })?;
            let trimmed = contents.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(Secret(trimmed.to_string())))
            }
        }
    }
}

fn resolve_auth_config(
    password: Option<Secret>,
    token: Option<Secret>,
    insecure_allow_no_auth: bool,
) -> Result<AuthConfig, ConfigError> {
    if insecure_allow_no_auth && (password.is_some() || token.is_some()) {
        let credential = if password.is_some() {
            "MOUSEHOLE_AUTH_PASSWORD"
        } else {
            "MOUSEHOLE_AUTH_TOKEN"
        };
        return Err(ConfigError(format!(
            "MOUSEHOLE_INSECURE_ALLOW_NO_AUTH=true cannot be combined with {credential}: \
turning off authentication and configuring a credential are mutually exclusive. Unset one of them."
        )));
    }
    Ok(if password.is_some() {
        AuthConfig::Configured { password, token }
    } else if token.is_some() {
        AuthConfig::Configured {
            password: None,
            token,
        }
    } else {
        AuthConfig::None {
            insecure_allow_no_auth,
        }
    })
}

fn parse_csv_or_all(raw: &str) -> Option<Vec<String>> {
    if raw == "*" {
        return None; // caller maps to All
    }
    Some(
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

impl Config {
    pub fn from_env(
        env: &HashMap<String, String>,
        read_file: &dyn Fn(&Path) -> io::Result<String>,
    ) -> Result<Self, ConfigError> {
        let log_level = match get_env(env, "MOUSEHOLE_LOG_LEVEL") {
            None => LogLevel::Info,
            Some(raw) => LogLevel::parse(raw).ok_or_else(|| {
                invalid(
                    "MOUSEHOLE_LOG_LEVEL",
                    raw,
                    "must be one of debug, info, warn, error",
                )
            })?,
        };

        let state_dir_path = PathBuf::from(
            get_env(env, "MOUSEHOLE_STATE_DIR_PATH").unwrap_or("/var/lib/mousehole"),
        );

        let update_interval_seconds =
            parse_positive_number(env, "MOUSEHOLE_UPDATE_INTERVAL_SECONDS", 300.0)?;
        let mam_request_timeout_seconds =
            parse_positive_number(env, "MOUSEHOLE_MAM_REQUEST_TIMEOUT_SECONDS", 10.0)?;
        let session_duration_seconds =
            parse_positive_int(env, "MOUSEHOLE_SESSION_DURATION_SECONDS", 604_800)?;
        let port = parse_port(env, "MOUSEHOLE_PORT", 5010)?;
        let https_only_cookies = parse_bool_flag(env, "MOUSEHOLE_HTTPS_ONLY_COOKIES")?;
        let insecure_allow_no_auth = parse_bool_flag(env, "MOUSEHOLE_INSECURE_ALLOW_NO_AUTH")?;

        let password = resolve_secret(env, "MOUSEHOLE_AUTH_PASSWORD", read_file)?;
        let token = resolve_secret(env, "MOUSEHOLE_AUTH_TOKEN", read_file)?;
        let auth = resolve_auth_config(password, token, insecure_allow_no_auth)?;

        let allowed_hosts = match get_env(env, "MOUSEHOLE_ALLOWED_HOSTS") {
            None => AllowedHosts::Allowlist(vec![
                "localhost".into(),
                "127.0.0.1".into(),
                "[::1]".into(),
            ]),
            Some(raw) => match parse_csv_or_all(raw) {
                None => AllowedHosts::All,
                Some(hosts) if hosts.is_empty() => {
                    return Err(ConfigError(
                        "Invalid environment variable MOUSEHOLE_ALLOWED_HOSTS: \
must not be empty; use * to allow all hosts"
                            .into(),
                    ))
                }
                Some(hosts) => AllowedHosts::Allowlist(hosts),
            },
        };

        let allowed_origins = match get_env(env, "MOUSEHOLE_ALLOWED_ORIGINS") {
            None => AllowedOrigins::SameOrigin,
            Some(raw) => match parse_csv_or_all(raw) {
                None => AllowedOrigins::All,
                Some(origins) if origins.is_empty() => {
                    return Err(ConfigError(
                        "Invalid environment variable MOUSEHOLE_ALLOWED_ORIGINS: \
must not be empty; use * to allow all origins"
                            .into(),
                    ))
                }
                Some(origins) => AllowedOrigins::Allowlist(origins),
            },
        };

        Ok(Config {
            log_level,
            state_dir_path,
            update_interval_seconds,
            mam_request_timeout_seconds,
            session_duration_seconds,
            port,
            https_only_cookies,
            auth,
            allowed_hosts,
            allowed_origins,
        })
    }
}

/// §1.6 — runs after bind + banner. Warns for permissive settings; fatal when
/// auth is unconfigured without the explicit opt-out.
pub fn validate_runtime_security_config(config: &Config) -> Result<(), ConfigError> {
    if matches!(config.allowed_hosts, AllowedHosts::All) {
        logger::warn(
            "MOUSEHOLE_ALLOWED_HOSTS allows any Host header. This is less secure and almost \
always avoidable. Set it to your specific host(s) or IP(s).",
        );
    }
    if matches!(config.allowed_origins, AllowedOrigins::All) {
        logger::warn(
            "MOUSEHOLE_ALLOWED_ORIGINS allows any Origin Header for cross-origin requests. \
This is less secure and almost always avoidable. Set it to your specific allowed origins.",
        );
    }
    match &config.auth {
        AuthConfig::Configured {
            password: None,
            token: Some(_),
        } => logger::warn(
            "Browser login will be unavailable. Set MOUSEHOLE_AUTH_PASSWORD to enable it.",
        ),
        AuthConfig::None {
            insecure_allow_no_auth: false,
        } => {
            return Err(ConfigError(
                "Mousehole authentication is not configured. Set MOUSEHOLE_AUTH_PASSWORD \
and/or MOUSEHOLE_AUTH_TOKEN, or set MOUSEHOLE_INSECURE_ALLOW_NO_AUTH=true to opt out."
                    .into(),
            ))
        }
        AuthConfig::None {
            insecure_allow_no_auth: true,
        } => logger::warn(
            "Running without authentication (MOUSEHOLE_INSECURE_ALLOW_NO_AUTH=true). \
Do not expose Mousehole to mixed-trust LAN, VPN, or public interfaces.",
        ),
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn no_files(_: &Path) -> io::Result<String> {
        Err(io::Error::new(io::ErrorKind::NotFound, "no such file"))
    }

    #[test]
    fn defaults() {
        let c = Config::from_env(&env(&[("MOUSEHOLE_AUTH_PASSWORD", "pw")]), &no_files).unwrap();
        assert_eq!(c.port, 5010);
        assert_eq!(c.session_duration_seconds, 604_800);
        assert_eq!(c.update_interval_seconds, 300.0);
        assert!(matches!(c.allowed_origins, AllowedOrigins::SameOrigin));
        match &c.allowed_hosts {
            AllowedHosts::Allowlist(h) => assert_eq!(h, &["localhost", "127.0.0.1", "[::1]"]),
            _ => panic!(),
        }
    }

    #[test]
    fn empty_env_var_is_unset() {
        let c = Config::from_env(
            &env(&[("MOUSEHOLE_PORT", "   "), ("MOUSEHOLE_AUTH_TOKEN", "t")]),
            &no_files,
        )
        .unwrap();
        assert_eq!(c.port, 5010);
    }

    #[test]
    fn floats_allowed_where_speced() {
        let c = Config::from_env(
            &env(&[
                ("MOUSEHOLE_UPDATE_INTERVAL_SECONDS", "0.5"),
                ("MOUSEHOLE_AUTH_PASSWORD", "pw"),
            ]),
            &no_files,
        )
        .unwrap();
        assert_eq!(c.update_interval_seconds, 0.5);
        assert!(Config::from_env(
            &env(&[
                ("MOUSEHOLE_SESSION_DURATION_SECONDS", "0.5"),
                ("MOUSEHOLE_AUTH_PASSWORD", "pw"),
            ]),
            &no_files,
        )
        .is_err());
    }

    #[test]
    fn invalid_value_error_frame() {
        let e = Config::from_env(
            &env(&[("MOUSEHOLE_PORT", "eleventy"), ("MOUSEHOLE_AUTH_TOKEN", "t")]),
            &no_files,
        )
        .unwrap_err();
        assert_eq!(
            e.0,
            "Invalid environment variable MOUSEHOLE_PORT=\"eleventy\": Invalid input: expected number, received NaN"
        );
    }

    #[test]
    fn file_variant_precedence_and_empty_file() {
        let read = |p: &Path| -> io::Result<String> {
            match p.to_str().unwrap() {
                "/s/full" => Ok("  filepw  \n".into()),
                "/s/empty" => Ok("   \n".into()),
                _ => Err(io::Error::new(io::ErrorKind::NotFound, "no such file")),
            }
        };
        let c = Config::from_env(
            &env(&[
                ("MOUSEHOLE_AUTH_PASSWORD", "plain"),
                ("MOUSEHOLE_AUTH_PASSWORD_FILE", "/s/full"),
            ]),
            &read,
        )
        .unwrap();
        assert_eq!(c.auth.password().unwrap().as_str(), "filepw");
        let c = Config::from_env(
            &env(&[
                ("MOUSEHOLE_AUTH_PASSWORD", "plain"),
                ("MOUSEHOLE_AUTH_PASSWORD_FILE", "/s/empty"),
            ]),
            &read,
        )
        .unwrap();
        assert!(!c.auth.is_configured());
        let e = Config::from_env(
            &env(&[("MOUSEHOLE_AUTH_PASSWORD_FILE", "/s/missing")]),
            &read,
        )
        .unwrap_err();
        assert!(e.0.starts_with(
            "Invalid environment variable MOUSEHOLE_AUTH_PASSWORD_FILE=\"/s/missing\": could not read file ("
        ));
    }

    #[test]
    fn mutual_exclusion() {
        let e = Config::from_env(
            &env(&[
                ("MOUSEHOLE_INSECURE_ALLOW_NO_AUTH", "true"),
                ("MOUSEHOLE_AUTH_PASSWORD", "pw"),
                ("MOUSEHOLE_AUTH_TOKEN", "t"),
            ]),
            &no_files,
        )
        .unwrap_err();
        assert!(e.0.contains("MOUSEHOLE_AUTH_PASSWORD"));
        assert!(e
            .0
            .starts_with("MOUSEHOLE_INSECURE_ALLOW_NO_AUTH=true cannot be combined"));
    }

    #[test]
    fn hosts_origins_star_and_empty() {
        let c = Config::from_env(
            &env(&[
                ("MOUSEHOLE_ALLOWED_HOSTS", "*"),
                ("MOUSEHOLE_ALLOWED_ORIGINS", " a.example ,, b.example "),
                ("MOUSEHOLE_AUTH_PASSWORD", "pw"),
            ]),
            &no_files,
        )
        .unwrap();
        assert!(matches!(c.allowed_hosts, AllowedHosts::All));
        match &c.allowed_origins {
            AllowedOrigins::Allowlist(o) => assert_eq!(o, &["a.example", "b.example"]),
            _ => panic!(),
        }
        let e = Config::from_env(
            &env(&[("MOUSEHOLE_ALLOWED_HOSTS", ",,"), ("MOUSEHOLE_AUTH_TOKEN", "t")]),
            &no_files,
        )
        .unwrap_err();
        assert_eq!(
            e.0,
            "Invalid environment variable MOUSEHOLE_ALLOWED_HOSTS: must not be empty; use * to allow all hosts"
        );
    }
}
