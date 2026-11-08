//! Clients from the dashboard's side (listing, renaming, deleting, pairing
//! codes), plus `/enroll`, where a client trades a pairing code for its id.
//! Who may do what is decided in `access.rs`.

use crate::access::{self, Check, ClientAccess, ClientAction};
use crate::auth::CurrentUser;
use crate::db::{self, EnrollOutcome};
use crate::http::{ApiError, ApiJson, ApiResult, ClientIp};
use crate::live::Sample;
use crate::{log, AppState};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use protocol::{Backend, EnrollError, EnrollRequest, EnrollResponse, HardwareSpecs, ModelState, Progress};
use rand::Rng;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Must match MAX_FRIENDLY_NAME_LENGTH in frontend/src/api.ts.
const MAX_FRIENDLY_NAME_CHARS: usize = 64;

#[derive(Serialize)]
pub struct ClientsResponse {
    /// Random per server start; a change means `seq` numbering restarted.
    epoch: String,
    /// The next poll's `since`: the newest sample seq among these clients
    /// (never less than the `since` asked for).
    latest_seq: u64,
    clients: Vec<ClientListEntry>,
}

#[derive(Serialize)]
pub struct Owner {
    user_id: String,
    username: String,
}

#[derive(Serialize)]
pub struct ClientListEntry {
    client_id: String,
    hostname: String,
    /// What the dashboard displays: set by the user, or the hostname.
    friendly_name: String,
    enrolled_at: String,
    last_seen_at: Option<String>,
    connected: bool,
    owner: Owner,
    access: ClientAccess,
    /// The dashboard decides what to offer from these, never from the role.
    can_use: bool,
    can_manage: bool,
    /// Jobs (chat replies) it's working on now, from the moment one is sent
    /// to it until it ends, whoever started them. Drives the card's blinking
    /// blue dot.
    active_jobs: usize,
    /// The rest come from the client's `info` message, so they're null
    /// until the client has connected at least once.
    os: Option<String>,
    client_version: Option<String>,
    specs: Option<HardwareSpecs>,
    info_updated_at: Option<String>,
    /// Samples newer than `since`, oldest first. Empty when disconnected.
    samples: Vec<Sample>,
    /// What models it has and is pulling; the card's expanded view fetches
    /// the details from `/api/clients/:id/models`.
    models: ModelsSummary,
}

/// Counts by state (the built-in echo model left out), and pulls running.
#[derive(Serialize, Default, Debug, PartialEq)]
pub struct ModelsSummary {
    available: usize,
    loading: usize,
    loaded: usize,
    downloading: usize,
    failed: usize,
    active_pulls: Vec<ActivePull>,
}

#[derive(Serialize, Debug, PartialEq)]
pub struct ActivePull {
    command_id: String,
    model_id: String,
    model_name: String,
    progress: Option<Progress>,
}

#[derive(Deserialize)]
pub struct ClientsQuery {
    since: Option<u64>,
}

