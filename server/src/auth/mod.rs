//! Built-in authentication: users, password hashing, sessions in an
//! HttpOnly cookie, and the one-time setup token for the first admin.
//!
//! Every `/api` route needs a session unless it's on the allow-list in
//! `routes.rs`; the middleware there calls [`Auth::authenticate`] and puts a
//! [`SessionCtx`] in the request, which handlers read through the
//! [`CurrentUser`], [`CurrentSession`] and [`AdminUser`] extractors.

pub mod handlers;
pub mod password;
pub mod store;

use crate::config::Settings;
use crate::db::{is_past, parse_ts, Db};
use crate::http::ApiError;
use crate::ratelimit::RateLimiter;
use anyhow::Result;
use axum::{
    extract::FromRequestParts,
    http::{header, request::Parts, HeaderMap, HeaderValue},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use password::Hasher;
use rand::Rng as _;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// How long a validated session is trusted from memory before the database
/// is asked again. The dashboard polls every second; this keeps that to a
/// read every few seconds, and changes made by another process (the admin
/// CLI) take effect within it. Changes made through the API clear the
/// cache at once.
const SESSION_CACHE_TTL: Duration = Duration::from_secs(5);

/// Usernames nobody can sign up with. The first admin, at setup, can.
pub const RESERVED_USERNAMES: &[&str] = &["admin", "administrator", "root", "system", "delegation"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    User,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::User => "user",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "admin" => Some(Role::Admin),
            "user" => Some(Role::User),
            _ => None,
        }
    }
}

/// A signed-in user, as `GET /api/me` returns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub role: Role,
}

impl User {
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }
}

/// What the session middleware attaches to an authenticated request.
#[derive(Debug, Clone)]
pub struct SessionCtx {
    pub token_hash: String,
    pub user: User,
}

struct CachedSession {
    user: User,
    checked: Instant,
    expires_at: SystemTime,
    last_seen_at: SystemTime,
}

pub struct Auth {
    pub hasher: Arc<Hasher>,
    sessions: Mutex<HashMap<String, CachedSession>>,
    /// Failed logins, keyed `user:<name>|<ip>` and `ip:<ip>`. Bad setup
    /// tokens and wrong current passwords count too.
    pub login_failures: RateLimiter,
    pub signups: RateLimiter,
    pub enrolls: RateLimiter,
    /// Only while no admin exists; never written anywhere but the log.
    setup_token: Mutex<Option<String>>,
}

