//! `/api/auth/*`, `/api/me*`, `/api/password-reset/*` and `/api/admin/users*`.

use super::{
    clear_cookie, new_session_token, random_token, session_cookie, sha256_hex, store, validate_password,
    validate_username, AdminUser, CurrentSession, CurrentUser, Role, User,
};
use crate::db::ts;
use crate::http::{user_agent, ApiError, ApiJson, ApiResult, ClientIp};
use crate::{log, AppState};
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

const BAD_CREDENTIALS: &str = "Incorrect username or password.";

#[derive(Serialize)]
pub struct StatusResponse {
    /// No admin yet: the dashboard sends everyone to /setup.
    setup_required: bool,
    /// Whether the sign-up form is offered: `AUTH_SIGNUP_ENABLED`, and only
    /// once the admin exists.
    signup_enabled: bool,
    password_min_len: usize,
    password_max_len: usize,
    /// For "Remember me for N days".
    remember_days: u64,
}

/// `GET /api/auth/status` (public): what the login page should show.
pub async fn status(State(state): State<AppState>) -> ApiResult<Json<StatusResponse>> {
    let admin_exists = state.db.call(|c| store::admin_exists(c)).await?;
    let a = &state.settings.auth;
    Ok(Json(StatusResponse {
        setup_required: !admin_exists,
        signup_enabled: a.signup_enabled && admin_exists,
        password_min_len: a.password_min_len,
        password_max_len: a.password_max_len,
        remember_days: a.remember_days,
    }))
}

#[derive(Deserialize)]
pub struct Credentials {
    username: String,
    password: String,
    #[serde(default)]
    remember: bool,
}

/// Creates a session for `user` and returns its `Set-Cookie` value.
async fn start_session(
    state: &AppState,
    user: &User,
    remember: bool,
    headers: &HeaderMap,
    ip: &str,
) -> ApiResult<HeaderValue> {
    let a = &state.settings.auth;
    let lifetime = if remember {
        a.session_absolute_timeout
    } else {
        a.session_short_timeout
    };
    let (token, token_hash) = new_session_token();
    let user_id = user.id.clone();
    let ua = user_agent(headers);
    let ip = ip.to_string();
    let expires_at = SystemTime::now() + lifetime;
    state
        .db
        .call(move |c| {
            store::insert_session(
                c,
                store::NewSession {
                    token_hash: &token_hash,
                    user_id: &user_id,
                    expires_at,
                    persistent: remember,
                    user_agent: ua.as_deref(),
                    ip: &ip,
                },
            )
        })
        .await?;
    // Remember me: a cookie that outlives the browser, for as long as the
    // session. Otherwise a browser-session cookie, and the session itself
    // ends after SESSION_SHORT_TIMEOUT_HOURS regardless.
    Ok(session_cookie(
        &state.settings,
        &token,
        remember.then_some(a.session_absolute_timeout),
    ))
}

fn signed_in(status: StatusCode, user: User, cookie: HeaderValue) -> Response {
    let mut response = (status, Json(user)).into_response();
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    response
}

fn user_key(username: &str, ip: &str) -> String {
    format!("user:{}|{ip}", username.to_ascii_lowercase())
}

fn ip_key(ip: &str) -> String {
    format!("ip:{ip}")
}

/// Refuses while this username+IP, or this IP, has failed too often.
fn check_login_limits(state: &AppState, username: &str, ip: &str) -> ApiResult<()> {
    let a = &state.settings.auth;
    let limits = &state.auth.login_failures;
    limits
        .check(&user_key(username, ip), a.login_max_failures)
        .and_then(|()| limits.check(&ip_key(ip), a.login_max_failures_per_ip))
        .map_err(ApiError::rate_limited)
}

fn record_login_failure(state: &AppState, username: &str, ip: &str) {
    state.auth.login_failures.record(&user_key(username, ip));
    state.auth.login_failures.record(&ip_key(ip));
}