/// `GET /api/clients?since=<seq>`: the clients this user can see, with their
/// live samples.
pub async fn list_clients(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<ClientsQuery>,
) -> ApiResult<Json<ClientsResponse>> {
    let viewer = user.clone();
    let rows = state.db.call(move |c| access::visible_clients(c, &viewer)).await?;
    let since = query.since.unwrap_or(0);
    let mut snapshot = state.live.snapshot(since);
    let active_jobs = state.replies.active_jobs();
    let mut active_pulls = state.model_commands.active_pulls();
    // The cursor only reflects samples this user gets, so it says nothing
    // about other people's clients.
    let mut latest_seq = since;
    let clients = rows
        .into_iter()
        .filter_map(|r| {
            let access = access::client_access(&user, &r).filter(|&a| access::allows(a, ClientAction::View))?;
            let samples = snapshot.connected.remove(&r.client_id);
            if let Some(last) = samples.as_ref().and_then(|s| s.last()) {
                latest_seq = latest_seq.max(last.seq);
            }
            let mut models = ModelsSummary::default();
            if let Some(inventory) = state.live.inventory(&r.client_id) {
                for m in inventory.models.iter().filter(|m| m.backend != Backend::Echo) {
                    *match m.state {
                        ModelState::Available => &mut models.available,
                        ModelState::Loading => &mut models.loading,
                        ModelState::Loaded => &mut models.loaded,
                        ModelState::Downloading => &mut models.downloading,
                        ModelState::Failed => &mut models.failed,
                    } += 1;
                }
            }
            models.active_pulls = active_pulls
                .remove(&r.client_id)
                .unwrap_or_default()
                .into_iter()
                .map(|c| ActivePull {
                    command_id: c.command_id,
                    model_id: c.model_id,
                    model_name: c.model_name,
                    progress: c.progress,
                })
                .collect();
            Some(ClientListEntry {
                models,
                connected: samples.is_some(),
                samples: samples.unwrap_or_default(),
                last_seen_at: snapshot.last_seen.remove(&r.client_id).or(r.last_seen_at),
                owner: Owner {
                    user_id: r.owner_user_id,
                    username: r.owner_username,
                },
                access,
                can_use: access::allows(access, ClientAction::Use),
                can_manage: access::allows(access, ClientAction::Manage),
                active_jobs: active_jobs.get(&r.client_id).copied().unwrap_or(0),
                client_id: r.client_id,
                hostname: r.hostname,
                friendly_name: r.friendly_name,
                enrolled_at: r.enrolled_at,
                os: r.os,
                client_version: r.client_version,
                specs: r.specs_json.and_then(|json| serde_json::from_str(&json).ok()),
                info_updated_at: r.info_updated_at,
            })
        })
        .collect();
    Ok(Json(ClientsResponse {
        epoch: state.live.epoch.clone(),
        latest_seq,
        clients,
    }))
}

/// 404 if the user can't see the client, 403 if they can but may not do
/// `action`.
pub(crate) async fn require(
    state: &AppState,
    user: &crate::auth::User,
    client_id: &str,
    action: ClientAction,
) -> ApiResult<db::ClientListRow> {
    let (viewer, id) = (user.clone(), client_id.to_string());
    match state.db.call(move |c| access::check(c, &viewer, &id, action)).await? {
        Check::Allowed(row) => Ok(*row),
        Check::Denied => Err(ApiError::forbidden("You can't change this client.")),
        Check::Hidden => Err(ApiError::not_found()),
    }
}

#[derive(Deserialize)]
pub struct UpdateClientRequest {
    /// Blank or null resets to the hostname.
    friendly_name: Option<String>,
}

