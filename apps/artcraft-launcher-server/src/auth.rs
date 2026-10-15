//! The administrator's sign-in: HTTP Basic over the HTTPS the reverse proxy provides, so the
//! browser shows its own password prompt and remembers it for `/admin/`, and the launcher's
//! interface needs no sign-in screen of its own.

use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use base64::Engine;

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

    /// Whether `headers` carry this administrator's name and password.
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