/// `POST /api/auth/login` (public). "No such user", "wrong password" and
/// "disabled" all get the same answer after the same amount of work.
pub async fn login(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    ApiJson(body): ApiJson<Credentials>,
) -> ApiResult<Response> {
    let username = body.username.trim().to_string();
    check_login_limits(&state, &username, &ip)?;
    // Bounds hashing cost; says nothing about the account.
    if body.password.chars().count() > state.settings.auth.password_max_len {
        record_login_failure(&state, &username, &ip);
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, BAD_CREDENTIALS));
    }
    let name = username.clone();
    let row = state.db.call(move |c| store::find_by_username(c, &name)).await?;
    let verified = state
        .auth
        .hasher
        .verify(row.as_ref().map(|r| r.password_hash.clone()), body.password.clone())
        .await?;
    let Some(row) = row.filter(|r| verified.ok && !r.disabled) else {
        record_login_failure(&state, &username, &ip);
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, BAD_CREDENTIALS));
    };
    state.auth.login_failures.clear(&user_key(&username, &ip));
    if verified.needs_rehash {
        let hash = state.auth.hasher.hash(body.password).await?;
        let id = row.user.id.clone();
        state.db.call(move |c| store::replace_hash(c, &id, &hash)).await?;
        log(format!(
            "[auth] rehashed {}'s password with the current parameters",
            row.user.username
        ));
    }
    let cookie = start_session(&state, &row.user, body.remember, &headers, &ip).await?;
    log(format!("[auth] {} signed in from {ip}", row.user.username));
    Ok(signed_in(StatusCode::OK, row.user, cookie))
}

/// `POST /api/auth/signup` (public): a new `user` account, signed in.
pub async fn signup(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    ApiJson(body): ApiJson<Credentials>,
) -> ApiResult<Response> {
    let a = &state.settings.auth;
    if !a.signup_enabled {
        return Err(ApiError::forbidden("Sign-up is turned off on this site."));
    }
    // Nobody can claim the site before the admin exists.
    if !state.db.call(|c| store::admin_exists(c)).await? {
        return Err(ApiError::forbidden("This site hasn't been set up yet."));
    }
    state
        .auth
        .signups
        .check(&ip, a.signup_max_per_ip)
        .map_err(ApiError::rate_limited)?;
    let username = body.username.trim().to_string();
    validate_username(&username, false)?;
    validate_password(&state.settings, &body.password)?;
    let name = username.clone();
    // Checked before hashing so a taken name costs nothing; the insert
    // below still catches a race.
    if state.db.call(move |c| store::username_taken(c, &name)).await? {
        return Err(ApiError::conflict("That username is taken."));
    }
    let hash = state.auth.hasher.hash(body.password).await?;
    let name = username.clone();
    let user = match state
        .db
        .call(move |c| store::insert_user(c, &name, &hash, Role::User))
        .await?
    {
        store::Inserted::Done(user) => user,
        store::Inserted::Conflict => return Err(ApiError::conflict("That username is taken.")),
    };
    state.auth.signups.record(&ip);
    let cookie = start_session(&state, &user, body.remember, &headers, &ip).await?;
    log(format!("[auth] new account {} from {ip}", user.username));
    Ok(signed_in(StatusCode::CREATED, user, cookie))
}

#[derive(Deserialize)]
pub struct SetupBody {
    token: String,
    username: String,
    password: String,
    #[serde(default)]
    remember: bool,
}

