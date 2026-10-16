//! The administrator's sign-in. The launcher's sign-in page (`/admin/login`) checks the name and
//! password and sets a session cookie; scripts may send the same name and password with HTTP
//! Basic instead. Everything travels over the HTTPS the reverse proxy provides.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use axum::http::HeaderMap;
use axum::http::header::{AUTHORIZATION, COOKIE};
use base64::Engine;
use sha2::{Digest, Sha256};

/// The session cookie's name.
pub const COOKIE_NAME: &str = "artcraft_admin";
/// How long a sign-in lasts.
pub const SESSION_LIFE: i64 = 14 * 24 * 3600;

/// Signed-in administrators' sessions: the SHA-256 of each cookie and when it ends, kept across
/// restarts in `sessions.json`.
pub struct Sessions {
    path: PathBuf,
    map: Mutex<HashMap<String, i64>>,
}

impl Sessions {
    pub fn open(data: &std::path::Path) -> Self {
        let path = data.join("sessions.json");
        let mut map: HashMap<String, i64> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let now = artcraft_launcher_native::now();
        map.retain(|_, ends| *ends > now);
        Self { path, map: Mutex::new(map) }
    }

    fn save(&self, map: &HashMap<String, i64>) {
        match serde_json::to_vec(map) {
            Ok(json) => {
                if let Err(e) = artcraft_launcher_native::paths::write_atomic(&self.path, &json) {
                    log::warn!("couldn't save the sign-ins ({e})");
                }
            }
            Err(e) => log::warn!("couldn't save the sign-ins ({e})"),
        }
    }

    /// A new session; returns the cookie's value.
    pub fn start(&self) -> Result<String, String> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| format!("no randomness available ({e})"))?;
        let value = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        let now = artcraft_launcher_native::now();
        map.retain(|_, ends| *ends > now);
        map.insert(hash(&value), now + SESSION_LIFE);
        self.save(&map);
        Ok(value)
    }

    pub fn end(&self, value: &str) {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        if map.remove(&hash(value)).is_some() {
            self.save(&map);
        }
    }

    /// Whether `headers` carry a live session cookie.
    pub fn signed_in(&self, headers: &HeaderMap) -> bool {
        let Some(value) = cookie(headers) else { return false };
        let map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        map.get(&hash(&value)).is_some_and(|ends| *ends > artcraft_launcher_native::now())
    }
}

/// The session cookie's value in `headers`.
pub fn cookie(headers: &HeaderMap) -> Option<String> {
    headers.get_all(COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|pair| {
        let (name, value) = pair.trim().split_once('=')?;
        (name == COOKIE_NAME && !value.is_empty() && value.len() < 100).then(|| value.to_string())
    })
}

fn hash(value: &str) -> String {
    artcraft_launcher_native::http::hex(&Sha256::digest(value.as_bytes()))
}

pub struct Admin {
    user: String,
    password: String,
}

impl Admin {
    pub fn new(user: String, password: String) -> Result<Self, String> {
        if user.contains(':') {
            return Err("ARTCRAFT_ADMIN_USER can't contain ':'".into());
        }
        if password.chars().count() < 8 {
            return Err("ARTCRAFT_ADMIN_PASSWORD must be at least 8 characters".into());
        }
        Ok(Self { user, password })
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    /// Whether `user` and `password` are this administrator's.
    pub fn allows_login(&self, user: &str, password: &str) -> bool {
        // Both compared in full, whatever the first says, so the timing tells nothing.
        let user_ok = same(user.as_bytes(), self.user.as_bytes());
        let password_ok = same(password.as_bytes(), self.password.as_bytes());
        user_ok & password_ok
    }

    /// Whether `headers` carry this administrator's name and password (HTTP Basic).
    pub fn allows(&self, headers: &HeaderMap) -> bool {
        let Some(value) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) else { return false };
        let Some((scheme, encoded)) = value.trim().split_once(' ') else { return false };
        if !scheme.eq_ignore_ascii_case("basic") || encoded.len() > 1024 {
            return false;
        }
        let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else { return false };
        let Some(colon) = decoded.iter().position(|b| *b == b':') else { return false };
        let (user, password) = (&decoded[..colon], &decoded[colon + 1..]);
        // Both compared in full, whatever the first says, so the timing tells nothing.
        let user_ok = same(user, self.user.as_bytes());
        let password_ok = same(password, self.password.as_bytes());
        user_ok & password_ok
    }
}

/// Compares in time that depends only on the lengths, not on where the bytes differ.
fn same(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, value.parse().unwrap());
        h
    }

    fn basic(user_password: &str) -> HeaderMap {
        headers(&format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(user_password)))
    }

    #[test]
    fn lets_in_only_the_right_name_and_password() {
        let admin = Admin::new("admin".into(), "correct horse".into()).unwrap();
        assert!(admin.allows(&basic("admin:correct horse")));
        assert!(admin.allows(&headers(&format!("basic  {}", base64::engine::general_purpose::STANDARD.encode("admin:correct horse")))));
        for wrong in ["admin:correct hors", "admin:correct horse!", "Admin:correct horse", "admin", ":", "admin:"] {
            assert!(!admin.allows(&basic(wrong)), "{wrong}");
        }
        assert!(!admin.allows(&headers("Bearer abc")));
        assert!(!admin.allows(&headers("Basic not-base64!")));
        assert!(!admin.allows(&HeaderMap::new()));
    }

    #[test]
    fn sessions_live_until_they_end() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = Sessions::open(dir.path());
        let value = sessions.start().unwrap();
        let mut h = HeaderMap::new();
        h.insert(COOKIE, format!("other=1; {COOKIE_NAME}={value}").parse().unwrap());
        assert!(sessions.signed_in(&h));
        // Kept across restarts, as a fingerprint only.
        assert!(Sessions::open(dir.path()).signed_in(&h));
        assert!(!std::fs::read_to_string(dir.path().join("sessions.json")).unwrap().contains(&value));
        sessions.end(&value);
        assert!(!sessions.signed_in(&h));
        let mut wrong = HeaderMap::new();
        wrong.insert(COOKIE, format!("{COOKIE_NAME}=nope").parse().unwrap());
        assert!(!sessions.signed_in(&wrong));
    }

    #[test]
    fn refuses_weak_settings() {
        assert!(Admin::new("admin".into(), "short".into()).is_err());
        assert!(Admin::new("a:b".into(), "long enough".into()).is_err());
    }

    #[test]
    fn compares_whole_strings() {
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"abcd"));
        assert!(!same(b"", b"a"));
    }
}
