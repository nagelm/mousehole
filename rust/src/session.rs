//! Sessions per docs/rust-rewrite/config-auth-boundary.md §2.
//! In-memory only (restart logs everyone out, by design). Fixed lifetime, no
//! sliding renewal. Deletion (logout, expiry, reaper) fires the SSE-close
//! callback so the client re-pulls /state, gets 401, and lands on login.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use rand::RngCore;
use subtle::ConstantTimeEq;

pub const SESSION_COOKIE_NAME: &str = "mousehole-session";

type OnDeleted = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<Inner>,
}

struct Inner {
    sessions: Mutex<HashMap<String, Instant>>, // id -> expiry
    duration: Duration,
    on_deleted: OnDeleted,
}

impl SessionStore {
    pub fn new(duration_seconds: u64, on_deleted: OnDeleted) -> Self {
        Self {
            inner: Arc::new(Inner {
                sessions: Mutex::new(HashMap::new()),
                duration: Duration::from_secs(duration_seconds),
                on_deleted,
            }),
        }
    }

    /// 32 crypto-random bytes, base64url no-pad = 43 chars.
    pub fn create(&self) -> String {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let expiry = Instant::now() + self.inner.duration;
        self.inner.sessions.lock().unwrap().insert(id.clone(), expiry);
        self.spawn_expiry_reaper(id.clone(), self.inner.duration);
        id
    }

    /// The per-session timer of the original: fires the SSE close at expiry
    /// moment even if no request ever observes the expiry.
    fn spawn_expiry_reaper(&self, id: String, after: Duration) {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let expired = {
                let mut map = inner.sessions.lock().unwrap();
                match map.get(&id) {
                    // Only delete if still present and actually due (a fresh
                    // login with a recycled id is impossible — ids are random)
                    Some(expiry) if *expiry <= Instant::now() => {
                        map.remove(&id);
                        true
                    }
                    _ => false,
                }
            };
            if expired {
                (inner.on_deleted)(&id);
            }
        });
    }

    /// Lazy sweep before lookup, per contract.
    pub fn is_valid(&self, id: &str) -> bool {
        let now = Instant::now();
        let mut expired: Vec<String> = Vec::new();
        let valid = {
            let mut map = self.inner.sessions.lock().unwrap();
            map.retain(|k, expiry| {
                if *expiry <= now {
                    expired.push(k.clone());
                    false
                } else {
                    true
                }
            });
            map.contains_key(id)
        };
        for e in &expired {
            (self.inner.on_deleted)(e);
        }
        valid
    }

    /// Logout: delete if known; fire on_deleted only for known sessions.
    pub fn delete(&self, id: &str) {
        let known = self.inner.sessions.lock().unwrap().remove(id).is_some();
        if known {
            (self.inner.on_deleted)(id);
        }
    }
}

/// safeEqual: constant-time comparison; false immediately on length mismatch.
pub fn safe_equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

/// Set-Cookie on login: value; Max-Age; Path; HttpOnly; [Secure;] SameSite=Lax
/// — Hono's serialization order, §2.1.
pub fn login_cookie(id: &str, max_age_seconds: u64, secure: bool) -> String {
    let secure_part = if secure { " Secure;" } else { "" };
    format!(
        "{SESSION_COOKIE_NAME}={id}; Max-Age={max_age_seconds}; Path=/; HttpOnly;{secure_part} SameSite=Lax"
    )
}

/// Set-Cookie on logout: empty value, Max-Age=0, Path=/ — no other attributes.
pub fn logout_cookie() -> String {
    format!("{SESSION_COOKIE_NAME}=; Max-Age=0; Path=/")
}

/// Cookie-header parse per Hono semantics: split on `;`, trim, exact name
/// match, strip one pair of surrounding double quotes, percent-decode.
pub fn extract_session_id(cookie_header: Option<&str>) -> Option<String> {
    let header = cookie_header?;
    for pair in header.split(';') {
        let pair = pair.trim();
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if name != SESSION_COOKIE_NAME {
            continue;
        }
        let mut v = value;
        if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
            v = &v[1..v.len() - 1];
        }
        return Some(
            percent_encoding::percent_decode_str(v)
                .decode_utf8()
                .map(|s| s.to_string())
                .unwrap_or_else(|_| v.to_string()),
        );
    }
    None
}

/// Whether the request presented a session cookie at all (for the 401
/// message ladder).
pub fn presented_session_cookie(cookie_header: Option<&str>) -> bool {
    extract_session_id(cookie_header).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noop() -> OnDeleted {
        Arc::new(|_| {})
    }

    #[tokio::test]
    async fn create_produces_43_char_base64url_ids() {
        let store = SessionStore::new(60, noop());
        let id = store.create();
        assert_eq!(id.len(), 43);
        assert!(id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
        assert!(store.is_valid(&id));
        assert!(!store.is_valid("nope"));
    }

    #[tokio::test]
    async fn delete_fires_callback_only_for_known_sessions() {
        let fired = Arc::new(Mutex::new(Vec::<String>::new()));
        let f2 = fired.clone();
        let store = SessionStore::new(60, Arc::new(move |id| f2.lock().unwrap().push(id.into())));
        let id = store.create();
        store.delete("unknown");
        assert!(fired.lock().unwrap().is_empty());
        store.delete(&id);
        assert_eq!(fired.lock().unwrap().as_slice(), &[id]);
    }

    #[test]
    fn safe_equal_semantics() {
        assert!(safe_equal(b"abc", b"abc"));
        assert!(!safe_equal(b"abc", b"abd"));
        assert!(!safe_equal(b"abc", b"abcd"));
        assert!(safe_equal(b"", b""));
    }

    #[test]
    fn cookie_builders_match_contract() {
        assert_eq!(
            login_cookie("SID", 604800, false),
            "mousehole-session=SID; Max-Age=604800; Path=/; HttpOnly; SameSite=Lax"
        );
        assert_eq!(
            login_cookie("SID", 604800, true),
            "mousehole-session=SID; Max-Age=604800; Path=/; HttpOnly; Secure; SameSite=Lax"
        );
        assert_eq!(logout_cookie(), "mousehole-session=; Max-Age=0; Path=/");
    }

    #[test]
    fn cookie_extraction() {
        assert_eq!(
            extract_session_id(Some("a=b; mousehole-session=XYZ; c=d")),
            Some("XYZ".into())
        );
        assert_eq!(
            extract_session_id(Some("mousehole-session=\"Q%20T\"")),
            Some("Q T".into())
        );
        assert_eq!(extract_session_id(Some("other=1")), None);
        assert_eq!(extract_session_id(None), None);
    }
}