/// `POST /api/auth/setup` (public, until an admin exists): the first admin,
/// unlocked by the token printed in the server log.
pub async fn setup(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    ApiJson(body): ApiJson<SetupBody>,
) -> ApiResult<Response> {
    if !state.auth.has_setup_token() || state.db.call(|c| store::admin_exists(c)).await? {
        return Err(ApiError::not_found());
    }
    state
        .auth
        .login_failures
        .check(&ip_key(&ip), state.settings.auth.login_max_failures_per_ip)
        .map_err(ApiError::rate_limited)?;
    if !state.auth.setup_token_matches(&body.token) {
        state.auth.login_failures.record(&ip_key(&ip));
        return Err(ApiError::forbidden(
            "That setup token isn't right. Copy it from the server log.",
        ));
    }
    let username = body.username.trim().to_string();
    validate_username(&username, true)?;
    validate_password(&state.settings, &body.password)?;
    let hash = state.auth.hasher.hash(body.password).await?;
    let name = username.clone();
    let user = match state
        .db
        .call(move |c| store::insert_user(c, &name, &hash, Role::Admin))
        .await?
    {
        store::Inserted::Done(user) => user,
        // The one_admin index: someone finished setup first.
        store::Inserted::Conflict => return Err(ApiError::not_found()),
    };
    state.auth.clear_setup_token();
    let cookie = start_session(&state, &user, body.remember, &headers, &ip).await?;
    log(format!(
        "[auth] setup complete: admin account {} created",
        user.username
    ));
    Ok(signed_in(StatusCode::CREATED, user, cookie))
}

/// `POST /api/auth/logout`: ends this session only.
pub async fn logout(State(state): State<AppState>, CurrentSession(session): CurrentSession) -> ApiResult<Response> {
    let hash = session.token_hash.clone();
    state.db.call(move |c| store::delete_session(c, &hash)).await?;
    state.auth.forget_session(&session.token_hash);
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, clear_cookie(&state.settings));
    Ok(response)
}

/// `GET /api/me`.
pub async fn me(CurrentUser(user): CurrentUser) -> Json<User> {
    Json(user)
}

#[derive(Deserialize)]
pub struct ChangePassword {
    current_password: String,
    new_password: String,
}

/// `POST /api/me/password`: needs the current password; signs out every
/// other session.
pub async fn change_password(
    State(state): State<AppState>,
    CurrentSession(session): CurrentSession,
    ClientIp(ip): ClientIp,
    ApiJson(body): ApiJson<ChangePassword>,
) -> ApiResult<StatusCode> {
    let user = session.user;
    check_login_limits(&state, &user.username, &ip)?;
    let id = user.id.clone();
    let Some(row) = state.db.call(move |c| store::find_by_id(c, &id)).await? else {
        return Err(ApiError::unauthorized());
    };
    let verified = state
        .auth
        .hasher
        .verify(Some(row.password_hash), body.current_password)
        .await?;
    if !verified.ok {
        record_login_failure(&state, &user.username, &ip);
        return Err(ApiError::forbidden("Your current password isn't right."));
    }
    validate_password(&state.settings, &body.new_password)?;
    let hash = state.auth.hasher.hash(body.new_password).await?;
    let (id, keep) = (user.id.clone(), session.token_hash.clone());
    state
        .db
        .call(move |c| store::set_password(c, &id, &hash, Some(&keep)))
        .await?;
    state.auth.forget_user(&user.id);
    log(format!("[auth] {} changed their password", user.username));
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct ResetInfo {
    username: String,
    expires_at: String,
}

/// `GET /api/password-reset/:token` (public): which account a link is for.
pub async fn reset_info(State(state): State<AppState>, Path(token): Path<String>) -> ApiResult<Json<ResetInfo>> {
    let hash = sha256_hex(&token);
    match state.db.call(move |c| store::find_reset(c, &hash)).await? {
        Some(link) => Ok(Json(ResetInfo {
            username: link.username,
            expires_at: link.expires_at,
        })),
        None => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "This reset link has expired or has already been used. Ask the site admin for a new one.",
        )),
    }
}

#[derive(Deserialize)]
pub struct NewPassword {
    password: String,
}

