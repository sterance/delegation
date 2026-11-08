//! HTTP-level tests: the whole router, an in-memory database, and fake
//! clients registered straight into `Live`.

use crate::auth::Auth;
use crate::config::Settings;
use crate::conversations::MessageStatus;
use crate::live::{ConnHandle, Live};
use crate::models::catalog::Catalog;
use crate::models::commands::ModelCommands;
use crate::replies::{ChatEvent, ReplyRunner};
use crate::{db, routes, AppState};
use axum::{
    body::Body,
    http::{header, HeaderMap, Method, Request, StatusCode},
    response::Response,
    Router,
};
use protocol::{
    Backend, ChatRole, ChatTurn, CommandState, Inventory, LocalModel, ModelCommandKind, ModelState, Progress,
    ProgressPhase, ServerMessage, Stage, Storage,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tower::ServiceExt;

const ORIGIN: &str = "http://test.local";
const PASSWORD: &str = "correct horse battery";

fn settings(extra: &[(&str, &str)]) -> Settings {
    let mut map: HashMap<String, String> = [
        ("PUBLIC_ORIGIN", ORIGIN),
        ("COOKIE_SECURE", "false"),
        ("TRUST_CF_CONNECTING_IP", "true"),
        ("AUTH_ARGON2_MEMORY_KIB", "64"),
        ("AUTH_ARGON2_ITERATIONS", "1"),
        ("CHAT_FLUSH_INTERVAL_MS", "50"),
        ("FRONTEND_DIR", "/nonexistent-frontend"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    for (k, v) in extra {
        map.insert(k.to_string(), v.to_string());
    }
    Settings::parse(|k| map.get(k).cloned()).expect("valid test settings")
}

struct Resp {
    status: StatusCode,
    headers: HeaderMap,
    json: Value,
}

impl Resp {
    fn error(&self) -> &str {
        self.json["error"].as_str().unwrap_or("")
    }
}

struct TestApp {
    state: AppState,
    app: Router,
    setup_token: String,
}

async fn test_app(extra: &[(&str, &str)]) -> TestApp {
    let settings = settings(extra);
    let db = db::open(":memory:", 3).unwrap();
    let live = Arc::new(Live::new(settings.metrics));
    let auth = Arc::new(Auth::new(&settings).unwrap());
    let settings = Arc::new(settings);
    let replies = ReplyRunner::new(db.clone(), live.clone(), settings.chat, settings.client.inference);
    let state = AppState {
        db,
        live,
        catalog: Arc::new(Catalog::from_settings(&settings)),
        settings,
        auth,
        replies,
        model_commands: Arc::new(ModelCommands::default()),
    };
    let setup_token = state.auth.issue_setup_token();
    TestApp {
        app: routes::app(state.clone()),
        state,
        setup_token,
    }
}

/// The `name=value` part of a response's Set-Cookie.
fn cookie_of(resp: &Resp) -> String {
    resp.headers
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .expect("a Set-Cookie header")
        .to_string()
}

fn set_cookie(resp: &Resp) -> String {
    resp.headers
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

struct Call<'a> {
    method: Method,
    path: &'a str,
    cookie: Option<&'a str>,
    body: Option<Value>,
    origin: Option<&'a str>,
    ip: &'a str,
}

impl TestApp {
    fn call<'a>(&self, method: Method, path: &'a str) -> Call<'a> {
        Call {
            method,
            path,
            cookie: None,
            body: None,
            origin: Some(ORIGIN),
            ip: "198.51.100.1",
        }
    }

    async fn raw(&self, c: Call<'_>) -> Response {
        let mut req = Request::builder()
            .method(c.method)
            .uri(c.path)
            .header("cf-connecting-ip", c.ip);
        if let Some(origin) = c.origin {
            req = req.header(header::ORIGIN, origin);
        }
        if let Some(cookie) = c.cookie {
            req = req.header(header::COOKIE, cookie);
        }
        let req = match c.body {
            Some(body) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        self.app.clone().oneshot(req).await.unwrap()
    }

    async fn send(&self, c: Call<'_>) -> Resp {
        let response = self.raw(c).await;
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Resp { status, headers, json }
    }

    async fn get(&self, path: &str, cookie: &str) -> Resp {
        self.send(Call {
            cookie: Some(cookie),
            ..self.call(Method::GET, path)
        })
        .await
    }

    async fn post(&self, path: &str, cookie: Option<&str>, body: Value) -> Resp {
        self.send(Call {
            cookie,
            body: Some(body),
            ..self.call(Method::POST, path)
        })
        .await
    }

    async fn patch(&self, path: &str, cookie: &str, body: Value) -> Resp {
        self.send(Call {
            cookie: Some(cookie),
            body: Some(body),
            ..self.call(Method::PATCH, path)
        })
        .await
    }

    async fn delete(&self, path: &str, cookie: &str) -> Resp {
        self.send(Call {
            cookie: Some(cookie),
            ..self.call(Method::DELETE, path)
        })
        .await
    }

    /// Creates the admin ("admin") and returns its session cookie.
    async fn setup_admin(&self) -> String {
        let r = self
            .post(
                "/api/auth/setup",
                None,
                json!({ "token": self.setup_token, "username": "admin", "password": PASSWORD }),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json);
        cookie_of(&r)
    }

    async fn signup(&self, username: &str) -> String {
        // A different address per name, so the per-IP sign-up limit
        // doesn't get in the way.
        let ip = format!("203.0.113.{}", username.bytes().map(usize::from).sum::<usize>() % 250);
        let r = self
            .send(Call {
                body: Some(json!({ "username": username, "password": PASSWORD })),
                ip: &ip,
                ..self.call(Method::POST, "/api/auth/signup")
            })
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json);
        cookie_of(&r)
    }

    async fn login(&self, username: &str, password: &str, remember: bool) -> Resp {
        self.post(
            "/api/auth/login",
            None,
            json!({ "username": username, "password": password, "remember": remember }),
        )
        .await
    }

    async fn me(&self, cookie: &str) -> Value {
        let r = self.get("/api/me", cookie).await;
        assert_eq!(r.status, StatusCode::OK);
        r.json
    }

    /// Mints a code as `cookie`'s user and enrolls a client with it.
    async fn enroll(&self, cookie: &str, hostname: &str) -> String {
        let code = self.post("/api/pairing-codes", Some(cookie), json!({})).await.json["code"]
            .as_str()
            .unwrap()
            .to_string();
        let r = self
            .send(Call {
                body: Some(json!({ "pairing_code": code, "public_key": "pk", "hostname": hostname })),
                origin: None,
                ..self.call(Method::POST, "/enroll")
            })
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        r.json["client_id"].as_str().unwrap().to_string()
    }

    /// Connects a fake client offering `echo`.
    fn connect(&self, client_id: &str) -> ConnHandle {
        self.connect_with(client_id, vec![model("echo", Backend::Echo, ModelState::Loaded)])
    }

    /// Connects a fake client reporting these models.
    fn connect_with(&self, client_id: &str, models: Vec<LocalModel>) -> ConnHandle {
        let conn = self.state.live.register(client_id);
        self.state.live.set_inventory(
            client_id,
            conn.id,
            Inventory {
                models,
                storage: Storage {
                    models_bytes: 1_000,
                    free_bytes: 50_000,
                },
                runtimes: vec![],
            },
        );
        conn
    }
}

fn model(id: &str, backend: Backend, state: ModelState) -> LocalModel {
    LocalModel {
        id: id.into(),
        display_name: format!("{id} model"),
        backend,
        state,
        size_bytes: 10,
        context_length: None,
        meta: Default::default(),
        error: None,
    }
}

/// The next request a fake client was sent.
async fn next_request(conn: &mut ConnHandle) -> (String, Vec<ChatTurn>) {
    loop {
        match tokio::time::timeout(Duration::from_secs(2), conn.outbound.recv()).await {
            Ok(Some(ServerMessage::ChatRequest {
                request_id, messages, ..
            })) => return (request_id, messages),
            Ok(Some(_)) => continue,
            other => panic!("expected a chat_request, got {other:?}"),
        }
    }
}

/// Answers like the echo model: the last user message, in two pieces.
fn echo(app: &TestApp, conn: &ConnHandle, attempt: &str, messages: &[ChatTurn]) {
    let text = messages
        .iter()
        .rev()
        .find(|t| t.role == ChatRole::User)
        .map(|t| t.content.clone())
        .unwrap_or_default();
    let (a, b) = text.split_at(text.len() / 2);
    for piece in [a, b] {
        app.state
            .replies
            .on_client_event(conn.id, attempt, ChatEvent::Chunk(piece.to_string()));
    }
    app.state.replies.on_client_event(conn.id, attempt, ChatEvent::Done);
}

/// Parses a whole SSE body into (event, data) pairs.
async fn sse_events(response: Response) -> Vec<(String, Value)> {
    let bytes = tokio::time::timeout(
        Duration::from_secs(5),
        axum::body::to_bytes(response.into_body(), usize::MAX),
    )
    .await
    .expect("the stream ended")
    .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    text.split("\n\n")
        .filter_map(|block| {
            let mut event = None;
            let mut data = String::new();
            for line in block.lines() {
                if let Some(e) = line.strip_prefix("event:") {
                    event = Some(e.trim().to_string());
                } else if let Some(d) = line.strip_prefix("data:") {
                    data.push_str(d.trim_start());
                }
            }
            Some((event?, serde_json::from_str(&data).unwrap_or(Value::Null)))
        })
        .collect()
}

fn target(client_id: &str) -> Value {
    json!({ "kind": "client", "client_id": client_id, "model": "echo" })
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Sends a message and answers it with the echo model; returns the events.
async fn send_and_echo(
    app: &TestApp,
    cookie: &str,
    conn: &mut ConnHandle,
    conversation: &str,
    body: Value,
) -> (Vec<(String, Value)>, Vec<ChatTurn>) {
    let path = format!("/api/conversations/{conversation}/messages");
    let response = app
        .raw(Call {
            cookie: Some(cookie),
            body: Some(body),
            ..app.call(Method::POST, &path)
        })
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let reader = tokio::spawn(sse_events(response));
    let (attempt, messages) = next_request(conn).await;
    echo(app, conn, &attempt, &messages);
    (reader.await.unwrap(), messages)
}

/// The latest snapshot's text plus every chunk after it.
fn streamed_text(events: &[(String, Value)]) -> String {
    let mut text = String::new();
    for (name, data) in events {
        match name.as_str() {
            "snapshot" => text = data["content"].as_str().unwrap_or("").to_string(),
            "chunk" => text.push_str(data["delta"].as_str().unwrap_or("")),
            _ => {}
        }
    }
    text
}

fn reply_id(events: &[(String, Value)]) -> String {
    events[0].1["reply"]["id"].as_str().unwrap().to_string()
}

fn ending(events: &[(String, Value)]) -> (String, Value) {
    events.last().cloned().unwrap()
}

// ---------------- request protection ----------------

#[tokio::test]
async fn every_api_route_needs_a_session_except_the_public_ones() {
    let app = test_app(&[]).await;
    let routes = routes::api_routes().routes;
    for &(method, path) in routes::PUBLIC_API_ROUTES {
        assert!(
            routes.contains(&(method, path)),
            "public route {method} {path} doesn't exist"
        );
    }
    for &(method, path) in &routes {
        let concrete: String = path
            .split('/')
            .map(|seg| if seg.starts_with(':') { "x" } else { seg })
            .collect::<Vec<_>>()
            .join("/");
        let uri = format!("/api{concrete}");
        let method: Method = method.parse().unwrap();
        let body = (method != Method::GET).then(|| json!({}));
        let r = app
            .send(Call {
                body,
                ..app.call(method.clone(), &uri)
            })
            .await;
        if routes::is_public(&method, path) {
            assert_ne!(r.status, StatusCode::UNAUTHORIZED, "{method} {uri} is public");
        } else {
            assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{method} {uri} must need a session");
        }
        // A cookie that isn't a session doesn't help either.
        if !routes::is_public(&method, path) {
            let r = app
                .send(Call {
                    cookie: Some("delegation_session=forged"),
                    body: (method != Method::GET).then(|| json!({})),
                    ..app.call(method.clone(), &uri)
                })
                .await;
            assert_eq!(
                r.status,
                StatusCode::UNAUTHORIZED,
                "{method} {uri} with a forged cookie"
            );
        }
    }
    // Unknown /api paths reveal nothing without a session.
    let r = app.send(app.call(Method::GET, "/api/no-such-thing")).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let admin = app.setup_admin().await;
    assert_eq!(
        app.get("/api/no-such-thing", &admin).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn state_changing_requests_need_this_sites_origin() {
    let app = test_app(&[("EXTRA_ALLOWED_ORIGINS", "http://localhost:5173")]).await;
    let login = json!({ "username": "x", "password": "y" });
    for origin in [None, Some("https://evil.example"), Some("null")] {
        let r = app
            .send(Call {
                body: Some(login.clone()),
                origin,
                ..app.call(Method::POST, "/api/auth/login")
            })
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "origin {origin:?}");
    }
    for origin in [ORIGIN, "http://localhost:5173"] {
        let r = app
            .send(Call {
                body: Some(login.clone()),
                origin: Some(origin),
                ..app.call(Method::POST, "/api/auth/login")
            })
            .await;
        assert_eq!(
            r.status,
            StatusCode::UNAUTHORIZED,
            "origin {origin} reaches the handler"
        );
    }
    // Even with a valid session.
    let admin = app.setup_admin().await;
    let r = app
        .send(Call {
            cookie: Some(&admin),
            body: Some(json!({})),
            origin: Some("https://evil.example"),
            ..app.call(Method::POST, "/api/pairing-codes")
        })
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // GETs don't need it.
    let r = app
        .send(Call {
            cookie: Some(&admin),
            origin: None,
            ..app.call(Method::GET, "/api/me")
        })
        .await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn security_headers_are_on_every_response() {
    let app = test_app(&[]).await;
    for path in ["/api/health", "/", "/clients", "/api/me"] {
        let r = app.send(app.call(Method::GET, path)).await;
        let h = &r.headers;
        assert_eq!(
            h.get(header::CONTENT_SECURITY_POLICY).unwrap(),
            crate::http::CONTENT_SECURITY_POLICY,
            "{path}"
        );
        assert_eq!(h.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
        assert_eq!(h.get(header::REFERRER_POLICY).unwrap(), "same-origin");
        assert!(
            h.get(header::STRICT_TRANSPORT_SECURITY).is_none(),
            "not without COOKIE_SECURE"
        );
    }
    assert_eq!(
        app.send(app.call(Method::GET, "/api/health")).await.status,
        StatusCode::OK
    );
    let secure = test_app(&[("COOKIE_SECURE", "true")]).await;
    let r = secure.send(secure.call(Method::GET, "/api/health")).await;
    assert_eq!(
        r.headers.get(header::STRICT_TRANSPORT_SECURITY).unwrap(),
        "max-age=31536000"
    );
}

// ---------------- first admin, login, sign-up ----------------

#[tokio::test]
async fn setup_creates_the_one_admin_once() {
    let app = test_app(&[]).await;
    let status = app.send(app.call(Method::GET, "/api/auth/status")).await.json;
    assert_eq!(status["setup_required"], true);
    assert_eq!(status["signup_enabled"], false, "not before setup");
    assert_eq!(status["password_min_len"], 12);
    assert_eq!(status["remember_days"], 30);

    // Nobody can sign up and claim the site first.
    let r = app
        .post(
            "/api/auth/signup",
            None,
            json!({ "username": "eve", "password": PASSWORD }),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    let wrong = app
        .post(
            "/api/auth/setup",
            None,
            json!({ "token": "guess", "username": "admin", "password": PASSWORD }),
        )
        .await;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN);

    let admin = app.setup_admin().await;
    let me = app.me(&admin).await;
    assert_eq!(
        (me["username"].as_str(), me["role"].as_str()),
        (Some("admin"), Some("admin"))
    );

    let again = app
        .post(
            "/api/auth/setup",
            None,
            json!({ "token": app.setup_token, "username": "admin2", "password": PASSWORD }),
        )
        .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
    let status = app.send(app.call(Method::GET, "/api/auth/status")).await.json;
    assert_eq!(status["setup_required"], false);
    assert_eq!(status["signup_enabled"], true);

    // The database itself refuses a second admin.
    let second = app.state.db.blocking(|c| {
        Ok(c.execute(
            "INSERT INTO users (id, username, password_hash, role, created_at, password_changed_at)
             VALUES ('x', 'other', 'h', 'admin', 'now', 'now')",
            [],
        ))
    });
    assert!(second.unwrap().is_err());
}

#[tokio::test]
async fn login_failures_look_alike_and_are_rate_limited() {
    let app = test_app(&[]).await;
    let admin = app.setup_admin().await;
    app.signup("bob").await;
    let users = app.get("/api/admin/users", &admin).await.json;
    let bob_id = users["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["username"] == "bob")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        app.patch(
            &format!("/api/admin/users/{bob_id}"),
            &admin,
            json!({ "disabled": true })
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );

    let mut answers = Vec::new();
    for (user, pass) in [("admin", "wrong password!"), ("nobody", PASSWORD), ("bob", PASSWORD)] {
        let started = std::time::Instant::now();
        let r = app.login(user, pass, false).await;
        answers.push((r.status, r.json.clone(), started.elapsed()));
    }
    assert!(answers
        .iter()
        .all(|(s, j, _)| *s == StatusCode::UNAUTHORIZED && *j == answers[0].1));
    // Case-insensitive usernames, and success from the same address.
    assert_eq!(app.login("ADMIN", PASSWORD, false).await.status, StatusCode::OK);

    // Five failures for one username+IP, then the sixth is refused.
    for _ in 0..5 {
        let r = app
            .send(Call {
                body: Some(json!({ "username": "admin", "password": "nope nope nope" })),
                ip: "192.0.2.50",
                ..app.call(Method::POST, "/api/auth/login")
            })
            .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    let r = app
        .send(Call {
            body: Some(json!({ "username": "admin", "password": PASSWORD })),
            ip: "192.0.2.50",
            ..app.call(Method::POST, "/api/auth/login")
        })
        .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    let retry: u64 = r
        .headers
        .get(header::RETRY_AFTER)
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(retry > 0 && retry <= 900);
}

#[tokio::test]
async fn the_per_ip_login_limit_spans_usernames() {
    let app = test_app(&[("AUTH_LOGIN_MAX_FAILURES_PER_IP", "3")]).await;
    app.setup_admin().await;
    for name in ["a1", "a2", "a3"] {
        let r = app
            .send(Call {
                body: Some(json!({ "username": name, "password": "x" })),
                ip: "192.0.2.60",
                ..app.call(Method::POST, "/api/auth/login")
            })
            .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    let r = app
        .send(Call {
            body: Some(json!({ "username": "a4", "password": "x" })),
            ip: "192.0.2.60",
            ..app.call(Method::POST, "/api/auth/login")
        })
        .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn signup_rules() {
    let app = test_app(&[("AUTH_SIGNUP_MAX_PER_IP", "2")]).await;
    app.setup_admin().await;
    let signup = |name: &'static str, ip: &'static str| {
        app.send(Call {
            body: Some(json!({ "username": name, "password": PASSWORD })),
            ip,
            ..app.call(Method::POST, "/api/auth/signup")
        })
    };
    let r = signup("alice", "192.0.2.1").await;
    assert_eq!(r.status, StatusCode::CREATED);
    let cookie = cookie_of(&r);
    let me = app.me(&cookie).await;
    assert_eq!(
        (me["username"].as_str(), me["role"].as_str()),
        (Some("alice"), Some("user"))
    );
    // A new account sees nothing.
    assert_eq!(app.get("/api/clients", &cookie).await.json["clients"], json!([]));
    assert_eq!(
        app.get("/api/conversations", &cookie).await.json["conversations"],
        json!([])
    );

    let r = signup("ALICE", "192.0.2.2").await;
    assert_eq!((r.status, r.error()), (StatusCode::CONFLICT, "That username is taken."));
    for reserved in ["admin", "Root", "delegation"] {
        let r = app
            .send(Call {
                body: Some(json!({ "username": reserved, "password": PASSWORD })),
                ip: "192.0.2.3",
                ..app.call(Method::POST, "/api/auth/signup")
            })
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{reserved}");
    }
    let r = app
        .send(Call {
            body: Some(json!({ "username": "shorty", "password": "too short" })),
            ip: "192.0.2.3",
            ..app.call(Method::POST, "/api/auth/signup")
        })
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    // Two accounts per address per window.
    assert_eq!(signup("carol", "192.0.2.9").await.status, StatusCode::CREATED);
    assert_eq!(signup("dave", "192.0.2.9").await.status, StatusCode::CREATED);
    assert_eq!(signup("erin", "192.0.2.9").await.status, StatusCode::TOO_MANY_REQUESTS);

    let closed = test_app(&[("AUTH_SIGNUP_ENABLED", "false")]).await;
    closed.setup_admin().await;
    let status = closed.send(closed.call(Method::GET, "/api/auth/status")).await.json;
    assert_eq!(status["signup_enabled"], false);
    let r = closed
        .post(
            "/api/auth/signup",
            None,
            json!({ "username": "frank", "password": PASSWORD }),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn remember_me_decides_the_cookie_and_session_lifetime() {
    let app = test_app(&[]).await;
    app.setup_admin().await;
    let remembered = app.login("admin", PASSWORD, true).await;
    let header = set_cookie(&remembered);
    assert!(header.contains("HttpOnly") && header.contains("SameSite=Lax") && header.contains("Path=/"));
    assert!(header.contains(&format!("Max-Age={}", 30 * 86_400)), "{header}");
    assert!(!header.contains("Secure"), "COOKIE_SECURE=false");
    assert!(!header.contains("Domain"));

    let short = app.login("admin", PASSWORD, false).await;
    assert!(!set_cookie(&short).contains("Max-Age"), "a browser-session cookie");

    let expiries: Vec<(bool, String)> = app
        .state
        .db
        .blocking(|c| {
            let mut stmt = c.prepare("SELECT persistent, expires_at FROM sessions ORDER BY persistent")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
        .unwrap();
    let left = |at: &str| {
        db::parse_ts(at)
            .unwrap()
            .duration_since(SystemTime::now())
            .unwrap()
            .as_secs()
    };
    let short_left = expiries.iter().find(|(p, _)| !p).map(|(_, at)| left(at)).unwrap();
    let long_left = expiries.iter().find(|(p, _)| *p).map(|(_, at)| left(at)).unwrap();
    assert!((12 * 3600 - 60..=12 * 3600).contains(&short_left), "{short_left}");
    assert!((30 * 86_400 - 60..=30 * 86_400).contains(&long_left), "{long_left}");

    // An expired session stops working and is cleared.
    let cookie = cookie_of(&short);
    app.state
        .db
        .blocking(|c| Ok(c.execute("UPDATE sessions SET expires_at = '2000-01-01T00:00:00.000Z'", [])?))
        .unwrap();
    // Past the cache: a fresh app over the same database would see it at once;
    // here, wait out the cache.
    tokio::time::sleep(Duration::from_millis(5_100)).await;
    let r = app.get("/api/me", &cookie).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert!(set_cookie(&r).contains("Max-Age=0"));

    let secure = test_app(&[("COOKIE_SECURE", "true")]).await;
    let r = secure
        .post(
            "/api/auth/setup",
            None,
            json!({ "token": secure.setup_token, "username": "admin", "password": PASSWORD, "remember": true }),
        )
        .await;
    let header = set_cookie(&r);
    assert!(
        header.starts_with("__Host-delegation_session=") && header.contains("; Secure"),
        "{header}"
    );
}

#[tokio::test]
async fn logout_ends_only_this_session() {
    let app = test_app(&[]).await;
    let a = app.setup_admin().await;
    let b = cookie_of(&app.login("admin", PASSWORD, false).await);
    let r = app.post("/api/auth/logout", Some(&a), json!({})).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(set_cookie(&r).contains("Max-Age=0"));
    assert_eq!(app.get("/api/me", &a).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(app.get("/api/me", &b).await.status, StatusCode::OK);
}

// ---------------- passwords ----------------

async fn user_id(app: &TestApp, admin: &str, username: &str) -> String {
    app.get("/api/admin/users", admin).await.json["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["username"] == username)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn reset_links_work_once_expire_and_sign_out_everywhere() {
    let app = test_app(&[]).await;
    let admin = app.setup_admin().await;
    let bob = app.signup("bob").await;
    let bob_id = user_id(&app, &admin, "bob").await;
    let link = |r: Resp| {
        let url = r.json["url"].as_str().unwrap().to_string();
        assert!(url.starts_with(&format!("{ORIGIN}/reset/")), "{url}");
        url.rsplit('/').next().unwrap().to_string()
    };
    let path = format!("/api/admin/users/{bob_id}/reset-link");
    let first = link(app.post(&path, Some(&admin), json!({})).await);
    let second = link(app.post(&path, Some(&admin), json!({})).await);
    // Only admins make them.
    assert_eq!(
        app.post(&path, Some(&bob), json!({})).await.status,
        StatusCode::FORBIDDEN
    );

    let app = &app;
    let info = |token: String| async move {
        app.send(app.call(Method::GET, &format!("/api/password-reset/{token}")))
            .await
    };
    assert_eq!(
        info(first.clone()).await.status,
        StatusCode::NOT_FOUND,
        "replaced by the newer link"
    );
    let r = info(second.clone()).await;
    assert_eq!(r.json["username"], "bob");

    let new_password = "a brand new password";
    let r = app
        .post(
            &format!("/api/password-reset/{second}"),
            None,
            json!({ "password": new_password }),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(
        app.get("/api/me", &bob).await.status,
        StatusCode::UNAUTHORIZED,
        "signed out"
    );
    assert_eq!(app.login("bob", PASSWORD, false).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(app.login("bob", new_password, false).await.status, StatusCode::OK);
    let again = app
        .post(
            &format!("/api/password-reset/{second}"),
            None,
            json!({ "password": PASSWORD }),
        )
        .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND, "single use");

    // Expiry.
    let third = link(app.post(&path, Some(&admin), json!({})).await);
    app.state
        .db
        .blocking(|c| {
            Ok(c.execute(
                "UPDATE password_resets SET expires_at = '2000-01-01T00:00:00.000Z' WHERE used_at IS NULL",
                [],
            )?)
        })
        .unwrap();
    assert_eq!(info(third).await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn changing_a_password_signs_out_other_sessions() {
    let app = test_app(&[]).await;
    app.setup_admin().await;
    let a = app.signup("bob").await;
    let b = cookie_of(&app.login("bob", PASSWORD, false).await);
    let wrong = app
        .post(
            "/api/me/password",
            Some(&a),
            json!({ "current_password": "not it at all", "new_password": "another long password" }),
        )
        .await;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN);
    let r = app
        .post(
            "/api/me/password",
            Some(&a),
            json!({ "current_password": PASSWORD, "new_password": "another long password" }),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(
        app.get("/api/me", &a).await.status,
        StatusCode::OK,
        "this session stays"
    );
    assert_eq!(
        app.get("/api/me", &b).await.status,
        StatusCode::UNAUTHORIZED,
        "others end"
    );
    assert_eq!(
        app.login("bob", "another long password", false).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_weak_hash_is_upgraded_on_login() {
    let app = test_app(&[]).await;
    app.setup_admin().await;
    let weak = crate::auth::password::Hasher::new(crate::config::Argon2Settings {
        memory_kib: 32,
        iterations: 1,
        parallelism: 1,
    })
    .unwrap()
    .hash_blocking(PASSWORD)
    .unwrap();
    app.state
        .db
        .blocking(move |c| Ok(c.execute("UPDATE users SET password_hash = ?1", [&weak])?))
        .unwrap();
    assert_eq!(app.login("admin", PASSWORD, false).await.status, StatusCode::OK);
    let stored: String = app
        .state
        .db
        .blocking(|c| Ok(c.query_row("SELECT password_hash FROM users", [], |r| r.get(0))?))
        .unwrap();
    assert!(stored.contains("m=64,"), "{stored}");
}

// ---------------- the admin's user management ----------------

#[tokio::test]
async fn disabling_ends_access_and_deleting_removes_everything() {
    let app = test_app(&[]).await;
    let admin = app.setup_admin().await;
    let bob = app.signup("bob").await;
    let bob_id = user_id(&app, &admin, "bob").await;
    let admin_id = app.me(&admin).await["id"].as_str().unwrap().to_string();

    assert_eq!(app.get("/api/me", &bob).await.status, StatusCode::OK);
    let path = format!("/api/admin/users/{bob_id}");
    assert_eq!(
        app.patch(&path, &admin, json!({ "disabled": true })).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.get("/api/me", &bob).await.status,
        StatusCode::UNAUTHORIZED,
        "immediately"
    );
    assert_eq!(app.login("bob", PASSWORD, false).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        app.patch(&path, &admin, json!({ "disabled": false })).await.status,
        StatusCode::NO_CONTENT
    );
    let bob = cookie_of(&app.login("bob", PASSWORD, false).await);

    // The admin can't lock themselves out, and only the admin can do this.
    let me = format!("/api/admin/users/{admin_id}");
    assert_eq!(
        app.patch(&me, &admin, json!({ "disabled": true })).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(app.delete(&me, &admin).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        app.patch(&me, &bob, json!({ "disabled": true })).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(app.get("/api/admin/users", &bob).await.status, StatusCode::FORBIDDEN);

    // Bob gets a connected client and a conversation.
    let client = app.enroll(&bob, "bob-pc").await;
    let mut conn = app.connect(&client);
    let (events, _) = send_and_echo(
        &app,
        &bob,
        &mut conn,
        &new_id(),
        json!({ "content": "hi", "parent_id": null, "target": target(&client) }),
    )
    .await;
    assert_eq!(ending(&events).1["status"], "completed");

    let users = app.get("/api/admin/users", &admin).await.json;
    assert_eq!(users["signup_enabled"], true);
    let row = users["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == bob_id)
        .unwrap()
        .clone();
    assert_eq!(
        (row["client_count"].as_i64(), row["conversation_count"].as_i64()),
        (Some(1), Some(1))
    );
    assert!(row["last_active_at"].is_string());
    assert_eq!(users["users"][0]["username"], "bob", "newest first");

    assert_eq!(app.delete(&path, &admin).await.status, StatusCode::NO_CONTENT);
    assert_eq!(app.get("/api/me", &bob).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(app.get("/api/clients", &admin).await.json["clients"], json!([]));
    let left: (i64, i64) = app
        .state
        .db
        .blocking(|c| {
            Ok((
                c.query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))?,
                c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?,
            ))
        })
        .unwrap();
    assert_eq!(left, (0, 0));
    // Its connection was told to close.
    tokio::time::timeout(Duration::from_secs(1), conn.kick.notified())
        .await
        .expect("kicked");
    assert_eq!(app.delete(&path, &admin).await.status, StatusCode::NOT_FOUND);
}

// ---------------- client ownership ----------------

#[tokio::test]
async fn clients_belong_to_whoever_minted_the_code() {
    let app = test_app(&[]).await;
    let admin = app.setup_admin().await;
    let alice = app.signup("alice").await;
    let bob = app.signup("bob").await;
    let a1 = app.enroll(&alice, "alice-pc").await;
    let b1 = app.enroll(&bob, "bob-pc").await;
    let _a_conn = app.connect(&a1);
    let _b_conn = app.connect(&b1);

    let app = &app;
    let list = |cookie: String| async move {
        let r = app.get("/api/clients", &cookie).await;
        assert_eq!(r.status, StatusCode::OK);
        r.json["clients"].as_array().unwrap().clone()
    };
    let alices = list(alice.clone()).await;
    assert_eq!(alices.len(), 1);
    assert_eq!(alices[0]["client_id"], a1);
    assert_eq!(alices[0]["owner"]["username"], "alice");
    assert_eq!(
        (&alices[0]["access"], &alices[0]["can_use"], &alices[0]["can_manage"]),
        (&json!("owner"), &json!(true), &json!(true))
    );
    let all = list(admin.clone()).await;
    assert_eq!(all.len(), 2);
    let bobs_for_admin = all.iter().find(|c| c["client_id"] == b1).unwrap();
    assert_eq!(bobs_for_admin["access"], "admin");
    assert_eq!(bobs_for_admin["owner"]["username"], "bob");

    // Another user's client is invisible: 404 for everything.
    let other = format!("/api/clients/{b1}");
    assert_eq!(
        app.patch(&other, &alice, json!({ "friendly_name": "mine" }))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(app.delete(&other, &alice).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        app.patch(
            &format!("/api/clients/{a1}"),
            &alice,
            json!({ "friendly_name": "Alice's PC" })
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.patch(&other, &admin, json!({ "friendly_name": "Bob's" }))
            .await
            .status,
        StatusCode::NO_CONTENT
    );

    // Chat: the model list and sending follow the same rule.
    let models = app.get("/api/chat/models", &alice).await.json;
    assert_eq!(models.as_array().unwrap().len(), 1);
    assert_eq!(models[0]["client_id"], a1);
    assert_eq!(models[0]["client_name"], "Alice's PC");
    assert_eq!(
        app.get("/api/chat/models", &admin).await.json.as_array().unwrap().len(),
        2
    );
    let r = app
        .post(
            &format!("/api/conversations/{}/messages", new_id()),
            Some(&alice),
            json!({ "content": "hi", "parent_id": null, "target": target(&b1) }),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // Live samples only for visible clients; the cursor ignores the rest.
    let r = app.get("/api/clients?since=0", &alice).await.json;
    assert_eq!(r["clients"].as_array().unwrap().len(), 1);

    // The admin deletes Bob's client.
    assert_eq!(app.delete(&other, &admin).await.status, StatusCode::NO_CONTENT);
    assert_eq!(list(bob.clone()).await.len(), 0);
}

#[tokio::test]
async fn pairing_code_status_is_only_for_its_creator() {
    let app = test_app(&[]).await;
    let admin = app.setup_admin().await;
    let alice = app.signup("alice").await;
    let code = app.post("/api/pairing-codes", Some(&alice), json!({})).await.json["code"]
        .as_str()
        .unwrap()
        .to_string();
    let path = format!("/api/pairing-codes/{code}/status");
    let mine = app.get(&path, &alice).await;
    assert_eq!((mine.status, &mine.json["used"]), (StatusCode::OK, &json!(false)));
    assert_eq!(app.get(&path, &admin).await.status, StatusCode::NOT_FOUND);
    assert_eq!(app.get("/api/dashboard-config", &alice).await.status, StatusCode::OK);
}

#[tokio::test]
async fn enrollment_is_rate_limited_per_ip() {
    let app = test_app(&[("ENROLL_MAX_ATTEMPTS_PER_IP", "2")]).await;
    for expected in [
        StatusCode::FORBIDDEN,
        StatusCode::FORBIDDEN,
        StatusCode::TOO_MANY_REQUESTS,
    ] {
        let r = app
            .send(Call {
                body: Some(json!({ "pairing_code": "WRONG", "public_key": "pk", "hostname": "h" })),
                origin: None,
                ip: "192.0.2.77",
                ..app.call(Method::POST, "/enroll")
            })
            .await;
        assert_eq!(r.status, expected);
    }
}

// ---------------- chat ----------------

#[tokio::test]
async fn a_reply_streams_and_is_saved() {
    let app = test_app(&[]).await;
    app.setup_admin().await;
    let alice = app.signup("alice").await;
    let client = app.enroll(&alice, "alice-pc").await;
    let mut conn = app.connect(&client);
    let conversation = new_id();

    let (events, sent) = send_and_echo(
        &app,
        &alice,
        &mut conn,
        &conversation,
        json!({ "content": "hello there\nsecond line", "parent_id": null, "target": target(&client) }),
    )
    .await;
    assert_eq!(sent.len(), 1, "history is built by the server");
    let names: Vec<&str> = events.iter().map(|(e, _)| e.as_str()).collect();
    assert_eq!(names.first(), Some(&"created"));
    assert_eq!(names[1], "snapshot");
    // However the pieces split between the snapshot and chunks, they add up.
    assert_eq!(streamed_text(&events), "hello there\nsecond line");
    let (last, data) = ending(&events);
    assert_eq!((last.as_str(), &data["status"]), ("end", &json!("completed")));
    assert_eq!(events[0].1["conversation"]["title"], "hello there");
    let reply = reply_id(&events);

    let c = app
        .get(&format!("/api/conversations/{conversation}"), &alice)
        .await
        .json;
    let messages = c["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(c["active_leaf_id"], reply);
    let r = messages.iter().find(|m| m["id"] == reply).unwrap();
    assert_eq!(
        (
            &r["status"],
            &r["content"],
            &r["client_label"],
            &r["model"],
            &r["attempts"]
        ),
        (
            &json!("completed"),
            &json!("hello there\nsecond line"),
            &json!("alice-pc"),
            &json!("echo"),
            &json!(1)
        )
    );

    // Reconnecting to a finished reply: snapshot and end at once.
    let events = sse_events(
        app.raw(Call {
            cookie: Some(&alice),
            ..app.call(Method::GET, &format!("/api/messages/{reply}/stream"))
        })
        .await,
    )
    .await;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].1["content"], "hello there\nsecond line");
    assert_eq!(events[1].1["status"], "completed");

    let list = app.get("/api/conversations", &alice).await.json;
    assert_eq!(list["conversations"][0]["id"], conversation);
    assert!(list["next_before"].is_null());
}

#[tokio::test]
async fn a_reply_keeps_going_when_the_browser_leaves() {
    let app = test_app(&[]).await;
    let admin = app.setup_admin().await;
    let alice = app.signup("alice").await;
    let client = app.enroll(&alice, "alice-pc").await;
    let mut conn = app.connect(&client);
    let conversation = new_id();
    let response = app
        .raw(Call {
            cookie: Some(&alice),
            body: Some(json!({ "content": "keep going", "parent_id": null, "target": target(&client) })),
            ..app.call(Method::POST, &format!("/api/conversations/{conversation}/messages"))
        })
        .await;
    drop(response); // the tab closed
    let (attempt, messages) = next_request(&mut conn).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Nothing told the client to stop.
    assert!(conn.outbound.try_recv().is_err());

    // Reload mid-reply: the reconnect picks up where it is.
    app.state
        .replies
        .on_client_event(conn.id, &attempt, ChatEvent::Chunk("keep ".into()));
    // The card shows the client working, to anyone who can see it.
    let app_ref = &app;
    let jobs = |cookie: String| async move {
        app_ref.get("/api/clients", &cookie).await.json["clients"][0]["active_jobs"].clone()
    };
    assert_eq!(jobs(alice.clone()).await, json!(1));
    assert_eq!(jobs(admin.clone()).await, json!(1), "anyone's job counts");
    let c = app
        .get(&format!("/api/conversations/{conversation}"), &alice)
        .await
        .json;
    let reply = c["active_leaf_id"].as_str().unwrap().to_string();
    let r = c["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == reply)
        .unwrap()
        .clone();
    assert_eq!((&r["status"], &r["content"]), (&json!("running"), &json!("keep ")));
    let stream = app
        .raw(Call {
            cookie: Some(&alice),
            ..app.call(Method::GET, &format!("/api/messages/{reply}/stream"))
        })
        .await;
    let reader = tokio::spawn(sse_events(stream));
    tokio::time::sleep(Duration::from_millis(50)).await;
    echo(
        &app,
        &conn,
        &attempt,
        &[ChatTurn {
            role: ChatRole::User,
            content: messages[0].content.replace("keep ", ""),
        }],
    );
    let events = reader.await.unwrap();
    assert_eq!(events[0].0, "snapshot");
    assert_eq!(events[0].1["content"], "keep ");
    assert_eq!(ending(&events).1["status"], "completed");
    let content: String = events
        .iter()
        .filter(|(e, _)| e == "chunk")
        .map(|(_, d)| d["delta"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(format!("keep {content}"), "keep going");
    assert_eq!(jobs(alice.clone()).await, json!(0));
}

#[tokio::test]
async fn the_history_sent_follows_the_selected_branch() {
    let app = test_app(&[]).await;
    app.setup_admin().await;
    let alice = app.signup("alice").await;
    let client = app.enroll(&alice, "alice-pc").await;
    let mut conn = app.connect(&client);
    let conv = new_id();
    let (e1, _) = send_and_echo(
        &app,
        &alice,
        &mut conn,
        &conv,
        json!({ "content": "one", "parent_id": null, "target": target(&client) }),
    )
    .await;
    let r1 = reply_id(&e1);
    let (e2, _) = send_and_echo(
        &app,
        &alice,
        &mut conn,
        &conv,
        json!({ "content": "two", "parent_id": r1, "target": target(&client) }),
    )
    .await;
    let u2 = e2[0].1["user_message"]["id"].as_str().unwrap().to_string();
    let r2 = reply_id(&e2);

    // Edit "two": a sibling user message, answered with only its own branch.
    let (e3, history) = send_and_echo(
        &app,
        &alice,
        &mut conn,
        &conv,
        json!({ "content": "two, edited", "parent_id": r1, "target": target(&client) }),
    )
    .await;
    let texts: Vec<&str> = history.iter().map(|t| t.content.as_str()).collect();
    assert_eq!(texts, vec!["one", "one", "two, edited"]);
    assert_eq!(e3[0].1["user_message"]["parent_id"], r1);

    // Regenerate r2: no new user message, a sibling reply to "two".
    let (e4, history) = send_and_echo(
        &app,
        &alice,
        &mut conn,
        &conv,
        json!({ "parent_id": u2, "target": target(&client) }),
    )
    .await;
    assert!(e4[0].1["user_message"].is_null());
    assert_eq!(e4[0].1["reply"]["parent_id"], u2);
    let texts: Vec<&str> = history.iter().map(|t| t.content.as_str()).collect();
    assert_eq!(texts, vec!["one", "one", "two"]);

    let c = app.get(&format!("/api/conversations/{conv}"), &alice).await.json;
    let siblings = c["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["parent_id"] == u2)
        .count();
    assert_eq!(siblings, 2);
    assert_eq!(c["active_leaf_id"], reply_id(&e4));

    // Switch back to r2's branch.
    let path = format!("/api/conversations/{conv}");
    assert_eq!(
        app.patch(&path, &alice, json!({ "active_leaf_id": r2 })).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(app.get(&path, &alice).await.json["active_leaf_id"], r2);
    assert_eq!(
        app.patch(&path, &alice, json!({ "active_leaf_id": "nope" }))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.patch(&path, &alice, json!({ "title": "Renamed" })).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(app.get(&path, &alice).await.json["title"], "Renamed");
}

#[tokio::test]
async fn stop_retry_and_orphaning_through_the_api() {
    let app = test_app(&[]).await;
    app.setup_admin().await;
    let alice = app.signup("alice").await;
    let client = app.enroll(&alice, "alice-pc").await;
    let mut conn = app.connect(&client);
    let conv = new_id();
    let send = app
        .raw(Call {
            cookie: Some(&alice),
            body: Some(json!({ "content": "stop me", "parent_id": null, "target": target(&client) })),
            ..app.call(Method::POST, &format!("/api/conversations/{conv}/messages"))
        })
        .await;
    let reader = tokio::spawn(sse_events(send));
    let (attempt, _) = next_request(&mut conn).await;
    app.state
        .replies
        .on_client_event(conn.id, &attempt, ChatEvent::Chunk("partial".into()));

    // One reply at a time per conversation.
    let c = app.get(&format!("/api/conversations/{conv}"), &alice).await.json;
    let reply = c["active_leaf_id"].as_str().unwrap().to_string();
    let busy = app
        .post(
            &format!("/api/conversations/{conv}/messages"),
            Some(&alice),
            json!({ "content": "again", "parent_id": reply, "target": target(&client) }),
        )
        .await;
    assert_eq!(busy.status, StatusCode::CONFLICT);

    let stopped = app
        .post(&format!("/api/messages/{reply}/cancel"), Some(&alice), json!({}))
        .await;
    assert_eq!(stopped.status, StatusCode::OK);
    assert_eq!(
        (&stopped.json["status"], &stopped.json["content"]),
        (&json!("cancelled"), &json!("partial"))
    );
    assert!(matches!(
        conn.outbound.recv().await,
        Some(ServerMessage::ChatCancel { request_id }) if request_id == attempt
    ));
    assert_eq!(ending(&reader.await.unwrap()).1["status"], "cancelled");
    let r = app
        .post(&format!("/api/messages/{reply}/retry"), Some(&alice), json!({}))
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "cancelled replies aren't retried");

    // The client dies mid-reply.
    let send = app
        .raw(Call {
            cookie: Some(&alice),
            body: Some(json!({ "content": "die", "parent_id": reply, "target": target(&client) })),
            ..app.call(Method::POST, &format!("/api/conversations/{conv}/messages"))
        })
        .await;
    let reader = tokio::spawn(sse_events(send));
    let (attempt, _) = next_request(&mut conn).await;
    app.state
        .replies
        .on_client_event(conn.id, &attempt, ChatEvent::Chunk("half".into()));
    app.state.live.unregister(&client, conn.id);
    app.state.replies.on_connection_closed(conn.id);
    let events = reader.await.unwrap();
    let orphan = reply_id(&events);
    assert_eq!(ending(&events).1["status"], "orphaned");

    // Retry once it's back: same client and model by default.
    let mut conn = app.connect(&client);
    let response = app
        .raw(Call {
            cookie: Some(&alice),
            body: None,
            ..app.call(Method::POST, &format!("/api/messages/{orphan}/retry"))
        })
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let reader = tokio::spawn(sse_events(response));
    let (second, messages) = next_request(&mut conn).await;
    assert_ne!(second, attempt);
    // The old attempt's late chunks are ignored.
    app.state
        .replies
        .on_client_event(conn.id, &attempt, ChatEvent::Chunk("stale".into()));
    echo(&app, &conn, &second, &messages);
    let events = reader.await.unwrap();
    assert_eq!(events[0].0, "created");
    assert_eq!(ending(&events).1["status"], "completed");
    let m = app.get(&format!("/api/conversations/{conv}"), &alice).await.json["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == orphan)
        .unwrap()
        .clone();
    assert_eq!((&m["content"], &m["attempts"]), (&json!("die"), &json!(2)));
}

#[tokio::test]
async fn sends_are_validated() {
    let app = test_app(&[
        ("CHAT_MAX_MESSAGE_CHARS", "10"),
        ("CHAT_MAX_ACTIVE_REPLIES_PER_USER", "1"),
    ])
    .await;
    app.setup_admin().await;
    let alice = app.signup("alice").await;
    let client = app.enroll(&alice, "alice-pc").await;
    let mut conn = app.connect(&client);
    let path = format!("/api/conversations/{}/messages", new_id());
    let send = |body: Value| app.post(&path, Some(&alice), body);

    let pool = send(json!({ "content": "hi", "parent_id": null, "target": { "kind": "pool", "model": "echo" } })).await;
    assert_eq!(pool.status, StatusCode::BAD_REQUEST);
    let none = send(json!({ "content": "hi", "parent_id": null })).await;
    assert_eq!(none.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        send(json!({ "content": "x".repeat(11), "parent_id": null, "target": target(&client) }))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(json!({ "content": "  ", "parent_id": null, "target": target(&client) }))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let wrong_model = json!({ "content": "hi", "parent_id": null, "target": { "kind": "client", "client_id": client, "model": "gpt" } });
    assert_eq!(send(wrong_model).await.status, StatusCode::BAD_REQUEST);
    let bad_id = app
        .post(
            "/api/conversations/not-a-uuid/messages",
            Some(&alice),
            json!({ "content": "hi", "parent_id": null, "target": target(&client) }),
        )
        .await;
    assert_eq!(bad_id.status, StatusCode::BAD_REQUEST);

    // One active reply per user (the limit here): a second conversation is refused.
    let first = app
        .raw(Call {
            cookie: Some(&alice),
            body: Some(json!({ "content": "hi", "parent_id": null, "target": target(&client) })),
            ..app.call(Method::POST, &path)
        })
        .await;
    let _ = next_request(&mut conn).await;
    let second = app
        .post(
            &format!("/api/conversations/{}/messages", new_id()),
            Some(&alice),
            json!({ "content": "hi", "parent_id": null, "target": target(&client) }),
        )
        .await;
    assert_eq!(second.status, StatusCode::TOO_MANY_REQUESTS);
    drop(first);

    // A disconnected client is refused before anything is created.
    app.state.live.unregister(&client, conn.id);
    let offline = app
        .post(
            &format!("/api/conversations/{}/messages", new_id()),
            Some(&alice),
            json!({ "content": "hi", "parent_id": null, "target": target(&client) }),
        )
        .await;
    assert_eq!(offline.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn other_users_get_404_for_every_conversation_and_message_endpoint() {
    let app = test_app(&[]).await;
    let admin = app.setup_admin().await;
    let alice = app.signup("alice").await;
    let bob = app.signup("bob").await;
    let client = app.enroll(&alice, "alice-pc").await;
    let mut conn = app.connect(&client);
    let conv = new_id();
    let (events, _) = send_and_echo(
        &app,
        &alice,
        &mut conn,
        &conv,
        json!({ "content": "secret", "parent_id": null, "target": target(&client) }),
    )
    .await;
    let reply = reply_id(&events);

    for intruder in [&bob, &admin] {
        let c = format!("/api/conversations/{conv}");
        assert_eq!(app.get(&c, intruder).await.status, StatusCode::NOT_FOUND);
        assert_eq!(
            app.patch(&c, intruder, json!({ "title": "x" })).await.status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(app.delete(&c, intruder).await.status, StatusCode::NOT_FOUND);
        assert_eq!(
            app.post(
                &format!("{c}/messages"),
                Some(intruder),
                json!({ "content": "x", "parent_id": reply, "target": target(&client) })
            )
            .await
            .status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            app.get(&format!("/api/messages/{reply}/stream"), intruder).await.status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            app.post(&format!("/api/messages/{reply}/cancel"), Some(intruder), json!({}))
                .await
                .status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            app.post(&format!("/api/messages/{reply}/retry"), Some(intruder), json!({}))
                .await
                .status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            app.get("/api/conversations", intruder).await.json["conversations"],
            json!([])
        );
    }
    assert_eq!(
        app.get(&format!("/api/conversations/{conv}"), &alice).await.status,
        StatusCode::OK
    );
    assert_eq!(
        app.delete(&format!("/api/conversations/{conv}"), &alice).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.get(&format!("/api/conversations/{conv}"), &alice).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn deleting_a_client_keeps_its_replies_readable() {
    let app = test_app(&[]).await;
    app.setup_admin().await;
    let alice = app.signup("alice").await;
    let client = app.enroll(&alice, "alice-pc").await;
    let mut conn = app.connect(&client);
    let conv = new_id();
    let (events, _) = send_and_echo(
        &app,
        &alice,
        &mut conn,
        &conv,
        json!({ "content": "hi", "parent_id": null, "target": target(&client) }),
    )
    .await;
    let reply = reply_id(&events);
    assert_eq!(
        app.delete(&format!("/api/clients/{client}"), &alice).await.status,
        StatusCode::NO_CONTENT
    );
    let c = app.get(&format!("/api/conversations/{conv}"), &alice).await.json;
    let m = c["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == reply)
        .unwrap()
        .clone();
    assert_eq!(
        (&m["client_id"], &m["client_label"], &m["content"]),
        (&Value::Null, &json!("alice-pc"), &json!("hi"))
    );
    let status = app
        .state
        .db
        .blocking(move |c| Ok(crate::conversations::message_state(c, &reply)?.unwrap().0))
        .unwrap();
    assert_eq!(status, MessageStatus::Completed);
}

// ---------------- models on clients ----------------

/// The next model command a fake client was sent.
async fn next_command(conn: &mut ConnHandle) -> (String, ModelCommandKind) {
    match tokio::time::timeout(Duration::from_secs(2), conn.outbound.recv()).await {
        Ok(Some(ServerMessage::ModelCommand { command_id, command })) => (command_id, command),
        other => panic!("expected a model_command, got {other:?}"),
    }
}

const TEST_MODEL: &str = crate::config::DEFAULT_TEST_MODEL_ID;

#[tokio::test]
async fn installing_needs_manage_and_takes_catalog_ids_only() {
    let app = test_app(&[]).await;
    let admin = app.setup_admin().await;
    let alice = app.signup("alice").await;
    let bob = app.signup("bob").await;
    let a1 = app.enroll(&alice, "alice-pc").await;
    let b1 = app.enroll(&bob, "bob-pc").await;
    let mut a_conn = app.connect(&a1);
    let mut b_conn = app.connect(&b1);

    // Bob's client is invisible to Alice: 404 for every models endpoint.
    let install = json!({ "catalog_id": TEST_MODEL });
    assert_eq!(
        app.get(&format!("/api/clients/{b1}/models"), &alice).await.status,
        StatusCode::NOT_FOUND
    );
    let r = app
        .post(&format!("/api/clients/{b1}/models"), Some(&alice), install.clone())
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = app
        .delete(&format!("/api/clients/{b1}/models/{TEST_MODEL}"), &alice)
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = app
        .post(
            &format!("/api/clients/{b1}/model-commands/x/cancel"),
            Some(&alice),
            json!({}),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(b_conn.outbound.try_recv().is_err(), "nothing reached Bob's client");

    // The browser can't name a URL or hash, only a catalog id.
    let r = app
        .post(
            &format!("/api/clients/{a1}/models"),
            Some(&alice),
            json!({ "catalog_id": TEST_MODEL, "url": "https://evil.example/x.gguf" }),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    let r = app
        .post(
            &format!("/api/clients/{a1}/models"),
            Some(&alice),
            json!({ "catalog_id": "nope" }),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(a_conn.outbound.try_recv().is_err());

    let catalog = app.get("/api/models/catalog", &alice).await.json;
    assert_eq!(catalog[0]["id"], TEST_MODEL);
    assert!(catalog[0].get("url").is_none() && catalog[0].get("source").is_none());

    // Alice installs on her own client: the pull carries the catalog's source.
    let r = app
        .post(&format!("/api/clients/{a1}/models"), Some(&alice), install.clone())
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{:?}", r.json);
    assert_eq!(
        (&r.json["state"], &r.json["requested_by"]),
        (&json!("sent"), &json!("alice"))
    );
    let (command_id, command) = next_command(&mut a_conn).await;
    assert_eq!(r.json["command_id"], command_id);
    let ModelCommandKind::Pull { model: spec } = command else {
        panic!("expected a pull")
    };
    assert_eq!(spec, app.state.settings.test_model);
    // Asking again while it runs is the same command.
    let again = app
        .post(&format!("/api/clients/{a1}/models"), Some(&alice), install.clone())
        .await;
    assert_eq!(again.json["command_id"], command_id);
    assert!(a_conn.outbound.try_recv().is_err());

    // Progress shows in the detail and in the poll's summary.
    let progress = Progress {
        done_bytes: 25,
        total_bytes: 100,
        phase: ProgressPhase::Model,
    };
    app.state
        .model_commands
        .on_status(a_conn.id, &command_id, CommandState::InProgress, Some(progress), None);
    let detail = app.get(&format!("/api/clients/{a1}/models"), &alice).await.json;
    assert_eq!(detail["can_manage"], true);
    assert_eq!(detail["storage"]["free_bytes"], 50_000);
    assert_eq!(detail["commands"][0]["state"], "in_progress");
    assert_eq!(detail["commands"][0]["progress"]["done_bytes"], 25);
    let clients = app.get("/api/clients", &alice).await.json;
    let pulls = &clients["clients"][0]["models"]["active_pulls"];
    assert_eq!(pulls[0]["command_id"], command_id);
    assert_eq!(pulls[0]["progress"]["total_bytes"], 100);

    // Cancel sends a cancel naming the pull.
    let r = app
        .post(
            &format!("/api/clients/{a1}/model-commands/{command_id}/cancel"),
            Some(&alice),
            json!({}),
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    let (_, cancel) = next_command(&mut a_conn).await;
    assert_eq!(
        cancel,
        ModelCommandKind::Cancel {
            target_command_id: command_id.clone()
        }
    );
    app.state
        .model_commands
        .on_status(a_conn.id, &command_id, CommandState::Cancelled, None, None);
    let r = app
        .post(
            &format!("/api/clients/{a1}/model-commands/{command_id}/cancel"),
            Some(&alice),
            json!({}),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "a finished pull can't be cancelled");

    // Delete goes out as a delete; echo is built in.
    let r = app
        .delete(&format!("/api/clients/{a1}/models/{TEST_MODEL}"), &alice)
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    assert!(
        matches!(next_command(&mut a_conn).await.1, ModelCommandKind::Delete { model_id } if model_id == TEST_MODEL)
    );
    assert_eq!(
        app.delete(&format!("/api/clients/{a1}/models/echo"), &alice)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.delete(&format!("/api/clients/{a1}/models/..%2Fx"), &alice)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );

    // The admin manages everyone's clients.
    let r = app
        .post(&format!("/api/clients/{b1}/models"), Some(&admin), install)
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    assert!(matches!(
        next_command(&mut b_conn).await.1,
        ModelCommandKind::Pull { .. }
    ));
}

#[tokio::test]
async fn offline_clients_fail_commands_immediately() {
    let app = test_app(&[]).await;
    app.setup_admin().await;
    let alice = app.signup("alice").await;
    let a1 = app.enroll(&alice, "alice-pc").await;
    let r = app
        .post(
            &format!("/api/clients/{a1}/models"),
            Some(&alice),
            json!({ "catalog_id": TEST_MODEL }),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert!(r.error().contains("offline"), "{}", r.error());
    assert_eq!(r.json["command"]["state"], "failed");
    let detail = app.get(&format!("/api/clients/{a1}/models"), &alice).await.json;
    assert_eq!(detail["connected"], false);
    assert_eq!(detail["models"], json!([]));
    assert_eq!(detail["commands"][0]["state"], "failed");

    // A client that drops mid-pull fails its commands too.
    let conn = app.connect(&a1);
    let r = app
        .post(
            &format!("/api/clients/{a1}/models"),
            Some(&alice),
            json!({ "catalog_id": TEST_MODEL }),
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    app.state.live.unregister(&a1, conn.id);
    app.state.model_commands.on_connection_closed(conn.id);
    let detail = app.get(&format!("/api/clients/{a1}/models"), &alice).await.json;
    assert_eq!(detail["commands"][0]["state"], "failed");
    assert!(detail["commands"][0]["error"].as_str().unwrap().contains("resume"));
}

#[tokio::test]
async fn the_picker_lists_usable_models_on_clients_the_user_can_use() {
    let app = test_app(&[]).await;
    let admin = app.setup_admin().await;
    let alice = app.signup("alice").await;
    let bob = app.signup("bob").await;
    let a1 = app.enroll(&alice, "alice-pc").await;
    let b1 = app.enroll(&bob, "bob-pc").await;
    let _a = app.connect_with(
        &a1,
        vec![
            model("echo", Backend::Echo, ModelState::Loaded),
            model("ready", Backend::LlamaCpp, ModelState::Available),
            model("warm", Backend::LlamaCpp, ModelState::Loaded),
            model("pulling", Backend::LlamaCpp, ModelState::Downloading),
            model("broken", Backend::LlamaCpp, ModelState::Failed),
        ],
    );
    let _b = app.connect_with(&b1, vec![model("bobs", Backend::LlamaCpp, ModelState::Available)]);

    let list = app.get("/api/chat/models", &alice).await.json;
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["model"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["echo", "ready", "warm"], "{list}");
    assert_eq!(list[1]["model_name"], "ready model");
    assert_eq!(list[1]["client_name"], "alice-pc");
    assert_eq!(list[1]["backend"], "llama_cpp");
    let all = app.get("/api/chat/models", &admin).await.json;
    assert_eq!(all.as_array().unwrap().len(), 4, "the admin can use Bob's too");

    // Sending to a model that's still downloading is refused.
    let r = app
        .post(
            &format!("/api/conversations/{}/messages", new_id()),
            Some(&alice),
            json!({ "content": "hi", "parent_id": null, "target": { "kind": "client", "client_id": a1, "model": "pulling" } }),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert!(r.error().contains("still downloading"), "{}", r.error());

    // With INFERENCE_ECHO_MODEL off, echo isn't offered even if a client lists it.
    let app = test_app(&[("INFERENCE_ECHO_MODEL", "false")]).await;
    app.setup_admin().await;
    let alice = app.signup("alice").await;
    let a1 = app.enroll(&alice, "alice-pc").await;
    let _a = app.connect(&a1);
    assert_eq!(app.get("/api/chat/models", &alice).await.json, json!([]));
}

#[tokio::test]
async fn inference_echo_model_controls_echo_on_cards_and_in_chat() {
    for (on, ids) in [("true", vec!["echo", "ready"]), ("false", vec!["ready"])] {
        let app = test_app(&[("INFERENCE_ECHO_MODEL", on)]).await;
        app.setup_admin().await;
        let alice = app.signup("alice").await;
        let a1 = app.enroll(&alice, "alice-pc").await;
        let _a = app.connect_with(
            &a1,
            vec![
                model("echo", Backend::Echo, ModelState::Loaded),
                model("ready", Backend::LlamaCpp, ModelState::Available),
            ],
        );
        let ids_of = |list: &Value, key: &str| -> Vec<String> {
            list.as_array()
                .unwrap()
                .iter()
                .map(|m| m[key].as_str().unwrap().to_string())
                .collect()
        };
        let detail = app.get(&format!("/api/clients/{a1}/models"), &alice).await.json;
        assert_eq!(
            ids_of(&detail["models"], "id"),
            ids,
            "card, INFERENCE_ECHO_MODEL={on}: {detail}"
        );
        let list = app.get("/api/chat/models", &alice).await.json;
        assert_eq!(ids_of(&list, "model"), ids, "chat, INFERENCE_ECHO_MODEL={on}: {list}");
    }
}

#[tokio::test]
async fn loading_progress_reaches_the_reply_stream() {
    let app = test_app(&[]).await;
    app.setup_admin().await;
    let alice = app.signup("alice").await;
    let client = app.enroll(&alice, "alice-pc").await;
    let mut conn = app.connect(&client);
    let response = app
        .raw(Call {
            cookie: Some(&alice),
            body: Some(json!({ "content": "hello", "parent_id": null, "target": target(&client) })),
            ..app.call(Method::POST, &format!("/api/conversations/{}/messages", new_id()))
        })
        .await;
    let reader = tokio::spawn(sse_events(response));
    let (attempt, messages) = next_request(&mut conn).await;
    // Give the stream a moment to attach, so the progress is live, not in a snapshot.
    tokio::time::sleep(Duration::from_millis(50)).await;
    app.state
        .replies
        .on_client_event(conn.id, &attempt, ChatEvent::Progress(Stage::LoadingModel));
    app.state
        .replies
        .on_client_event(conn.id, &attempt, ChatEvent::Progress(Stage::Generating));
    echo(&app, &conn, &attempt, &messages);
    let events = reader.await.unwrap();
    let stages: Vec<&Value> = events
        .iter()
        .filter(|(name, _)| name == "progress")
        .map(|(_, data)| &data["stage"])
        .collect();
    assert_eq!(stages, vec![&json!("loading_model"), &json!("generating")]);
    assert_eq!(streamed_text(&events), "hello");
}
