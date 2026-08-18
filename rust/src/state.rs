//! The state file: one JSON document, the single source of truth (sessions,
//! SSE registrations, and the contact timer are memory-only and die with the
//! process, by design). Struct fields are declared in on-disk key order and
//! absent optionals are omitted — the output is byte-identical to what the
//! Bun backend writes. Only ENOENT means "fresh install"; any other read
//! problem is a hard error, because treating an unreadable file as "no state"
//! would let the next contact write a cookieless state over the real one.
//! (Full schema/migration details: docs/rust-rewrite/state-and-build.md §1.)

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::error::{self, AppError, Issue};
use crate::timefmt;

pub const STATE_VERSION: u64 = 2;

// ---- on-disk / wire shapes (field order is contract) ----

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IpUpdate {
    pub success: bool,
    pub msg: String,
    #[serde(rename = "httpStatus")]
    pub http_status: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContactError {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

/// Discriminated union on `reached`. Serde `untagged` picks by required
/// fields (`error` vs `ip`/`asn`/`as`); the `reached` literal is verified
/// after parse in `validate_contact`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum SerializedMamContact {
    Unreached {
        at: String,
        reached: bool, // always false; validated post-parse
        error: ContactError,
    },
    Reached {
        at: String,
        reached: bool, // always true; validated post-parse
        ip: String,
        asn: i64,
        #[serde(rename = "as")]
        as_name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[serde(rename = "ipUpdate")]
        ip_update: Option<IpUpdate>,
    },
}

impl SerializedMamContact {
    pub fn at(&self) -> &str {
        match self {
            Self::Unreached { at, .. } | Self::Reached { at, .. } => at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SerializedState {
    pub version: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cookie: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "lastMamContact")]
    pub last_mam_contact: Option<SerializedMamContact>,
}

// ---- in-memory state (no version field; `at` parsed to Zoned) ----

#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub cookie: Option<String>,
    pub last_mam_contact: Option<MamContact>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MamContact {
    pub at: jiff::Zoned,
    pub body: SerializedMamContact, // canonical serialized form (with `at` string)
}

impl State {
    pub fn fresh() -> Self {
        Self {
            cookie: None,
            last_mam_contact: None,
        }
    }

    /// Empty-string cookies are treated as absent everywhere they're consumed.
    pub fn effective_cookie(&self) -> Option<&str> {
        self.cookie.as_deref().filter(|c| !c.is_empty())
    }
}

// ---- classification (api-contract.md §2.4): status code only, never msg ----

pub fn classify(contact: Option<&SerializedMamContact>) -> &'static str {
    match contact {
        None => "pending",
        Some(SerializedMamContact::Unreached { .. }) => "unreachable",
        Some(SerializedMamContact::Reached { ip_update: None, .. }) => "no-cookie",
        Some(SerializedMamContact::Reached {
            ip_update: Some(u), ..
        }) => match u.http_status {
            200 => "ok",
            429 => "throttled",
            _ => "rejected",
        },
    }
}

// ---- PublicState (api-contract.md §2.1) ----

#[derive(Debug, Clone, Serialize)]
pub struct PublicState {
    #[serde(rename = "hasCookie")]
    pub has_cookie: bool,
    #[serde(rename = "hasAuth")]
    pub has_auth: bool,
    #[serde(rename = "nextContactAt")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_contact_at: Option<String>,
    #[serde(rename = "lastMamContact")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_mam_contact: Option<SerializedMamContact>,
}

// ---- store ----

#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
    state_path: PathBuf,
}

