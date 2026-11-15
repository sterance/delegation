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
        // Off by default; most tests chat with it.
        ("INFERENCE_ECHO_MODEL", "true"),
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
    test_app_tuned(extra, |_| {}).await
}

/// `tune` adjusts the parsed settings, for values below what `config.env`
/// allows (a queue timeout of milliseconds, say).
async fn test_app_tuned(extra: &[(&str, &str)], tune: impl FnOnce(&mut Settings)) -> TestApp {
    let mut settings = settings(extra);
    tune(&mut settings);
    start(db::open(":memory:", 3).unwrap(), settings)
}

/// What `serve` does with a database: recovers interrupted jobs, then
/// starts everything. Called again on the same database, it's a restart.
fn start(db: db::Db, settings: Settings) -> TestApp {
    let queue_timeout = settings.jobs.queue_timeout;
    let recovered = db.blocking(|c| crate::jobs::store::recover(c, queue_timeout)).unwrap();
    let live = Arc::new(Live::new(settings.metrics));
    let auth = Arc::new(Auth::new(&settings).unwrap());
    let settings = Arc::new(settings);
    let replies = ReplyRunner::new(
        db.clone(),
        live.clone(),
        settings.chat,
        settings.client.inference,
        settings.jobs,
    );
    for job in recovered.queued {
        replies.submit(job);
    }
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

    async fn put(&self, path: &str, cookie: &str, body: Value) -> Resp {
        self.send(Call {
            cookie: Some(cookie),
            body: Some(body),
            ..self.call(Method::PUT, path)
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

    /// The client's connection closes, as the WebSocket loop does it.
    fn disconnect(&self, client_id: &str, conn: &ConnHandle) {
        self.state.live.unregister(client_id, conn.id);
        self.state.replies.on_connection_closed(conn.id);
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
        settings: Default::default(),
        run: None,
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
    app.state
        .replies
        .on_client_event(conn.id, attempt, ChatEvent::Done(None));
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

/// The latest snapshot's text plus every chunk after it, starting again
/// when the reply is re-queued, as the dashboard does.
fn streamed_text(events: &[(String, Value)]) -> String {
    let mut text = String::new();
    for (name, data) in events {
        match name.as_str() {
            "snapshot" => text = data["content"].as_str().unwrap_or("").to_string(),
            "requeued" => text.clear(),
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
            .map(|seg| if seg.starts_with('{') { "x" } else { seg })
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
    let models = app.get("/api/chat/models", &alice).await.json["clients"].clone();
    assert_eq!(models.as_array().unwrap().len(), 1);
    assert_eq!(models[0]["client_id"], a1);
    assert_eq!(models[0]["client_name"], "Alice's PC");
    let admins = app.get("/api/chat/models", &admin).await.json;
    assert_eq!(admins["clients"].as_array().unwrap().len(), 2);
    assert_eq!(
        admins["pools"],
        json!([]),
        "the admin may use them directly, but neither is in the admin's pool"
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
    let app = test_app(&[("JOB_MAX_ATTEMPTS", "1")]).await;
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

    // The client dies mid-reply, on the reply's only attempt.
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
    app.disconnect(&client, &conn);
    let events = reader.await.unwrap();
    let orphan = reply_id(&events);
    assert_eq!(ending(&events).1["status"], "orphaned");
    assert_eq!(
        ending(&events).1["error"],
        "The client disconnected before the reply finished (1 attempt)."
    );

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

    let other =
        send(json!({ "content": "hi", "parent_id": null, "target": { "kind": "anyone", "model": "echo" } })).await;
    assert_eq!(
        other.status,
        StatusCode::BAD_REQUEST,
        "only client and pool targets exist"
    );
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

// ---------------- the job queue ----------------

fn pool() -> Value {
    json!({ "kind": "pool", "model": "echo" })
}

/// Sends `content` as a new conversation to `target`; returns the
/// conversation id and the task reading the reply's stream to its end.
async fn start_send(
    app: &TestApp,
    cookie: &str,
    content: &str,
    target: Value,
) -> (String, tokio::task::JoinHandle<Vec<(String, Value)>>) {
    let conversation = new_id();
    let response = app
        .raw(Call {
            cookie: Some(cookie),
            body: Some(json!({ "content": content, "parent_id": null, "target": target })),
            ..app.call(Method::POST, &format!("/api/conversations/{conversation}/messages"))
        })
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    (conversation, tokio::spawn(sse_events(response)))
}

/// The reply a conversation currently ends in.
async fn leaf(app: &TestApp, cookie: &str, conversation: &str) -> String {
    app.get(&format!("/api/conversations/{conversation}"), cookie)
        .await
        .json["active_leaf_id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn message(app: &TestApp, cookie: &str, conversation: &str, id: &str) -> Value {
    app.get(&format!("/api/conversations/{conversation}"), cookie)
        .await
        .json["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == id)
        .unwrap()
        .clone()
}

/// Waits for the writer to bring a reply's job to `status` (its message
/// must agree), and returns its attempts' outcomes.
async fn job_reaches(app: &TestApp, reply: &str, status: MessageStatus) -> Vec<String> {
    for _ in 0..300 {
        let id = reply.to_string();
        let (job, message, outcomes) = app
            .state
            .db
            .call(move |c| crate::jobs::store::job_state(c, &id))
            .await
            .unwrap();
        assert_eq!(job, message, "the job and its message disagree");
        if job == status {
            return outcomes;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the job never became {}", status.as_str());
}

/// Waits until the scheduler has told a queued reply where it stands.
async fn queue_of(app: &TestApp, reply: &str) -> Value {
    for _ in 0..300 {
        if let Some((snapshot, _)) = app.state.replies.attach(reply) {
            if let Some(queue) = &snapshot.queue {
                assert_eq!(snapshot.status, MessageStatus::Queued);
                return serde_json::to_value(queue).unwrap();
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the reply never got a place in the queue");
}

fn events_named<'a>(events: &'a [(String, Value)], name: &str) -> Vec<&'a Value> {
    events.iter().filter(|(n, _)| n == name).map(|(_, data)| data).collect()
}

/// Nothing is waiting to be written to this client's socket.
async fn nothing_sent(conn: &mut ConnHandle) {
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(conn.outbound.try_recv().is_err(), "the client was sent something");
}

fn chunk(app: &TestApp, conn: &ConnHandle, attempt: &str, text: &str) {
    app.state
        .replies
        .on_client_event(conn.id, attempt, ChatEvent::Chunk(text.into()));
}

fn done(app: &TestApp, conn: &ConnHandle, attempt: &str) {
    app.state
        .replies
        .on_client_event(conn.id, attempt, ChatEvent::Done(None));
}

fn fail(app: &TestApp, conn: &ConnHandle, attempt: &str, error: &str, code: Option<protocol::ChatErrorCode>) {
    app.state.replies.on_client_event(
        conn.id,
        attempt,
        ChatEvent::Error {
            error: error.into(),
            code,
        },
    );
}

/// An app with Alice and one connected client of hers.
async fn alice_with_a_client(extra: &[(&str, &str)]) -> (TestApp, String, String, ConnHandle) {
    let app = test_app(extra).await;
    app.setup_admin().await;
    let alice = app.signup("alice").await;
    let client = app.enroll(&alice, "alice-pc").await;
    let conn = app.connect(&client);
    (app, alice, client, conn)
}

/// An app with Alice and two connected clients of hers. The first has echo
/// loaded and the second only available, so a pooled job goes to the first
/// while it's free.
async fn alice_with_two_clients(extra: &[(&str, &str)]) -> (TestApp, String, [String; 2], [ConnHandle; 2]) {
    let app = test_app(extra).await;
    app.setup_admin().await;
    let alice = app.signup("alice").await;
    let warm = app.enroll(&alice, "warm-pc").await;
    let cold = app.enroll(&alice, "cold-pc").await;
    let warm_conn = app.connect(&warm);
    let cold_conn = app.connect_with(&cold, vec![model("echo", Backend::Echo, ModelState::Available)]);
    (app, alice, [warm, cold], [warm_conn, cold_conn])
}

#[tokio::test]
async fn a_directed_reply_waits_for_a_busy_client_then_runs() {
    let (app, alice, client, mut conn) = alice_with_a_client(&[]).await;

    // An idle client is assigned at once.
    let started = std::time::Instant::now();
    let (_, first) = start_send(&app, &alice, "first", target(&client)).await;
    let (first_attempt, first_messages) = next_request(&mut conn).await;
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "the queue added {:?}",
        started.elapsed()
    );

    // The client generates one reply at a time: the second waits for it.
    let (conv, second) = start_send(&app, &alice, "second", target(&client)).await;
    let reply = leaf(&app, &alice, &conv).await;
    assert_eq!(
        queue_of(&app, &reply).await,
        json!({ "position": 1, "waiting_for": "a_free_client", "client_label": "alice-pc" })
    );
    assert_eq!(
        job_reaches(&app, &reply, MessageStatus::Queued).await,
        Vec::<String>::new()
    );
    assert_eq!(message(&app, &alice, &conv, &reply).await["status"], "queued");
    nothing_sent(&mut conn).await;
    // The card's dot follows the reply being generated, not the one waiting.
    let card = app.get("/api/clients", &alice).await.json["clients"][0].clone();
    assert_eq!((&card["active_jobs"], &card["queued_for"]), (&json!(1), &json!(1)));

    // The first finishes: the second runs.
    echo(&app, &conn, &first_attempt, &first_messages);
    assert_eq!(ending(&first.await.unwrap()).1["status"], "completed");
    let (second_attempt, second_messages) = next_request(&mut conn).await;
    echo(&app, &conn, &second_attempt, &second_messages);
    let events = second.await.unwrap();
    assert_eq!(ending(&events).1["status"], "completed");
    assert_eq!(streamed_text(&events), "second");
    assert!(
        events_named(&events, "status")
            .iter()
            .any(|s| s["status"] == "queued" && s["queue"]["position"] == 1),
        "the stream heard where it stood: {events:?}"
    );
    let card = app.get("/api/clients", &alice).await.json["clients"][0].clone();
    assert_eq!((&card["active_jobs"], &card["queued_for"]), (&json!(0), &json!(0)));
}

#[tokio::test]
async fn pooled_replies_spread_across_clients_and_wait_when_all_are_busy() {
    let (app, alice, _, [mut warm, mut cold]) = alice_with_two_clients(&[]).await;

    // The first goes where the model is loaded; with that client busy, the
    // second goes to the other.
    let (_, first) = start_send(&app, &alice, "one", pool()).await;
    let (a1, m1) = next_request(&mut warm).await;
    let (_, second) = start_send(&app, &alice, "two", pool()).await;
    let (a2, m2) = next_request(&mut cold).await;

    // Both busy: the third waits for either.
    let (conv, third) = start_send(&app, &alice, "three", pool()).await;
    let reply = leaf(&app, &alice, &conv).await;
    assert_eq!(
        queue_of(&app, &reply).await,
        json!({ "position": 1, "waiting_for": "a_free_client", "client_label": null })
    );
    nothing_sent(&mut warm).await;
    nothing_sent(&mut cold).await;

    // The cold client frees up first, so that's where it goes.
    echo(&app, &cold, &a2, &m2);
    let (a3, m3) = next_request(&mut cold).await;
    echo(&app, &cold, &a3, &m3);
    echo(&app, &warm, &a1, &m1);
    for (reader, text) in [(first, "one"), (second, "two"), (third, "three")] {
        let events = reader.await.unwrap();
        assert_eq!(ending(&events).1["status"], "completed");
        assert_eq!(streamed_text(&events), text);
    }
    let answered = message(&app, &alice, &conv, &reply).await;
    assert_eq!(answered["client_label"], "cold-pc");
}

#[tokio::test]
async fn a_pooled_reply_moves_to_another_client_when_its_client_disconnects() {
    let (app, alice, [warm_id, _], [mut warm, mut cold]) = alice_with_two_clients(&[]).await;
    let (conv, reader) = start_send(&app, &alice, "tell me", pool()).await;
    let reply = leaf(&app, &alice, &conv).await;
    let (first, _) = next_request(&mut warm).await;
    chunk(&app, &warm, &first, "the first attempt's half");
    job_reaches(&app, &reply, MessageStatus::Running).await;

    // The answering client goes away mid-reply.
    app.disconnect(&warm_id, &warm);
    let (second, _) = next_request(&mut cold).await;
    assert_ne!(second, first);
    assert_eq!(
        job_reaches(&app, &reply, MessageStatus::Assigned).await,
        vec!["lost", "open"]
    );

    // It comes back and keeps talking about the attempt it lost: dropped.
    let warm_again = app.connect(&warm_id);
    for conn in [&warm, &warm_again] {
        chunk(&app, conn, &first, " stale");
        done(&app, conn, &first);
    }
    assert_eq!(message(&app, &alice, &conv, &reply).await["status"], "assigned");

    chunk(&app, &cold, &second, "the second attempt");
    done(&app, &cold, &second);
    let events = reader.await.unwrap();
    assert_eq!(ending(&events).1["status"], "completed");
    let requeued = events_named(&events, "requeued");
    assert_eq!(
        requeued,
        vec![&json!({
            "attempt": 2,
            "max_attempts": 3,
            "reason": "lost",
            "previous_client_label": "warm-pc",
        })]
    );
    // A queued status follows the requeue, then the fresh reply.
    let after: Vec<&str> = events
        .iter()
        .skip_while(|(name, _)| name != "requeued")
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(after[1], "status", "{after:?}");
    assert_eq!(
        events
            .iter()
            .skip_while(|(name, _)| name != "requeued")
            .nth(1)
            .unwrap()
            .1["status"],
        "queued"
    );

    // Only the second attempt's output is stored.
    let stored = message(&app, &alice, &conv, &reply).await;
    assert_eq!(
        (&stored["content"], &stored["client_label"], &stored["attempts"]),
        (&json!("the second attempt"), &json!("cold-pc"), &json!(2))
    );
    assert_eq!(
        job_reaches(&app, &reply, MessageStatus::Completed).await,
        vec!["lost", "completed"]
    );
    // Still nothing from the stale attempt, even now.
    chunk(&app, &warm_again, &first, " more");
    assert_eq!(
        message(&app, &alice, &conv, &reply).await["content"],
        "the second attempt"
    );
}

#[tokio::test]
async fn a_pinned_reply_waits_for_its_client_to_reconnect() {
    let (app, alice, client, mut conn) = alice_with_a_client(&[]).await;
    let (conv, reader) = start_send(&app, &alice, "hold on", target(&client)).await;
    let reply = leaf(&app, &alice, &conv).await;
    let (first, _) = next_request(&mut conn).await;
    chunk(&app, &conn, &first, "half");

    app.disconnect(&client, &conn);
    assert_eq!(
        queue_of(&app, &reply).await,
        json!({ "position": 1, "waiting_for": "a_client", "client_label": "alice-pc" })
    );
    let waiting = message(&app, &alice, &conv, &reply).await;
    assert_eq!(
        (&waiting["status"], &waiting["content"], &waiting["client_label"]),
        (&json!("queued"), &json!(""), &Value::Null)
    );

    // Back after a blip: the same client gets the next attempt.
    let mut conn = app.connect(&client);
    let (second, messages) = next_request(&mut conn).await;
    assert_ne!(second, first);
    echo(&app, &conn, &second, &messages);
    let events = reader.await.unwrap();
    assert_eq!(ending(&events).1["status"], "completed");
    assert_eq!(events_named(&events, "requeued")[0]["reason"], "lost");
    assert_eq!(streamed_text(&events), "hold on", "the lost half is gone");
    assert_eq!(
        job_reaches(&app, &reply, MessageStatus::Completed).await,
        vec!["lost", "completed"]
    );
}

#[tokio::test]
async fn a_reply_whose_client_never_returns_fails_at_the_deadline() {
    let app = test_app_tuned(&[], |s| s.jobs.queue_timeout = Duration::from_millis(300)).await;
    app.setup_admin().await;
    let alice = app.signup("alice").await;
    let client = app.enroll(&alice, "alice-pc").await;
    let mut conn = app.connect(&client);
    let (conv, reader) = start_send(&app, &alice, "anyone there?", target(&client)).await;
    let reply = leaf(&app, &alice, &conv).await;
    let (attempt, _) = next_request(&mut conn).await;
    chunk(&app, &conn, &attempt, "half");
    app.disconnect(&client, &conn);

    let events = reader.await.unwrap();
    let (_, end) = ending(&events);
    assert_eq!(end["status"], "failed");
    assert_eq!(end["error"], "alice-pc wasn't free (or connected) within 0 s.");
    assert_eq!(job_reaches(&app, &reply, MessageStatus::Failed).await, vec!["lost"]);

    // A pooled one says it differently.
    let mut conn = app.connect(&client);
    let (_, reader) = start_send(&app, &alice, "anyone at all?", pool()).await;
    let (attempt, _) = next_request(&mut conn).await;
    chunk(&app, &conn, &attempt, "half");
    app.disconnect(&client, &conn);
    assert_eq!(
        ending(&reader.await.unwrap()).1["error"],
        "No client offering echo was free within 0 s."
    );
}

#[tokio::test]
async fn an_idle_timeout_requeues_the_reply_instead_of_failing_it() {
    let (app, alice, client, mut conn) = alice_with_a_client(&[("CHAT_IDLE_TIMEOUT_MS", "150")]).await;
    let (conv, reader) = start_send(&app, &alice, "slow", target(&client)).await;
    let reply = leaf(&app, &alice, &conv).await;
    let (first, _) = next_request(&mut conn).await;

    // Silence: the client is told to stop, and the reply is sent again.
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), conn.outbound.recv()).await,
        Ok(Some(ServerMessage::ChatCancel { request_id })) if request_id == first
    ));
    let (second, messages) = next_request(&mut conn).await;
    assert_ne!(second, first);
    echo(&app, &conn, &second, &messages);
    let events = reader.await.unwrap();
    assert_eq!(ending(&events).1["status"], "completed");
    assert_eq!(events_named(&events, "requeued")[0]["reason"], "timed_out");
    assert_eq!(
        job_reaches(&app, &reply, MessageStatus::Completed).await,
        vec!["timed_out", "completed"]
    );
}

#[tokio::test]
async fn a_reply_is_orphaned_when_its_attempts_run_out_and_retry_gives_a_fresh_budget() {
    let (app, alice, client, mut conn) = alice_with_a_client(&[("JOB_MAX_ATTEMPTS", "2")]).await;
    let (conv, reader) = start_send(&app, &alice, "unlucky", target(&client)).await;
    let reply = leaf(&app, &alice, &conv).await;
    for _ in 0..2 {
        let (attempt, _) = next_request(&mut conn).await;
        chunk(&app, &conn, &attempt, "half");
        app.disconnect(&client, &conn);
        conn = app.connect(&client);
    }
    let events = reader.await.unwrap();
    let (_, end) = ending(&events);
    assert_eq!(end["status"], "orphaned");
    assert_eq!(
        end["error"],
        "The client disconnected before the reply finished (2 attempts)."
    );
    assert_eq!(
        events_named(&events, "requeued").len(),
        1,
        "only the first loss re-queued it"
    );
    nothing_sent(&mut conn).await;
    assert_eq!(
        job_reaches(&app, &reply, MessageStatus::Orphaned).await,
        vec!["lost", "lost"]
    );

    // Manual Retry: two more attempts, so it survives another loss.
    let response = app
        .raw(Call {
            cookie: Some(&alice),
            ..app.call(Method::POST, &format!("/api/messages/{reply}/retry"))
        })
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let reader = tokio::spawn(sse_events(response));
    let (attempt, _) = next_request(&mut conn).await;
    chunk(&app, &conn, &attempt, "half");
    app.disconnect(&client, &conn);
    let mut conn = app.connect(&client);
    let (attempt, messages) = next_request(&mut conn).await;
    echo(&app, &conn, &attempt, &messages);
    let events = reader.await.unwrap();
    assert_eq!(ending(&events).1["status"], "completed");
    assert_eq!(
        events_named(&events, "requeued"),
        vec![&json!({ "attempt": 4, "max_attempts": 4, "reason": "lost", "previous_client_label": "alice-pc" })]
    );
    let stored = message(&app, &alice, &conv, &reply).await;
    assert_eq!(
        (&stored["content"], &stored["attempts"]),
        (&json!("unlucky"), &json!(4))
    );
    assert_eq!(
        job_reaches(&app, &reply, MessageStatus::Completed).await,
        vec!["lost", "lost", "lost", "completed"]
    );
}

#[tokio::test]
async fn a_busy_client_refuses_without_using_an_attempt_until_it_refuses_too_often() {
    let (app, alice, client, mut conn) =
        alice_with_a_client(&[("JOB_REFUSED_BACKOFF_MS", "150"), ("JOB_MAX_REFUSALS", "3")]).await;
    let busy = Some(protocol::ChatErrorCode::Busy);

    // Refused once, then accepted after the backoff.
    let (conv, reader) = start_send(&app, &alice, "busy?", target(&client)).await;
    let reply = leaf(&app, &alice, &conv).await;
    let (first, _) = next_request(&mut conn).await;
    let refused_at = std::time::Instant::now();
    fail(&app, &conn, &first, "client busy", busy);
    let (second, messages) = next_request(&mut conn).await;
    assert!(
        refused_at.elapsed() >= Duration::from_millis(150),
        "placed again after {:?}",
        refused_at.elapsed()
    );
    echo(&app, &conn, &second, &messages);
    let events = reader.await.unwrap();
    assert_eq!(ending(&events).1["status"], "completed");
    assert_eq!(
        events_named(&events, "requeued"),
        vec![&json!({ "attempt": 1, "max_attempts": 3, "reason": "refused", "previous_client_label": "alice-pc" })],
        "still on its first attempt"
    );
    assert_eq!(message(&app, &alice, &conv, &reply).await["attempts"], 1);
    assert_eq!(
        job_reaches(&app, &reply, MessageStatus::Completed).await,
        vec!["refused", "completed"]
    );

    // Refused three times in a row: it fails, so a confused client can't
    // loop forever.
    let (conv, reader) = start_send(&app, &alice, "still busy?", target(&client)).await;
    let reply = leaf(&app, &alice, &conv).await;
    for _ in 0..3 {
        let (attempt, _) = next_request(&mut conn).await;
        fail(
            &app,
            &conn,
            &attempt,
            "busy with another model",
            Some(protocol::ChatErrorCode::BusyWithAnotherModel),
        );
    }
    let (_, end) = ending(&reader.await.unwrap());
    assert_eq!(end["status"], "failed");
    assert_eq!(
        end["error"],
        "Refused 3 times in a row; the last time: busy with another model"
    );
    assert_eq!(
        job_reaches(&app, &reply, MessageStatus::Failed).await,
        vec!["refused", "refused", "refused"]
    );
    assert_eq!(message(&app, &alice, &conv, &reply).await["attempts"], 0);
    nothing_sent(&mut conn).await;
}

#[tokio::test]
async fn errors_without_a_code_fail_the_reply_and_retry_keeps_its_target() {
    let (app, alice, [warm_id, _], [mut warm, mut cold]) =
        alice_with_two_clients(&[("JOB_REFUSED_BACKOFF_MS", "100")]).await;

    // An older client sends no code: a failure, as it always was.
    let (conv, reader) = start_send(&app, &alice, "break", pool()).await;
    let reply = leaf(&app, &alice, &conv).await;
    let (attempt, _) = next_request(&mut warm).await;
    fail(&app, &warm, &attempt, "llama-server crashed", None);
    let events = reader.await.unwrap();
    let (_, end) = ending(&events);
    assert_eq!(
        (&end["status"], &end["error"]),
        (&json!("failed"), &json!("llama-server crashed"))
    );
    assert!(events_named(&events, "requeued").is_empty());
    assert_eq!(job_reaches(&app, &reply, MessageStatus::Failed).await, vec!["failed"]);
    nothing_sent(&mut cold).await;

    // Retry with no body: the job's original target, so a pooled reply
    // retries pooled, wherever there's a client.
    app.disconnect(&warm_id, &warm);
    let response = app
        .raw(Call {
            cookie: Some(&alice),
            ..app.call(Method::POST, &format!("/api/messages/{reply}/retry"))
        })
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let reader = tokio::spawn(sse_events(response));
    let (attempt, messages) = next_request(&mut cold).await;
    echo(&app, &cold, &attempt, &messages);
    assert_eq!(ending(&reader.await.unwrap()).1["status"], "completed");
    assert_eq!(message(&app, &alice, &conv, &reply).await["client_label"], "cold-pc");

    // "The model isn't here" from one client of a pool: another may have
    // it, so that's a refusal. For a pinned reply it's a failure.
    let mut warm = app.connect(&warm_id);
    let unavailable = Some(protocol::ChatErrorCode::ModelUnavailable);
    let (_, reader) = start_send(&app, &alice, "pooled", pool()).await;
    let (attempt, _) = next_request(&mut warm).await;
    fail(&app, &warm, &attempt, "no such model", unavailable);
    let (attempt, messages) = next_request(&mut cold).await;
    echo(&app, &cold, &attempt, &messages);
    let events = reader.await.unwrap();
    assert_eq!(ending(&events).1["status"], "completed");
    assert_eq!(events_named(&events, "requeued")[0]["reason"], "refused");

    let (_, reader) = start_send(&app, &alice, "pinned", target(&warm_id)).await;
    let (attempt, _) = next_request(&mut warm).await;
    fail(&app, &warm, &attempt, "no such model", unavailable);
    let (_, end) = ending(&reader.await.unwrap());
    assert_eq!(
        (&end["status"], &end["error"]),
        (&json!("failed"), &json!("no such model"))
    );
}

#[tokio::test]
async fn stopping_or_deleting_a_queued_reply_sends_the_client_nothing() {
    let (app, alice, client, mut conn) = alice_with_a_client(&[]).await;
    let (_, _first) = start_send(&app, &alice, "busy with this", target(&client)).await;
    let _ = next_request(&mut conn).await;

    // Stop while queued.
    let (conv, reader) = start_send(&app, &alice, "never mind", target(&client)).await;
    let reply = leaf(&app, &alice, &conv).await;
    queue_of(&app, &reply).await;
    let stopped = app
        .post(&format!("/api/messages/{reply}/cancel"), Some(&alice), json!({}))
        .await;
    assert_eq!(stopped.status, StatusCode::OK);
    assert_eq!(stopped.json["status"], "cancelled");
    assert_eq!(ending(&reader.await.unwrap()).1["status"], "cancelled");
    assert!(job_reaches(&app, &reply, MessageStatus::Cancelled).await.is_empty());
    nothing_sent(&mut conn).await;

    // Deleting the conversation while its reply is queued does the same.
    let (conv, reader) = start_send(&app, &alice, "or this", pool()).await;
    let reply = leaf(&app, &alice, &conv).await;
    queue_of(&app, &reply).await;
    assert_eq!(
        app.delete(&format!("/api/conversations/{conv}"), &alice).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(ending(&reader.await.unwrap()).1["status"], "cancelled");
    nothing_sent(&mut conn).await;
    let jobs: i64 = app
        .state
        .db
        .call(|c| Ok(c.query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(jobs, 2, "the deleted conversation took its job with it");
    assert_eq!(app.state.replies.scheduler().queued_len(), 0);
}

#[tokio::test]
async fn deleting_a_client_fails_the_replies_waiting_for_it() {
    let (app, alice, client, mut conn) = alice_with_a_client(&[]).await;
    let (_, running) = start_send(&app, &alice, "running", target(&client)).await;
    let _ = next_request(&mut conn).await;
    let (conv, waiting) = start_send(&app, &alice, "waiting", target(&client)).await;
    let reply = leaf(&app, &alice, &conv).await;
    queue_of(&app, &reply).await;

    assert_eq!(
        app.delete(&format!("/api/clients/{client}"), &alice).await.status,
        StatusCode::NO_CONTENT
    );
    for reader in [running, waiting] {
        let (_, end) = ending(&reader.await.unwrap());
        assert_eq!(
            (&end["status"], &end["error"]),
            (&json!("failed"), &json!("That client was deleted."))
        );
    }
    // With its client gone there's nothing to default a Retry to.
    let retry = app
        .post(&format!("/api/messages/{reply}/retry"), Some(&alice), json!({}))
        .await;
    assert_eq!(retry.status, StatusCode::BAD_REQUEST);
    assert!(retry.error().contains("no longer exists"), "{}", retry.error());
}

#[tokio::test]
async fn a_restart_requeues_an_active_reply_which_completes_once_a_client_connects() {
    for (max_attempts, survives) in [("3", true), ("1", false)] {
        let (app, alice, client, mut conn) = alice_with_a_client(&[("JOB_MAX_ATTEMPTS", max_attempts)]).await;
        let (conv, _reader) = start_send(&app, &alice, "mid-reply", target(&client)).await;
        let reply = leaf(&app, &alice, &conv).await;
        let (attempt, _) = next_request(&mut conn).await;
        chunk(&app, &conn, &attempt, "half");
        job_reaches(&app, &reply, MessageStatus::Running).await;
        // The text so far is flushed before the server goes down.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // The server restarts: the same database, nothing in memory.
        let app = start(app.state.db.clone(), (*app.state.settings).clone());
        let stored = message(&app, &alice, &conv, &reply).await;
        if !survives {
            assert_eq!(
                (&stored["status"], &stored["content"], &stored["error"]),
                (
                    &json!("orphaned"),
                    &json!("half"),
                    &json!("Interrupted by a server restart")
                )
            );
            assert_eq!(
                job_reaches(&app, &reply, MessageStatus::Orphaned).await,
                vec!["interrupted"]
            );
            continue;
        }
        assert_eq!(
            (&stored["status"], &stored["content"]),
            (&json!("queued"), &json!("")),
            "back in the queue, starting again"
        );
        assert_eq!(
            job_reaches(&app, &reply, MessageStatus::Queued).await,
            vec!["interrupted"]
        );
        assert_eq!(app.state.replies.scheduler().queued_len(), 1);

        // Its client reconnects a few seconds later, and the reply runs.
        let stream = app
            .raw(Call {
                cookie: Some(&alice),
                ..app.call(Method::GET, &format!("/api/messages/{reply}/stream"))
            })
            .await;
        let reader = tokio::spawn(sse_events(stream));
        let mut conn = app.connect(&client);
        let (second, messages) = next_request(&mut conn).await;
        assert_ne!(second, attempt);
        echo(&app, &conn, &second, &messages);
        let events = reader.await.unwrap();
        assert_eq!(ending(&events).1["status"], "completed");
        assert_eq!(streamed_text(&events), "mid-reply");
        assert_eq!(
            job_reaches(&app, &reply, MessageStatus::Completed).await,
            vec!["interrupted", "completed"]
        );
        assert_eq!(message(&app, &alice, &conv, &reply).await["attempts"], 2);
    }
}

#[tokio::test]
async fn pool_targets_need_a_client_and_the_queue_has_a_limit() {
    let app = test_app(&[("JOB_QUEUE_MAX", "1")]).await;
    let admin = app.setup_admin().await;
    let alice = app.signup("alice").await;
    let bob = app.signup("bob").await;
    let bobs = app.enroll(&bob, "bob-pc").await;
    let _bob_conn = app.connect(&bobs);
    let send = |cookie: String, conversation: String, target: Value| {
        let app = &app;
        async move {
            app.post(
                &format!("/api/conversations/{conversation}/messages"),
                Some(&cookie),
                json!({ "content": "hi", "parent_id": null, "target": target }),
            )
            .await
        }
    };

    // Nobody in Alice's pool offers the model: 409, and nothing is created.
    let conversation = new_id();
    let none = send(alice.clone(), conversation.clone(), pool()).await;
    assert_eq!(none.status, StatusCode::CONFLICT);
    assert_eq!(none.error(), "None of your clients offer echo right now.");
    assert_eq!(
        app.get(&format!("/api/conversations/{conversation}"), &alice)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // Bob's client doesn't count for the admin either: the admin may send
    // to it directly, but it isn't in the admin's pool.
    assert_eq!(send(admin.clone(), new_id(), pool()).await.status, StatusCode::CONFLICT);

    let client = app.enroll(&alice, "alice-pc").await;
    let mut conn = app.connect_with(
        &client,
        vec![
            model("echo", Backend::Echo, ModelState::Loaded),
            model("pulling", Backend::LlamaCpp, ModelState::Downloading),
        ],
    );
    let downloading = send(alice.clone(), new_id(), json!({ "kind": "pool", "model": "pulling" })).await;
    assert_eq!(downloading.status, StatusCode::CONFLICT, "not usable yet");

    // One reply generating, one waiting: the queue (of one) is full.
    let (_, _running) = start_send(&app, &alice, "one", pool()).await;
    let _ = next_request(&mut conn).await;
    let (conv, _waiting) = start_send(&app, &alice, "two", pool()).await;
    queue_of(&app, &leaf(&app, &alice, &conv).await).await;
    let full = send(alice.clone(), new_id(), pool()).await;
    assert_eq!(full.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(full.headers.contains_key(header::RETRY_AFTER));
    assert!(full.error().contains("queue is full"), "{}", full.error());
}

#[tokio::test]
async fn the_reply_limits_count_queued_replies() {
    let (app, alice, client, mut conn) = alice_with_a_client(&[("CHAT_MAX_ACTIVE_REPLIES_PER_USER", "2")]).await;
    let (_, _running) = start_send(&app, &alice, "one", target(&client)).await;
    let _ = next_request(&mut conn).await;
    let (conv, _waiting) = start_send(&app, &alice, "two", pool()).await;
    let reply = leaf(&app, &alice, &conv).await;
    queue_of(&app, &reply).await;

    // Two replies in progress (one of them only queued) is the limit.
    let third = app
        .post(
            &format!("/api/conversations/{}/messages", new_id()),
            Some(&alice),
            json!({ "content": "three", "parent_id": null, "target": pool() }),
        )
        .await;
    assert_eq!(third.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(third.error().contains("too many replies"), "{}", third.error());
    // And a conversation whose reply is queued can't take another message.
    let again = app
        .post(
            &format!("/api/conversations/{conv}/messages"),
            Some(&alice),
            json!({ "content": "again", "parent_id": reply, "target": pool() }),
        )
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_pooled_job_never_lands_on_someone_elses_client() {
    let app = test_app(&[]).await;
    let admin = app.setup_admin().await;
    let bob = app.signup("bob").await;
    let admins = app.enroll(&admin, "admin-pc").await;
    let bobs = app.enroll(&bob, "bob-pc").await;
    let mut admin_conn = app.connect(&admins);
    let mut bob_conn = app.connect(&bobs);

    // The admin's own client is busy and Bob's is idle. The admin's pooled
    // reply waits for the admin's client all the same.
    let (_, first) = start_send(&app, &admin, "one", pool()).await;
    let (a1, m1) = next_request(&mut admin_conn).await;
    let (conv, second) = start_send(&app, &admin, "two", pool()).await;
    let reply = leaf(&app, &admin, &conv).await;
    assert_eq!(queue_of(&app, &reply).await["waiting_for"], "a_free_client");
    nothing_sent(&mut bob_conn).await;

    // A reply sent straight to Bob's client does go there.
    let (_, direct) = start_send(&app, &admin, "direct", target(&bobs)).await;
    let (a3, m3) = next_request(&mut bob_conn).await;
    echo(&app, &bob_conn, &a3, &m3);
    assert_eq!(ending(&direct.await.unwrap()).1["status"], "completed");

    echo(&app, &admin_conn, &a1, &m1);
    assert_eq!(ending(&first.await.unwrap()).1["status"], "completed");
    let (a2, m2) = next_request(&mut admin_conn).await;
    echo(&app, &admin_conn, &a2, &m2);
    assert_eq!(ending(&second.await.unwrap()).1["status"], "completed");
    nothing_sent(&mut bob_conn).await;
}

#[tokio::test]
async fn the_jobs_api_lists_a_users_own_jobs_without_message_content() {
    let app = test_app(&[]).await;
    let admin = app.setup_admin().await;
    let alice = app.signup("alice").await;
    let bob = app.signup("bob").await;
    let alices = app.enroll(&alice, "alice-pc").await;
    let bobs = app.enroll(&bob, "bob-pc").await;
    let mut alice_conn = app.connect(&alices);
    let mut bob_conn = app.connect(&bobs);

    // Alice: one finished (after losing its first attempt), one generating.
    let (finished_conv, reader) = start_send(&app, &alice, "alice's secret question", pool()).await;
    let (attempt, _) = next_request(&mut alice_conn).await;
    app.disconnect(&alices, &alice_conn);
    let mut alice_conn = app.connect(&alices);
    let (attempt2, _) = next_request(&mut alice_conn).await;
    assert_ne!(attempt, attempt2);
    chunk(&app, &alice_conn, &attempt2, "alice's secret answer");
    done(&app, &alice_conn, &attempt2);
    assert_eq!(ending(&reader.await.unwrap()).1["status"], "completed");
    let (_, _running) = start_send(&app, &alice, "another secret", target(&alices)).await;
    let _ = next_request(&mut alice_conn).await;
    // Bob: one finished.
    let (_, reader) = start_send(&app, &bob, "bob's secret", target(&bobs)).await;
    let (attempt, messages) = next_request(&mut bob_conn).await;
    echo(&app, &bob_conn, &attempt, &messages);
    assert_eq!(ending(&reader.await.unwrap()).1["status"], "completed");

    let mine = app.get("/api/jobs", &alice).await;
    assert_eq!(mine.status, StatusCode::OK);
    let jobs = mine.json["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 2, "only her own: {}", mine.json);
    // Newest first.
    assert_eq!(
        (
            &jobs[0]["status"],
            &jobs[0]["target"]["kind"],
            &jobs[0]["target"]["client_label"]
        ),
        (&json!("assigned"), &json!("client"), &json!("alice-pc"))
    );
    let finished = &jobs[1];
    assert_eq!(
        (
            &finished["kind"],
            &finished["status"],
            &finished["model"],
            &finished["target"]["kind"],
            &finished["attempts"],
            &finished["max_attempts"],
            &finished["conversation_id"],
            &finished["owner"]["username"],
        ),
        (
            &json!("chat_reply"),
            &json!("completed"),
            &json!("echo"),
            &json!("pool"),
            &json!(2),
            &json!(3),
            &json!(finished_conv),
            &json!("alice"),
        )
    );
    let history = finished["history"].as_array().unwrap();
    let outcomes: Vec<&Value> = history.iter().map(|a| &a["outcome"]).collect();
    assert_eq!(outcomes, vec![&json!("lost"), &json!("completed")]);
    assert_eq!(
        (&history[1]["number"], &history[1]["client_label"], &history[1]["model"]),
        (&json!(2), &json!("alice-pc"), &json!("echo"))
    );
    assert!(history[1]["first_output_at"].is_string() && history[1]["ended_at"].is_string());
    assert!(finished["queue_wait_ms"].is_u64() && finished["run_ms"].is_u64());

    // Filters.
    let active = app.get("/api/jobs?status=active", &alice).await.json;
    assert_eq!(active["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(active["jobs"][0]["status"], "assigned");
    let recent = app.get("/api/jobs?status=recent&limit=50", &alice).await.json;
    assert_eq!(recent["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(recent["jobs"][0]["id"], finished["id"]);
    assert_eq!(
        app.get("/api/jobs?status=sideways", &alice).await.status,
        StatusCode::BAD_REQUEST
    );

    // The admin sees their own by default, everyone's with all=1; nobody
    // else may ask for that.
    assert_eq!(app.get("/api/jobs", &admin).await.json["jobs"], json!([]));
    let everyone = app.get("/api/jobs?all=1", &admin).await;
    assert_eq!(everyone.json["jobs"].as_array().unwrap().len(), 3);
    assert_eq!(app.get("/api/jobs?all=1", &bob).await.status, StatusCode::FORBIDDEN);

    // One job: its owner and the admin, and a 404 for anyone else.
    let one = format!("/api/jobs/{}", finished["id"].as_str().unwrap());
    assert_eq!(app.get(&one, &alice).await.json["id"], finished["id"]);
    assert_eq!(app.get(&one, &admin).await.status, StatusCode::OK);
    assert_eq!(app.get(&one, &bob).await.status, StatusCode::NOT_FOUND);
    assert_eq!(app.get("/api/jobs/nope", &alice).await.status, StatusCode::NOT_FOUND);

    // Metadata only: no message text anywhere, for anyone.
    for body in [&mine.json, &everyone.json] {
        let text = body.to_string();
        assert!(!text.contains("secret"), "message content leaked: {text}");
    }
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

    let both = app.get("/api/chat/models", &alice).await.json;
    let list = &both["clients"];
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
    // "Any of my clients": one entry per model her pool offers.
    assert_eq!(
        both["pools"],
        json!([
            { "model": "echo", "model_name": "echo model", "clients": 1, "loaded_on": 1 },
            { "model": "ready", "model_name": "ready model", "clients": 1, "loaded_on": 0 },
            { "model": "warm", "model_name": "warm model", "clients": 1, "loaded_on": 1 },
        ])
    );
    let all = app.get("/api/chat/models", &admin).await.json;
    assert_eq!(
        all["clients"].as_array().unwrap().len(),
        4,
        "the admin can use Bob's too"
    );
    assert_eq!(all["pools"], json!([]), "but pools only their own clients");

    // A second client of Alice's with the same model joins the pool.
    let a2 = app.enroll(&alice, "alice-laptop").await;
    let _a2 = app.connect_with(&a2, vec![model("ready", Backend::LlamaCpp, ModelState::Loaded)]);
    let pools = app.get("/api/chat/models", &alice).await.json["pools"].clone();
    assert_eq!(
        pools[1],
        json!({ "model": "ready", "model_name": "ready model", "clients": 2, "loaded_on": 1 })
    );

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
    assert_eq!(
        app.get("/api/chat/models", &alice).await.json,
        json!({ "pools": [], "clients": [] })
    );
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
        assert_eq!(
            ids_of(&list["clients"], "model"),
            ids,
            "chat, INFERENCE_ECHO_MODEL={on}: {list}"
        );
        assert_eq!(
            ids_of(&list["pools"], "model"),
            ids,
            "pools, INFERENCE_ECHO_MODEL={on}: {list}"
        );
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

#[tokio::test]
async fn the_runtime_and_model_settings_are_set_through_commands() {
    let app = test_app(&[]).await;
    app.setup_admin().await;
    let alice = app.signup("alice").await;
    let bob = app.signup("bob").await;
    let a1 = app.enroll(&alice, "alice-pc").await;
    let mut conn = app.connect_with(&a1, vec![model("qwen", Backend::LlamaCpp, ModelState::Loaded)]);

    let r = app
        .post(
            &format!("/api/clients/{a1}/runtime"),
            Some(&alice),
            json!({ "variant": "cuda-13" }),
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{:?}", r.json);
    assert_eq!(r.json["kind"], "select_runtime");
    let (_, command) = next_command(&mut conn).await;
    assert_eq!(
        command,
        ModelCommandKind::SelectRuntime {
            variant: "cuda-13".into()
        }
    );
    for bad in [
        json!({ "variant": "../x" }),
        json!({ "variant": "" }),
        json!({ "variant": "CUDA" }),
    ] {
        let r = app.post(&format!("/api/clients/{a1}/runtime"), Some(&alice), bad).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
    }

    let r = app
        .put(
            &format!("/api/clients/{a1}/models/qwen/settings"),
            &alice,
            json!({ "gpu_layers": 20, "context_length": null }),
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{:?}", r.json);
    assert_eq!(
        (&r.json["kind"], &r.json["model_name"]),
        (&json!("configure"), &json!("qwen model"))
    );
    let (_, command) = next_command(&mut conn).await;
    assert_eq!(
        command,
        ModelCommandKind::Configure {
            model_id: "qwen".into(),
            settings: protocol::ModelSettings {
                gpu_layers: protocol::GpuLayers::Count(20),
                context_length: None,
            },
        }
    );
    for bad in [
        json!({ "gpu_layers": "lots" }),
        json!({ "gpu_layers": "auto", "context_length": 5 }),
        json!({ "gpu_layers": "auto", "url": "x" }),
    ] {
        let r = app
            .put(&format!("/api/clients/{a1}/models/qwen/settings"), &alice, bad.clone())
            .await;
        assert!(r.status.is_client_error(), "{bad}: {:?}", r.status);
    }
    assert!(conn.outbound.try_recv().is_err());

    // Only for those who can manage the client.
    let r = app
        .post(
            &format!("/api/clients/{a1}/runtime"),
            Some(&bob),
            json!({ "variant": "auto" }),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = app
        .put(
            &format!("/api/clients/{a1}/models/qwen/settings"),
            &bob,
            json!({ "gpu_layers": "off" }),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(conn.outbound.try_recv().is_err());

    // An older client answers `unsupported`; the command shows that.
    let r = app
        .post(
            &format!("/api/clients/{a1}/runtime"),
            Some(&alice),
            json!({ "variant": "auto" }),
        )
        .await;
    let (command_id, _) = next_command(&mut conn).await;
    assert_eq!(r.json["command_id"], command_id);
    app.state.model_commands.on_status(
        conn.id,
        &command_id,
        CommandState::Unsupported,
        None,
        Some("this client doesn't support that command".into()),
    );
    let detail = app.get(&format!("/api/clients/{a1}/models"), &alice).await.json;
    assert_eq!(detail["commands"][0]["state"], "unsupported");
}

#[tokio::test]
async fn models_needing_a_newer_runtime_are_not_installed_on_older_clients() {
    let dir = std::env::temp_dir().join(format!("delegation-catalog-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("catalog.json");
    std::fs::write(
        &path,
        json!([{
            "id": "future-model",
            "name": "Future model",
            "url": "https://example.com/future.gguf",
            "sha256": "ab".repeat(32),
            "size_bytes": 10,
            "context": 8192,
            "min_runtime_version": "b99999"
        }])
        .to_string(),
    )
    .unwrap();
    let app = test_app(&[("MODEL_CATALOG_PATH", path.to_str().unwrap())]).await;
    let _ = std::fs::remove_dir_all(&dir);
    app.setup_admin().await;
    let alice = app.signup("alice").await;
    let a1 = app.enroll(&alice, "alice-pc").await;
    let mut conn = app.connect(&a1);
    let catalog = app.get("/api/models/catalog", &alice).await.json;
    assert_eq!(catalog[1]["id"], "future-model");
    assert_eq!(catalog[1]["min_runtime_version"], "b99999");

    app.state.live.set_inventory(
        &a1,
        conn.id,
        Inventory {
            models: vec![],
            storage: Storage::default(),
            runtimes: vec![protocol::RuntimeInfo {
                backend: Backend::LlamaCpp,
                version: "b12110".into(),
                variant: "cpu".into(),
                state: protocol::RuntimeState::Ready,
                selected: true,
                automatic: true,
                devices: vec![],
                error: None,
            }],
        },
    );
    let r = app
        .post(
            &format!("/api/clients/{a1}/models"),
            Some(&alice),
            json!({ "catalog_id": "future-model" }),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert!(
        r.json["error"].as_str().unwrap().contains("needs llama.cpp b99999"),
        "{:?}",
        r.json
    );
    assert!(conn.outbound.try_recv().is_err(), "nothing was sent");
    // The test model has no minimum.
    let r = app
        .post(
            &format!("/api/clients/{a1}/models"),
            Some(&alice),
            json!({ "catalog_id": TEST_MODEL }),
        )
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
}