/// `PATCH /api/clients/:id` (Manage).
pub async fn update_client(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(client_id): Path<String>,
    ApiJson(req): ApiJson<UpdateClientRequest>,
) -> ApiResult<StatusCode> {
    require(&state, &user, &client_id, ClientAction::Manage).await?;
    let name = req
        .friendly_name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    if name
        .as_ref()
        .is_some_and(|n| n.chars().count() > MAX_FRIENDLY_NAME_CHARS)
    {
        return Err(ApiError::bad_request(format!(
            "Names can be at most {MAX_FRIENDLY_NAME_CHARS} characters long."
        )));
    }
    let (id, n) = (client_id.clone(), name.clone());
    if !state
        .db
        .call(move |c| db::set_friendly_name(c, &id, n.as_deref()))
        .await?
    {
        return Err(ApiError::not_found());
    }
    log(format!("[clients] {} renamed {client_id} to {name:?}", user.username));
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/clients/:id` (Manage). Closes its socket if it's connected;
/// it then reconnects, gets `unknown client_id`, and clears its identity.
/// Replies it gave stay readable under its saved name.
pub async fn delete_client(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(client_id): Path<String>,
) -> ApiResult<StatusCode> {
    require(&state, &user, &client_id, ClientAction::Manage).await?;
    let id = client_id.clone();
    if !state.db.call(move |c| db::delete_client(c, &id)).await? {
        return Err(ApiError::not_found());
    }
    state.live.forget(&client_id);
    log(format!("[clients] {} deleted client {client_id}", user.username));
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct PairingCodeResponse {
    code: String,
}

/// `POST /api/pairing-codes`: any signed-in user. Whoever enrolls with the
/// code becomes a client owned by them.
pub async fn create_pairing_code(State(state): State<AppState>, CurrentUser(user): CurrentUser) -> ApiResult<Response> {
    let code = random_code();
    let ttl_secs = i64::try_from(state.settings.pairing_code_ttl_secs).unwrap_or(i64::MAX);
    let (c2, uid) = (code.clone(), user.id.clone());
    state
        .db
        .call(move |c| db::create_pairing_code(c, &c2, &uid, ttl_secs))
        .await?;
    log(format!("[pairing] {} generated a pairing code", user.username));
    Ok(Json(PairingCodeResponse { code }).into_response())
}

#[derive(Serialize)]
pub struct PairingStatusResponse {
    used: bool,
    claimed_by: Option<ClaimedClient>,
}

#[derive(Serialize)]
pub struct ClaimedClient {
    client_id: String,
    hostname: String,
}

/// `GET /api/pairing-codes/:code/status`: only the code's creator sees it.
/// Polled by the "Add client" dialog until the code is claimed.
pub async fn pairing_code_status(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(code): Path<String>,
) -> ApiResult<Json<PairingStatusResponse>> {
    let status = state
        .db
        .call(move |c| {
            let Some(status) = db::pairing_code_status(c, &code)? else {
                return Ok(None);
            };
            let claimed = match &status.claimed_by {
                Some(client_id) => db::get_client(c, client_id)?,
                None => None,
            };
            Ok(Some((status, claimed)))
        })
        .await?;
    match status {
        Some((status, claimed)) if status.created_by == user.id => Ok(Json(PairingStatusResponse {
            used: status.used,
            claimed_by: claimed.map(|c| ClaimedClient {
                client_id: c.client_id,
                hostname: c.hostname,
            }),
        })),
        _ => Err(ApiError::not_found()),
    }
}

/// `GET /api/dashboard-config`: the dashboard's runtime settings (graph
/// look, poll intervals), so changing them needs a server restart rather
/// than a frontend rebuild.
pub async fn dashboard_config(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.settings.dashboard.clone())
}

/// `POST /enroll`, used by clients (no session): a pairing code buys a
/// client id. Rate-limited per IP.
pub async fn enroll(State(state): State<AppState>, ClientIp(ip): ClientIp, Json(req): Json<EnrollRequest>) -> Response {
    if let Err(retry_after) = state
        .auth
        .enrolls
        .attempt(&ip, state.settings.auth.enroll_max_attempts_per_ip)
    {
        log(format!("[enroll] too many attempts from {ip}"));
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            Json(EnrollError {
                error: format!("too many attempts, try again in {} s", retry_after.as_secs().max(1)),
            }),
        )
            .into_response();
        if let Ok(value) = retry_after.as_secs().max(1).to_string().parse() {
            response.headers_mut().insert(axum::http::header::RETRY_AFTER, value);
        }
        return response;
    }
    let client_id = Uuid::new_v4().to_string();
    let id = client_id.clone();
    let hostname = req.hostname.clone();
    let result = state
        .db
        .call(move |c| db::try_enroll(c, &req.pairing_code, &id, &req.public_key, &req.hostname))
        .await;
    match result {
        Ok(EnrollOutcome::Enrolled { owner_user_id }) => {
            log(format!(
                "[enroll] new client {client_id} ({hostname}), owned by {owner_user_id}"
            ));
            (StatusCode::OK, Json(EnrollResponse { client_id })).into_response()
        }
        Ok(EnrollOutcome::InvalidCode) => (
            StatusCode::FORBIDDEN,
            Json(EnrollError {
                error: "invalid or expired pairing code".into(),
            }),
        )
            .into_response(),
        Err(e) => {
            log(format!("[enroll] error: {e:#}"));
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(EnrollError {
                    error: "internal error".into(),
                }),
            )
                .into_response()
        }
    }
}

fn random_code() -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..8).map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char).collect()
}