impl Auth {
    pub fn new(settings: &Settings) -> Result<Self> {
        let a = &settings.auth;
        Ok(Auth {
            hasher: Hasher::new(a.argon2)?,
            sessions: Mutex::new(HashMap::new()),
            login_failures: RateLimiter::new(a.login_window),
            signups: RateLimiter::new(a.signup_window),
            enrolls: RateLimiter::new(a.login_window),
            setup_token: Mutex::new(None),
        })
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<String, CachedSession>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Generates and returns a new setup token (startup, no admin yet).
    pub fn issue_setup_token(&self) -> String {
        let token = random_token(18);
        *self.setup_token.lock().unwrap_or_else(|e| e.into_inner()) = Some(token.clone());
        token
    }

    /// Whether `candidate` is the current setup token, in constant time.
    pub fn setup_token_matches(&self, candidate: &str) -> bool {
        let guard = self.setup_token.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .as_deref()
            .is_some_and(|token| constant_time_eq(token.as_bytes(), candidate.trim().as_bytes()))
    }

    pub fn has_setup_token(&self) -> bool {
        self.setup_token.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    pub fn clear_setup_token(&self) {
        *self.setup_token.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// The session for a cookie value, if it's valid: it exists, hasn't
    /// expired or been idle too long, and its user isn't disabled. Writes
    /// `last_seen_at` at most every `SESSION_TOUCH_INTERVAL_SECS`.
    pub async fn authenticate(&self, db: &Db, settings: &Settings, token: &str) -> Result<Option<SessionCtx>> {
        let token_hash = sha256_hex(token);
        let idle = settings.auth.session_idle_timeout;
        let now = SystemTime::now();
        {
            let mut cache = self.cache();
            if let Some(cached) = cache.get(&token_hash) {
                let fresh = cached.checked.elapsed() < SESSION_CACHE_TTL;
                let valid = now < cached.expires_at && now < cached.last_seen_at + idle;
                if fresh && valid {
                    return Ok(Some(SessionCtx {
                        token_hash,
                        user: cached.user.clone(),
                    }));
                }
                cache.remove(&token_hash);
            }
        }

        let touch_every = settings.auth.session_touch_interval;
        let hash = token_hash.clone();
        let row = db
            .call(move |c| {
                let Some(row) = store::load_session(c, &hash)? else {
                    return Ok(None);
                };
                let last_seen = parse_ts(&row.last_seen_at).unwrap_or(SystemTime::UNIX_EPOCH);
                let expired = is_past(&row.expires_at, now) || last_seen + idle <= now;
                if expired || row.disabled {
                    store::delete_session(c, &hash)?;
                    return Ok(None);
                }
                let last_seen = if now.duration_since(last_seen).unwrap_or_default() >= touch_every {
                    store::touch_session(c, &hash, now)?;
                    now
                } else {
                    last_seen
                };
                let expires_at = parse_ts(&row.expires_at).unwrap_or(now);
                Ok(Some((row.user, expires_at, last_seen)))
            })
            .await?;
        let Some((user, expires_at, last_seen_at)) = row else {
            return Ok(None);
        };
        let mut cache = self.cache();
        if cache.len() > 10_000 {
            cache.clear();
        }
        cache.insert(
            token_hash.clone(),
            CachedSession {
                user: user.clone(),
                checked: Instant::now(),
                expires_at,
                last_seen_at,
            },
        );
        Ok(Some(SessionCtx { token_hash, user }))
    }

    /// Drops every cached session of `user_id`, so their next request goes
    /// to the database (after a password change, disable or delete).
    pub fn forget_user(&self, user_id: &str) {
        self.cache().retain(|_, s| s.user.id != user_id);
    }

    pub fn forget_session(&self, token_hash: &str) {
        self.cache().remove(token_hash);
    }

    /// For the cleanup task.
    pub fn prune(&self) {
        self.cache().retain(|_, s| s.checked.elapsed() < SESSION_CACHE_TTL);
        self.login_failures.prune();
        self.signups.prune();
        self.enrolls.prune();
    }
}

/// A new session's cookie value and the hash that's stored.
pub fn new_session_token() -> (String, String) {
    let token = random_token(32);
    let hash = sha256_hex(&token);
    (token, hash)
}

/// `n` random bytes, base64url without padding.
pub fn random_token(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn sha256_hex(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------------- cookies ----------------

/// `__Host-` stops subdomains from setting it, but needs `Secure`.
pub fn cookie_name(secure: bool) -> &'static str {
    if secure {
        "__Host-delegation_session"
    } else {
        "delegation_session"
    }
}

/// `max_age` None makes a browser-session cookie.
pub fn session_cookie(settings: &Settings, value: &str, max_age: Option<Duration>) -> HeaderValue {
    let secure = settings.http.cookie_secure;
    let mut cookie = format!("{}={value}; Path=/; HttpOnly; SameSite=Lax", cookie_name(secure));
    if secure {
        cookie.push_str("; Secure");
    }
    if let Some(age) = max_age {
        cookie.push_str(&format!("; Max-Age={}", age.as_secs()));
    }
    HeaderValue::from_str(&cookie).expect("cookie values are base64url")
}

pub fn clear_cookie(settings: &Settings) -> HeaderValue {
    session_cookie(settings, "", Some(Duration::ZERO))
}

/// This site's session cookie from the request, if it sent one.
pub fn read_session_cookie(headers: &HeaderMap, settings: &Settings) -> Option<String> {
    let name = cookie_name(settings.http.cookie_secure);
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, v)| *k == name && !v.is_empty())
        .map(|(_, v)| v.to_string())
}

// ---------------- validation ----------------

/// 3 to 32 letters, digits, `.`, `_` and `-`. Compared case-insensitively.
pub fn validate_username(username: &str, allow_reserved: bool) -> Result<(), ApiError> {
    let len = username.chars().count();
    if !(3..=32).contains(&len) {
        return Err(ApiError::bad_request("Usernames are 3 to 32 characters long."));
    }
    if !username
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(ApiError::bad_request(
            "Usernames can only contain letters, digits, '.', '_' and '-'.",
        ));
    }
    if !allow_reserved && RESERVED_USERNAMES.iter().any(|r| r.eq_ignore_ascii_case(username)) {
        return Err(ApiError::bad_request("That username is reserved. Choose another."));
    }
    Ok(())
}

pub fn validate_password(settings: &Settings, password: &str) -> Result<(), ApiError> {
    let len = password.chars().count();
    let (min, max) = (settings.auth.password_min_len, settings.auth.password_max_len);
    if len < min {
        return Err(ApiError::bad_request(format!(
            "Passwords must be at least {min} characters long."
        )));
    }
    if len > max {
        return Err(ApiError::bad_request(format!(
            "Passwords can be at most {max} characters long."
        )));
    }
    Ok(())
}

// ---------------- extractors ----------------

/// The signed-in user. Only on routes behind the session middleware.
pub struct CurrentUser(pub User);

/// The signed-in user plus which session this request used.
pub struct CurrentSession(pub SessionCtx);

/// The signed-in user, who must be the admin (403 otherwise).
pub struct AdminUser(pub User);

fn session_ctx(parts: &Parts) -> Result<SessionCtx, ApiError> {
    parts
        .extensions
        .get::<SessionCtx>()
        .cloned()
        .ok_or_else(ApiError::unauthorized)
}

impl<S: Send + Sync> FromRequestParts<S> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(CurrentUser(session_ctx(parts)?.user))
    }
}