impl Store {
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            state_path: dir.join("state.json"),
        }
    }

    fn path_str(&self) -> String {
        self.state_path.display().to_string()
    }

    /// §1.5 read path: ENOENT ⇒ None; other IO ⇒ file-read-error; bad JSON ⇒
    /// json-parse-error(file); wrong shape ⇒ schema-error(500, source = path);
    /// unparseable `at` ⇒ unhandled-error. Corruption is never a fresh state.
    pub fn read_if_exists(&self) -> Result<Option<State>, AppError> {
        let contents = match std::fs::read_to_string(&self.state_path) {
            Ok(c) => c,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(error::file_read_error(&self.path_str())),
        };
        let json: serde_json::Value = serde_json::from_str(&contents)
            .map_err(|_| error::json_parse_error_file(&self.path_str()))?;
        let serialized = migrate_to_current(json, &self.path_str())?;
        Ok(Some(deserialize_state(serialized)?))
    }

    /// Write to a temporary file first and then rename it to ensure atomic
    /// writes. This is a millisecond-level race condition protection against
    /// broken writes; unlikely, but why not be safe? (Errors name the final
    /// path, not the tmp file — that's what the user needs to go look at.)
    pub fn write(&self, state: &SerializedState) -> Result<(), AppError> {
        let body = serde_json::to_string_pretty(state)
            .map_err(|e| error::unhandled(format!("state serialization failed: {e}")))?;
        std::fs::create_dir_all(&self.dir)
            .map_err(|_| error::directory_create_error(&self.dir.display().to_string()))?;
        let tmp = self.state_path.with_extension("json.tmp");
        std::fs::write(&tmp, body)
            .map_err(|_| error::file_write_error(&self.path_str()))?;
        std::fs::rename(&tmp, &self.state_path)
            .map_err(|_| error::file_write_error(&self.path_str()))?;
        Ok(())
    }
}

/// §1.6: version==2 ⇒ strict parse; anything else ⇒ rescue only a non-empty
/// string `currentCookie` and drop the rest.
fn migrate_to_current(
    json: serde_json::Value,
    source_name: &str,
) -> Result<SerializedState, AppError> {
    let is_current = json
        .as_object()
        .and_then(|o| o.get("version"))
        .and_then(|v| v.as_u64())
        == Some(STATE_VERSION);

    let candidate = if is_current {
        json
    } else {
        let cookie = json
            .as_object()
            .and_then(|o| o.get("currentCookie"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let mut obj = serde_json::Map::new();
        obj.insert("version".into(), serde_json::json!(STATE_VERSION));
        if let Some(c) = cookie {
            obj.insert("cookie".into(), serde_json::Value::String(c));
        }
        serde_json::Value::Object(obj)
    };

    let parsed: SerializedState = serde_json::from_value(candidate).map_err(|e| {
        error::schema_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            source_name,
            vec![Issue {
                path: String::new(),
                message: e.to_string(),
            }],
        )
    })?;
    validate_contact(&parsed, source_name)?;
    Ok(parsed)
}

/// The `reached` literal check that serde's untagged parse cannot express.
fn validate_contact(state: &SerializedState, source_name: &str) -> Result<(), AppError> {
    let ok = match &state.last_mam_contact {
        None => true,
        Some(SerializedMamContact::Unreached { reached, .. }) => !reached,
        Some(SerializedMamContact::Reached { reached, .. }) => *reached,
    };
    if ok {
        Ok(())
    } else {
        Err(error::schema_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            source_name,
            vec![Issue {
                path: "lastMamContact.reached".into(),
                message: "reached flag does not match contact variant".into(),
            }],
        ))
    }
}

