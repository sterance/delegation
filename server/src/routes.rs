//! Every route, and what guards it.
//!
//! `/api` is default-deny: the session layer lets a request through without
//! a valid session only if its route is on [`PUBLIC_API_ROUTES`]. A new
//! route is protected unless someone deliberately adds it there, and
//! `tests.rs` checks every route in [`api_routes`] against the list, so a
//! route can't be added without that test seeing it.
//!
//! `/enroll` and `/ws` stay outside session auth (clients authenticate with
//! pairing codes and their keys), and so do the dashboard's static files,
//! which contain no data.

use crate::auth::{self, handlers as auth_api};
use crate::http::{self, ApiError, ApiResult};
use crate::{chat, clients, db, models, ws, AppState};
use axum::{
    extract::{MatchedPath, Request, State},
    handler::Handler,
    http::{header, Method},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{self, MethodRouter},
    Router,
};
use tower_http::services::{ServeDir, ServeFile};

/// `/api` routes reachable without a session, as (method, path under /api).
pub const PUBLIC_API_ROUTES: &[(&str, &str)] = &[
    ("GET", "/health"),
    ("GET", "/auth/status"),
    ("POST", "/auth/login"),
    ("POST", "/auth/signup"),
    ("POST", "/auth/setup"),
    ("GET", "/password-reset/:token"),
    ("POST", "/password-reset/:token"),
];

/// The `/api` router under construction, remembering every route added so
/// the protection test can try each one.
pub struct ApiRoutes {
    router: Router<AppState>,
    pub routes: Vec<(&'static str, &'static str)>,
}

impl ApiRoutes {
    fn new() -> Self {
        ApiRoutes {
            router: Router::new(),
            routes: Vec::new(),
        }
    }

    fn add(mut self, method: &'static str, path: &'static str, route: MethodRouter<AppState>) -> Self {
        self.routes.push((method, path));
        // Same path, another method: axum merges them.
        self.router = self.router.route(path, route);
        self
    }

    fn get<H: Handler<T, AppState>, T: 'static>(self, path: &'static str, handler: H) -> Self {
        self.add("GET", path, routing::get(handler))
    }

    fn post<H: Handler<T, AppState>, T: 'static>(self, path: &'static str, handler: H) -> Self {
        self.add("POST", path, routing::post(handler))
    }

    fn patch<H: Handler<T, AppState>, T: 'static>(self, path: &'static str, handler: H) -> Self {
        self.add("PATCH", path, routing::patch(handler))
    }

    fn delete<H: Handler<T, AppState>, T: 'static>(self, path: &'static str, handler: H) -> Self {
        self.add("DELETE", path, routing::delete(handler))
    }
}

/// Every `/api` route, before any layers.
pub fn api_routes() -> ApiRoutes {
    ApiRoutes::new()
        .get("/health", health)
        // auth
        .get("/auth/status", auth_api::status)
        .post("/auth/login", auth_api::login)
        .post("/auth/signup", auth_api::signup)
        .post("/auth/setup", auth_api::setup)
        .post("/auth/logout", auth_api::logout)
        .get("/me", auth_api::me)
        .post("/me/password", auth_api::change_password)
        .get("/password-reset/:token", auth_api::reset_info)
        .post("/password-reset/:token", auth_api::reset_password)
        // the admin
        .get("/admin/users", auth_api::list_users)
        .post("/admin/users/:id/reset-link", auth_api::create_reset_link)
        .patch("/admin/users/:id", auth_api::patch_user)
        .delete("/admin/users/:id", auth_api::delete_user)
        // clients
        .get("/clients", clients::list_clients)
        .patch("/clients/:client_id", clients::update_client)
        .delete("/clients/:client_id", clients::delete_client)
        .post("/pairing-codes", clients::create_pairing_code)
        .get("/pairing-codes/:code/status", clients::pairing_code_status)
        .get("/dashboard-config", clients::dashboard_config)
        // models on clients
        .get("/models/catalog", models::list_catalog)
        .get("/clients/:client_id/models", models::client_models)
        .post("/clients/:client_id/models", models::install)
        .delete("/clients/:client_id/models/:model_id", models::delete_model)
        .post(
            "/clients/:client_id/model-commands/:command_id/cancel",
            models::cancel_command,
        )
        // chat
        .get("/chat/models", chat::list_models)
        .get("/conversations", chat::list_conversations)
        .get("/conversations/:id", chat::get_conversation)
        .patch("/conversations/:id", chat::patch_conversation)
        .delete("/conversations/:id", chat::delete_conversation)
        .post("/conversations/:id/messages", chat::send_message)
        .get("/messages/:id/stream", chat::stream_message)
        .post("/messages/:id/cancel", chat::cancel_message)
        .post("/messages/:id/retry", chat::retry_message)
}

pub fn is_public(method: &Method, path: &str) -> bool {
    // HEAD is answered by GET handlers.
    let method = if method == Method::HEAD { "GET" } else { method.as_str() };
    PUBLIC_API_ROUTES.iter().any(|&(m, p)| m == method && p == path)
}

/// The session layer: default deny.
async fn require_session(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let path = req.extensions().get::<MatchedPath>().map(|p| {
        let p = p.as_str();
        p.strip_prefix("/api").unwrap_or(p).to_string()
    });
    if path.as_deref().is_some_and(|p| is_public(req.method(), p)) {
        return next.run(req).await;
    }
    let Some(token) = auth::read_session_cookie(req.headers(), &state.settings) else {
        return ApiError::unauthorized().into_response();
    };
    match state.auth.authenticate(&state.db, &state.settings, &token).await {
        Ok(Some(session)) => {
            req.extensions_mut().insert(session);
            next.run(req).await
        }
        Ok(None) => {
            // Expired or revoked: have the browser drop it.
            let mut response = ApiError::unauthorized().into_response();
            response
                .headers_mut()
                .insert(header::SET_COOKIE, auth::clear_cookie(&state.settings));
            response
        }
        Err(e) => ApiError::from(e).into_response(),
    }
}

/// `GET /api/health` (public): the server and its database are up. For the
/// deploy health check.
async fn health(State(state): State<AppState>) -> ApiResult<&'static str> {
    state.db.call(|c| db::ping(c)).await?;
    Ok("ok")
}

/// The whole application.
pub fn app(state: AppState) -> Router {
    let api = api_routes()
        .router
        // Unknown /api paths 404 here (after the session check, so they
        // reveal nothing) instead of falling through to index.html.
        .fallback(|| async { ApiError::not_found() })
        // Outermost last: no-store, then the Origin check, then sessions.
        .layer(middleware::from_fn_with_state(state.clone(), require_session))
        .layer(middleware::from_fn_with_state(state.clone(), http::require_same_origin))
        .layer(middleware::from_fn(http::no_store));

    // Any path that isn't a real file gets index.html, so deep links and
    // reloads of client-side routes load the app, which then routes itself.
    let frontend_dir = &state.settings.frontend_dir;
    let spa =
        ServeDir::new(frontend_dir).fallback(ServeFile::new(std::path::Path::new(frontend_dir).join("index.html")));

    Router::new()
        // Used by the client binary, so these stay at the root.
        .route("/enroll", routing::post(clients::enroll))
        .route("/ws", routing::get(ws::ws_upgrade))
        .nest("/api", api)
        // Fallback, not a route: only requests no route above matched ever
        // reach the static files, so the dashboard can't shadow the API.
        .fallback_service(spa)
        .layer(middleware::from_fn_with_state(state.clone(), http::security_headers))
        .with_state(state)
}