impl<S: Send + Sync> FromRequestParts<S> for CurrentSession {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(CurrentSession(session_ctx(parts)?))
    }
}

impl<S: Send + Sync> FromRequestParts<S> for AdminUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let user = session_ctx(parts)?.user;
        if !user.is_admin() {
            return Err(ApiError::forbidden("Only the site admin can do that."));
        }
        Ok(AdminUser(user))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames_follow_the_rules() {
        assert!(validate_username("alice", false).is_ok());
        assert!(validate_username("a.b_c-9", false).is_ok());
        assert!(validate_username("ab", false).is_err());
        assert!(validate_username(&"a".repeat(33), false).is_err());
        assert!(validate_username("has space", false).is_err());
        assert!(validate_username("émile", false).is_err());
        assert!(validate_username("Admin", false).is_err());
        assert!(validate_username("ROOT", false).is_err());
        assert!(validate_username("admin", true).is_ok(), "the first admin may take it");
    }

    #[test]
    fn cookies_parse_and_tokens_hash() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("theme=dark; delegation_session=abc; other=1"),
        );
        let settings = crate::config::Settings::parse(|k| (k == "COOKIE_SECURE").then(|| "false".into())).unwrap();
        assert_eq!(read_session_cookie(&headers, &settings).as_deref(), Some("abc"));
        let secure = crate::config::Settings::parse(|_| None).unwrap();
        assert_eq!(read_session_cookie(&headers, &secure), None, "the secure name differs");

        let (token, hash) = new_session_token();
        assert_eq!(token.len(), 43, "32 bytes of base64url");
        assert_eq!(hash, sha256_hex(&token));
        assert_eq!(hash.len(), 64);
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