/// `POST /api/password-reset/:token` (public): sets the password, uses up
/// the link and signs the account out everywhere.
pub async fn reset_password(
    State(state): State<AppState>,
    Path(token): Path<String>,
    ApiJson(body): ApiJson<NewPassword>,
) -> ApiResult<StatusCode> {
    let hash = sha256_hex(&token);
    let probe = hash.clone();
    let Some(link) = state.db.call(move |c| store::find_reset(c, &probe)).await? else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "This reset link has expired or has already been used. Ask the site admin for a new one.",
        ));
    };
    validate_password(&state.settings, &body.password)?;
    let password_hash = state.auth.hasher.hash(body.password).await?;
    let Some(user_id) = state
        .db
        .call(move |c| store::use_reset(c, &hash, &password_hash))
        .await?
    else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "This reset link has already been used.",
        ));
    };
    state.auth.forget_user(&user_id);
    log(format!(
        "[auth] {} set a new password with a reset link ({})",
        link.username, link.id
    ));
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct UsersResponse {
    signup_enabled: bool,
    users: Vec<store::UserSummary>,
}

/// `GET /api/admin/users`.
pub async fn list_users(State(state): State<AppState>, AdminUser(_): AdminUser) -> ApiResult<Json<UsersResponse>> {
    let users = state.db.call(|c| store::list_users(c)).await?;
    Ok(Json(UsersResponse {
        signup_enabled: state.settings.auth.signup_enabled,
        users,
    }))
}

#[derive(Serialize)]
pub struct ResetLinkResponse {
    /// Shown once: only its hash is stored.
    url: String,
    expires_at: String,
}

/// `POST /api/admin/users/:id/reset-link`: a single-use link, replacing any
/// older unused one.
pub async fn create_reset_link(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(user_id): Path<String>,
) -> ApiResult<Json<ResetLinkResponse>> {
    let id = user_id.clone();
    let Some(target) = state.db.call(move |c| store::find_by_id(c, &id)).await? else {
        return Err(ApiError::not_found());
    };
    let token = random_token(32);
    let hash = sha256_hex(&token);
    let expires_at = SystemTime::now() + state.settings.auth.password_reset_ttl;
    let admin_id = admin.id.clone();
    state
        .db
        .call(move |c| store::create_reset(c, &hash, &user_id, &admin_id, expires_at))
        .await?;
    log(format!("[admin] reset link created for {}", target.user.username));
    Ok(Json(ResetLinkResponse {
        url: format!("{}/reset/{token}", state.settings.http.public_origin),
        expires_at: ts(expires_at),
    }))
}

#[derive(Deserialize)]
pub struct PatchUser {
    disabled: bool,
}

/// `PATCH /api/admin/users/:id`: disable or re-enable. Disabling ends every
/// session at once.
pub async fn patch_user(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(user_id): Path<String>,
    ApiJson(body): ApiJson<PatchUser>,
) -> ApiResult<StatusCode> {
    if user_id == admin.id {
        return Err(ApiError::bad_request("You can't disable your own account."));
    }
    let id = user_id.clone();
    if !state
        .db
        .call(move |c| store::set_disabled(c, &id, body.disabled))
        .await?
    {
        return Err(ApiError::not_found());
    }
    state.auth.forget_user(&user_id);
    log(format!(
        "[admin] user {user_id} {}",
        if body.disabled { "disabled" } else { "re-enabled" }
    ));
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/admin/users/:id`: the account with its sessions,
/// conversations and clients. Connected clients are disconnected, and their
/// keys stop working.
pub async fn delete_user(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(user_id): Path<String>,
) -> ApiResult<StatusCode> {
    if user_id == admin.id {
        return Err(ApiError::bad_request("You can't delete your own account."));
    }
    // Stop their replies first, so nothing writes to rows being deleted.
    state.replies.cancel_owner(&user_id).await;
    let id = user_id.clone();
    let Some(clients) = state.db.call(move |c| store::delete_user(c, &id)).await? else {
        return Err(ApiError::not_found());
    };
    state.auth.forget_user(&user_id);
    for client_id in &clients {
        state.live.forget(client_id);
    }
    log(format!(
        "[admin] user {user_id} deleted, with {} client(s)",
        clients.len()
    ));
    Ok(StatusCode::NO_CONTENT)
}
