//! Shared HTTP pieces: the API's error type, a JSON extractor whose
//! rejections are plain 400s, the client IP, and the two middlewares that
//! guard every response (security headers) and every state-changing API
//! request (the `Origin` check).

use crate::{log, AppState};
use axum::{
    extract::{rejection::JsonRejection, ConnectInfo, FromRequest, FromRequestParts, Request, State},
    http::{header, request::Parts, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde::de::DeserializeOwned;
use std::net::SocketAddr;
use std::time::Duration;

/// An API failure: an HTTP status with a JSON `{ "error": "..." }` body
/// meant to be shown to the user.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    retry_after: Option<Duration>,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        ApiError {
            status,
            message: message.into(),
            retry_after: None,
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "Sign in to continue.")
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    /// Also what anything the user isn't allowed to see answers, so ids
    /// can't be probed.
    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "Not found.")
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    /// 429 with a `Retry-After` header.
    pub fn too_many(retry_after: Duration, message: impl Into<String>) -> Self {
        ApiError {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: message.into(),
            retry_after: Some(retry_after),
        }
    }

    /// "Try again in ..." for a 429.
    pub fn rate_limited(retry_after: Duration) -> Self {
        let secs = retry_after.as_secs().max(1);
        let wait = if secs < 90 {
            format!("{secs} seconds")
        } else {
            format!("{} minutes", secs.div_ceil(60))
        };
        Self::too_many(retry_after, format!("Too many attempts. Try again in {wait}."))
    }
}

/// Unexpected failures: logged in full, reported to the browser vaguely.
impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        log(format!("[http] internal error: {e:#}"));
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong on the server.")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(serde_json::json!({ "error": self.message }))).into_response();
        if let Some(after) = self.retry_after {
            let secs = after.as_secs().max(1).to_string();
            if let Ok(value) = HeaderValue::from_str(&secs) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
    }
}

/// `Json<T>`, except a body that doesn't parse is a 400 with the reason,
/// not axum's default 415/422.
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(ApiJson(value)),
            Err(rejection) => Err(json_rejection(rejection)),
        }
    }
}

fn json_rejection(rejection: JsonRejection) -> ApiError {
    ApiError::bad_request(format!("Invalid request body: {}", rejection.body_text()))
}

/// The address rate limits and sessions are recorded against.
/// `CF-Connecting-IP` when `TRUST_CF_CONNECTING_IP` (only safe when the port
/// is reachable through the tunnel alone), otherwise the socket address.
#[derive(Debug, Clone)]
pub struct ClientIp(pub String);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        Ok(ClientIp(client_ip(
            &parts.headers,
            parts.extensions.get::<ConnectInfo<SocketAddr>>().map(|c| c.0),
            state.settings.http.trust_cf_connecting_ip,
        )))
    }
}

pub fn client_ip(headers: &HeaderMap, socket: Option<SocketAddr>, trust_cf: bool) -> String {
    if trust_cf {
        if let Some(ip) = headers
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty() && v.len() <= 64)
        {
            return ip.to_string();
        }
    }
    socket.map_or_else(|| "unknown".to_string(), |addr| addr.ip().to_string())
}

/// The browser's `User-Agent`, shortened, for the sessions table.
pub fn user_agent(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|ua| ua.chars().take(256).collect())
}

/// CSRF protection for the API: anything but GET, HEAD and OPTIONS must
/// carry an `Origin` header naming this site. With `SameSite=Lax` cookies
/// that's sufficient.
pub async fn require_same_origin(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return next.run(req).await;
    }
    let allowed = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|origin| state.settings.http.origin_allowed(origin));
    if !allowed {
        return ApiError::forbidden("Cross-site request refused.").into_response();
    }
    next.run(req).await
}

pub const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; style-src 'self' 'unsafe-inline'; \
     frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

/// Added to every response, API and dashboard files alike: the site is
/// directly on the internet once Cloudflare Access is gone.
pub async fn security_headers(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(header::REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    if state.settings.http.cookie_secure {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    response
}

/// API responses are per-user and live; never let anything cache them.
pub async fn no_store(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    response
        .headers_mut()
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ip_prefers_cloudflare_only_when_trusted() {
        let mut headers = HeaderMap::new();
        headers.insert("cf-connecting-ip", HeaderValue::from_static("203.0.113.9"));
        let socket: SocketAddr = "10.0.0.2:5555".parse().unwrap();
        assert_eq!(client_ip(&headers, Some(socket), true), "203.0.113.9");
        assert_eq!(client_ip(&headers, Some(socket), false), "10.0.0.2");
        assert_eq!(client_ip(&HeaderMap::new(), Some(socket), true), "10.0.0.2");
        assert_eq!(client_ip(&HeaderMap::new(), None, true), "unknown");
    }

    #[test]
    fn rate_limit_messages_round_up() {
        assert!(ApiError::rate_limited(Duration::from_secs(30))
            .message
            .contains("30 seconds"));
        assert!(ApiError::rate_limited(Duration::from_secs(601))
            .message
            .contains("11 minutes"));
    }
}