/// §1.4: `at` parsed after schema validation; failure is an unhandled-error
/// (500), never a fresh state.
fn deserialize_state(s: SerializedState) -> Result<State, AppError> {
    let last = match s.last_mam_contact {
        None => None,
        Some(body) => {
            let at = timefmt::parse_wire(body.at())
                .map_err(|e| error::unhandled(format!("invalid stored timestamp: {e}")))?;
            Some(MamContact { at, body })
        }
    };
    Ok(State {
        cookie: s.cookie,
        last_mam_contact: last,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mousehole-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    const FULL_EXAMPLE: &str = r#"{
  "version": 2,
  "cookie": "long-opaque-mam-session-cookie-value",
  "lastMamContact": {
    "at": "2026-08-18T02:00:05.123+00:00[UTC]",
    "reached": true,
    "ip": "203.0.113.7",
    "asn": 64496,
    "as": "EXAMPLE-AS",
    "ipUpdate": {
      "success": true,
      "msg": "Completed",
      "httpStatus": 200
    }
  }
}"#;

    #[test]
    fn byte_identical_roundtrip_of_spec_example() {
        let parsed: SerializedState = serde_json::from_str(FULL_EXAMPLE).unwrap();
        let out = serde_json::to_string_pretty(&parsed).unwrap();
        assert_eq!(out, FULL_EXAMPLE);
    }

    #[test]
    fn optionals_are_omitted_not_null() {
        let s = SerializedState {
            version: 2,
            cookie: None,
            last_mam_contact: Some(SerializedMamContact::Reached {
                at: "2026-08-18T02:00:05.123+00:00[UTC]".into(),
                reached: true,
                ip: "203.0.113.7".into(),
                asn: 64496,
                as_name: "EXAMPLE-AS".into(),
                ip_update: None,
            }),
        };
        let out = serde_json::to_string_pretty(&s).unwrap();
        assert!(!out.contains("cookie"));
        assert!(!out.contains("ipUpdate"));
        assert!(!out.contains("null"));
    }

    #[test]
    fn store_roundtrip_and_fresh_install() {
        let dir = temp_dir("store");
        let store = Store::new(&dir);
        assert!(store.read_if_exists().unwrap().is_none());
        let s: SerializedState = serde_json::from_str(FULL_EXAMPLE).unwrap();
        store.write(&s).unwrap();
        let back = store.read_if_exists().unwrap().unwrap();
        assert_eq!(
            back.cookie.as_deref(),
            Some("long-opaque-mam-session-cookie-value")
        );
        assert_eq!(
            classify(back.last_mam_contact.as_ref().map(|c| &c.body)),
            "ok"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("state.json")).unwrap(),
            FULL_EXAMPLE
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v1_migration_rescues_only_nonempty_current_cookie() {
        let migrated = migrate_to_current(
            serde_json::json!({"currentCookie": "abc", "lastContact": {"junk": true}}),
            "src",
        )
        .unwrap();
        assert_eq!(migrated.cookie.as_deref(), Some("abc"));
        assert!(migrated.last_mam_contact.is_none());

        let migrated =
            migrate_to_current(serde_json::json!({"currentCookie": ""}), "src").unwrap();
        assert!(migrated.cookie.is_none());

        let migrated = migrate_to_current(serde_json::json!([1, 2, 3]), "src").unwrap();
        assert!(migrated.cookie.is_none());
        assert_eq!(migrated.version, 2);
    }

    #[test]
    fn corrupt_json_and_bad_schema_are_errors_not_fresh() {
        let dir = temp_dir("corrupt");
        let store = Store::new(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("state.json"), "{ not json").unwrap();
        assert_eq!(
            store.read_if_exists().unwrap_err().body.error_type,
            "json-parse-error"
        );
        std::fs::write(
            dir.join("state.json"),
            r#"{"version":2,"lastMamContact":{"at":1}}"#,
        )
        .unwrap();
        assert_eq!(
            store.read_if_exists().unwrap_err().body.error_type,
            "schema-error"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn classify_table() {
        assert_eq!(classify(None), "pending");
        let unreached = SerializedMamContact::Unreached {
            at: "2026-08-18T02:00:05+00:00[UTC]".into(),
            reached: false,
            error: ContactError {
                error_type: "network-error".into(),
                message: "x".into(),
            },
        };
        assert_eq!(classify(Some(&unreached)), "unreachable");
        let mk = |status: u16| SerializedMamContact::Reached {
            at: "2026-08-18T02:00:05+00:00[UTC]".into(),
            reached: true,
            ip: "1.2.3.4".into(),
            asn: 1,
            as_name: "A".into(),
            ip_update: Some(IpUpdate {
                success: status == 200,
                msg: "m".into(),
                http_status: status,
            }),
        };
        assert_eq!(classify(Some(&mk(200))), "ok");
        assert_eq!(classify(Some(&mk(429))), "throttled");
        assert_eq!(classify(Some(&mk(403))), "rejected");
        let no_update = SerializedMamContact::Reached {
            at: "2026-08-18T02:00:05+00:00[UTC]".into(),
            reached: true,
            ip: "1.2.3.4".into(),
            asn: 1,
            as_name: "A".into(),
            ip_update: None,
        };
        assert_eq!(classify(Some(&no_update)), "no-cookie");
    }

    #[test]
    fn empty_cookie_parses_but_is_effectively_absent() {
        let s: SerializedState = serde_json::from_str(r#"{"version":2,"cookie":""}"#).unwrap();
        let state = deserialize_state(s).unwrap();
        assert_eq!(state.cookie.as_deref(), Some(""));
        assert!(state.effective_cookie().is_none());
    }
}
