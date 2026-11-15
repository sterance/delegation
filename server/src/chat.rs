//! Chat over HTTP: conversations, sending, and streaming replies.
//!
//! The server builds each request's history and owns each reply's
//! lifecycle (`replies.rs`). A reply is a job in the queue (`jobs/`): a send
//! names a target, either one client (`{ kind: "client", client_id, model }`)
//! or the user's pool (`{ kind: "pool", model }`, "any of my clients"), and
//! the scheduler places it. Replies stream to the dashboard as server-sent
//! events, from the reply's hub, so a stream can close and reconnect
//! without affecting generation:
//!
//! - `created` (send and retry only): the messages the request created,
//!   `{ conversation?, user_message?, reply }`
//! - `snapshot`: the full text so far and the current status, with who's
//!   answering, `attempt` (attempts made so far, the current one included)
//!   and `max_attempts`. While the reply is queued it also has
//!   `queue: { position, waiting_for, client_label }`: `position` is
//!   1-based among the queued jobs it could share a client with (an
//!   approximation), `waiting_for` is `a_free_client` (clients that could
//!   run it are connected but busy) or `a_client` (none is connected), and
//!   `client_label` names the client a directed reply is waiting for.
//!   `queue` is null until the scheduler has looked at the job, and once
//!   it's assigned.
//! - `status`: a status change, or a change in `queue`; the same fields as
//!   `snapshot` without the text
//! - `requeued`: `{ attempt, max_attempts, reason, previous_client_label }`.
//!   The attempt was lost and the reply went back to the queue; its text so
//!   far is discarded. `reason` is `lost` (the client disconnected),
//!   `timed_out` (it went quiet), `refused` (it was busy) or `interrupted`
//!   (reserved: a server restart re-queues with no stream attached), and
//!   `attempt` is the attempt that comes next. A `status` (queued) follows.
//! - `progress`: `{ stage }`, before text: `loading_model`, then `generating`
//! - `chunk`: `{ delta }`
//! - `end`: the final status and error, last
//!
//! Errors found before a stream starts are plain JSON `{ error }`.

use crate::access::{self, Check, ClientAction};
use crate::auth::{CurrentUser, User};
use crate::conversations::{self, Busy, Message, MessageRole, SetLeaf, TurnOutcome, RENAME_MAX_CHARS};
use crate::http::{ApiError, ApiJson, ApiResult};
use crate::jobs::store::{self as job_store, OriginalTarget};
use crate::live::usable;
use crate::replies::{ReplyEvent, RetryError, Snapshot, Target};
use crate::{log, AppState};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use futures_util::stream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::time::Duration;
use tokio::sync::{broadcast::error::RecvError, mpsc};
use uuid::Uuid;

#[derive(Serialize)]
struct ChatModel {
    client_id: String,
    /// The client's friendly name (or hostname), for the picker.
    client_name: String,
    /// The id to send in a target.
    model: String,
    model_name: String,
    backend: protocol::Backend,
    state: protocol::ModelState,
}

/// "Any of my clients" for one model.
#[derive(Serialize)]
struct ChatPool {
    /// The id to send in a pool target.
    model: String,
    model_name: String,
    /// Connected clients in the user's pool that offer it.
    clients: usize,
    /// How many of them have it loaded already.
    loaded_on: usize,
}

#[derive(Serialize)]
struct ChatModels {
    /// One entry per model offered by at least one connected client in the
    /// user's pool.
    pools: Vec<ChatPool>,
    /// Every (client, model) the user can send to directly.
    clients: Vec<ChatModel>,
}

/// What each client the user can see allows them: its name if they may Use
/// it, and whether it's in their pool.
struct Reach {
    name: String,
    can_use: bool,
    can_pool: bool,
}

async fn reach(state: &AppState, user: &User) -> ApiResult<HashMap<String, Reach>> {
    let viewer = user.clone();
    let rows = state.db.call(move |c| access::visible_clients(c, &viewer)).await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let access = access::client_access(user, &r)?;
            Some((
                r.client_id,
                Reach {
                    name: r.friendly_name,
                    can_use: access::allows(access, ClientAction::Use),
                    can_pool: access::allows(access, ClientAction::Pool),
                },
            ))
        })
        .collect())
}

/// `GET /api/chat/models`: `{ pools, clients }`. `clients` is the models on
/// connected clients this user can Use that can answer now (`available`,
/// `loading` or `loaded`), `echo` included while it's enabled; `pools` is
/// the same models grouped across the clients in the user's pool.
pub async fn list_models(State(state): State<AppState>, CurrentUser(user): CurrentUser) -> ApiResult<Response> {
    let echo_on = state.settings.client.inference.echo_model_enabled;
    let live: Vec<_> = state
        .live
        .models()
        .into_iter()
        .filter(|(_, m)| echo_on || m.backend != protocol::Backend::Echo)
        .collect();
    let mut models = ChatModels {
        pools: Vec::new(),
        clients: Vec::new(),
    };
    if live.is_empty() {
        return Ok(Json(models).into_response());
    }
    let reach = reach(&state, &user).await?;
    let mut pools: BTreeMap<String, ChatPool> = BTreeMap::new();
    for (client_id, model) in live {
        let Some(client) = reach.get(&client_id) else { continue };
        if client.can_pool {
            let pool = pools.entry(model.id.clone()).or_insert_with(|| ChatPool {
                model: model.id.clone(),
                model_name: model.display_name.clone(),
                clients: 0,
                loaded_on: 0,
            });
            pool.clients += 1;
            pool.loaded_on += usize::from(model.state == protocol::ModelState::Loaded);
        }
        if client.can_use {
            models.clients.push(ChatModel {
                client_name: client.name.clone(),
                client_id,
                model: model.id,
                model_name: model.display_name,
                backend: model.backend,
                state: model.state,
            });
        }
    }
    models.pools = pools.into_values().collect();
    models
        .pools
        .sort_by(|a, b| (&a.model_name, &a.model).cmp(&(&b.model_name, &b.model)));
    models.clients.sort_by(|a, b| {
        (&a.client_name, &a.client_id, &a.model_name, &a.model).cmp(&(
            &b.client_name,
            &b.client_id,
            &b.model_name,
            &b.model,
        ))
    });
    Ok(Json(models).into_response())
}

/// Checked before anything is created. A client target: the user may Use
/// the client, it's connected and offers the model (it may be busy: the job
/// waits in the queue for it). A pool target: at least one connected client
/// in the user's pool offers the model, so a job is never queued with
/// nowhere to go. Either way, the queue has room.
async fn check_target(state: &AppState, user: &User, target: &Target) -> ApiResult<()> {
    let echo_off = !state.settings.client.inference.echo_model_enabled;
    match target {
        Target::Client { client_id, model } => {
            let (viewer, id) = (user.clone(), client_id.clone());
            let row = match state
                .db
                .call(move |c| access::check(c, &viewer, &id, ClientAction::Use))
                .await?
            {
                Check::Allowed(row) => row,
                Check::Denied => return Err(ApiError::forbidden("You can't chat with that client.")),
                Check::Hidden => return Err(ApiError::new(StatusCode::NOT_FOUND, "That client doesn't exist.")),
            };
            let Some(conn) = state.live.sender(client_id) else {
                return Err(ApiError::conflict(format!("{} isn't connected.", row.friendly_name)));
            };
            match conn.model(model) {
                Some(m) if echo_off && m.backend == protocol::Backend::Echo => {}
                Some(m) if usable(m.state) => return check_queue(state),
                Some(m) if m.state == protocol::ModelState::Downloading => {
                    return Err(ApiError::conflict(format!(
                        "{} is still downloading {}.",
                        row.friendly_name, m.display_name
                    )))
                }
                _ => {}
            }
            Err(ApiError::bad_request(format!(
                "{} doesn't offer the model {model:?}.",
                row.friendly_name
            )))
        }
        Target::Pool { model } => {
            let reach = reach(state, user).await?;
            let offered = reach.iter().filter(|(_, client)| client.can_pool).any(|(id, _)| {
                state.live.sender(id).is_some_and(|conn| {
                    conn.model(model)
                        .is_some_and(|m| usable(m.state) && !(echo_off && m.backend == protocol::Backend::Echo))
                })
            });
            if !offered {
                return Err(ApiError::conflict(format!(
                    "None of your clients offer {model} right now."
                )));
            }
            check_queue(state)
        }
    }
}

/// `JOB_QUEUE_MAX`: 429 while that many jobs are waiting.
fn check_queue(state: &AppState) -> ApiResult<()> {
    if state.replies.scheduler().queued_len() >= state.settings.jobs.queue_max {
        return Err(ApiError::too_many(
            Duration::from_secs(5),
            "The job queue is full. Try again in a moment.",
        ));
    }
    Ok(())
}

fn busy_error(busy: Busy) -> ApiError {
    match busy {
        Busy::Conversation => {
            ApiError::conflict("A reply is still being generated in this conversation. Stop it or wait for it.")
        }
        Busy::User => ApiError::too_many(
            Duration::from_secs(5),
            "You have too many replies being generated at once. Wait for one to finish.",
        ),
    }
}

// ---------------- conversations ----------------

#[derive(Deserialize)]
pub struct ListQuery {
    /// `updated_at` of the last conversation already shown.
    before: Option<String>,
    limit: Option<usize>,
}

#[derive(Serialize)]
pub struct ConversationPage {
    conversations: Vec<conversations::ConversationSummary>,
    /// Pass as `before` for the next page; null on the last one.
    next_before: Option<String>,
}

/// `GET /api/conversations?before=&limit=`: newest first.
pub async fn list_conversations(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ConversationPage>> {
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let before = query.before.filter(|b| !b.is_empty());
    let list = state
        .db
        .call(move |c| conversations::list_conversations(c, &user.id, before.as_deref(), limit))
        .await?;
    let next_before = (list.len() == limit)
        .then(|| list.last().map(|c| c.updated_at.clone()))
        .flatten();
    Ok(Json(ConversationPage {
        conversations: list,
        next_before,
    }))
}

/// `GET /api/conversations/:id`: with every message.
pub async fn get_conversation(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let mut conversation = state
        .db
        .call(move |c| conversations::get_conversation(c, &user.id, &id))
        .await?
        .ok_or_else(ApiError::not_found)?;
    state.replies.overlay(&mut conversation.messages);
    Ok(Json(conversation).into_response())
}

#[derive(Deserialize)]
pub struct PatchConversation {
    title: Option<String>,
    /// Switching branch.
    active_leaf_id: Option<String>,
}

/// `PATCH /api/conversations/:id`: rename, or switch branch.
pub async fn patch_conversation(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<PatchConversation>,
) -> ApiResult<StatusCode> {
    if let Some(title) = &body.title {
        let title = title.trim().to_string();
        if title.is_empty() || title.chars().count() > RENAME_MAX_CHARS {
            return Err(ApiError::bad_request(format!(
                "Titles are 1 to {RENAME_MAX_CHARS} characters long."
            )));
        }
        let (owner, cid) = (user.id.clone(), id.clone());
        if !state
            .db
            .call(move |c| conversations::rename_conversation(c, &owner, &cid, &title))
            .await?
        {
            return Err(ApiError::not_found());
        }
    }
    if let Some(leaf) = body.active_leaf_id {
        let (owner, cid) = (user.id.clone(), id.clone());
        match state
            .db
            .call(move |c| conversations::set_active_leaf(c, &owner, &cid, &leaf))
            .await?
        {
            SetLeaf::Done => {}
            SetLeaf::NoConversation => return Err(ApiError::not_found()),
            SetLeaf::BadLeaf => return Err(ApiError::bad_request("active_leaf_id isn't in this conversation.")),
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/conversations/:id`: with all its messages. Replies still
/// being generated are stopped first.
pub async fn delete_conversation(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let (owner, cid) = (user.id.clone(), id.clone());
    let exists = state
        .db
        .call(move |c| Ok(conversations::get_conversation(c, &owner, &cid)?.is_some()))
        .await?;
    if !exists {
        return Err(ApiError::not_found());
    }
    state.replies.cancel_conversation(&id).await;
    let owner = user.id.clone();
    if !state
        .db
        .call(move |c| conversations::delete_conversation(c, &owner, &id))
        .await?
    {
        return Err(ApiError::not_found());
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------------- sending and streaming ----------------

#[derive(Deserialize)]
pub struct SendBody {
    /// The new user message. Leave it out to regenerate: a new reply to
    /// `parent_id`, which must then be a user message.
    content: Option<String>,
    /// What this answers: normally the current leaf (a reply), an earlier
    /// reply when editing, or null to start a new first message.
    parent_id: Option<String>,
    target: Target,
}

/// `POST /api/conversations/:id/messages`. A new conversation id (a UUID
/// made by the browser) creates the conversation. Streams the reply; closing
/// the stream doesn't stop it.
pub async fn send_message(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(conversation_id): Path<String>,
    ApiJson(body): ApiJson<SendBody>,
) -> ApiResult<Response> {
    if Uuid::parse_str(&conversation_id).is_err() {
        return Err(ApiError::bad_request("Conversation ids are UUIDs."));
    }
    if let Some(content) = &body.content {
        if content.trim().is_empty() {
            return Err(ApiError::bad_request("The message is empty."));
        }
        let max = state.settings.chat.max_message_chars;
        if content.chars().count() > max {
            return Err(ApiError::bad_request(format!(
                "Messages can be at most {max} characters long."
            )));
        }
    }
    check_target(&state, &user, &body.target).await?;

    let max_active = state.settings.chat.max_active_replies_per_user;
    let jobs = state.settings.jobs;
    let (owner, cid) = (user.id.clone(), conversation_id.clone());
    let (content, parent_id, target) = (body.content, body.parent_id, body.target);
    let outcome = state
        .db
        .call(move |c| {
            conversations::create_turn(
                c,
                conversations::NewTurn {
                    owner: &owner,
                    conversation_id: &cid,
                    content: content.as_deref(),
                    parent_id: parent_id.as_deref(),
                    max_active_per_user: max_active,
                    target: &target,
                    max_attempts: jobs.max_attempts,
                    queue_timeout: jobs.queue_timeout,
                },
            )
        })
        .await?;
    let (new_conversation, user_message, reply) = match outcome {
        TurnOutcome::Created(turn) => {
            // Into the queue: the hub exists from here on, so the stream
            // below attaches at once and sees `queued`.
            state.replies.submit(turn.job);
            (turn.new_conversation, turn.user_message, turn.reply)
        }
        TurnOutcome::NotFound => return Err(ApiError::not_found()),
        TurnOutcome::BadParent(why) => return Err(ApiError::bad_request(why)),
        TurnOutcome::Busy(busy) => return Err(busy_error(busy)),
    };
    let created = event(
        "created",
        json!({ "conversation": new_conversation, "user_message": user_message, "reply": reply }),
    );
    Ok(reply_stream(state, user.id, reply.id, Some(created)))
}

/// `GET /api/messages/:id/stream`: reconnect to a reply. A finished one gets
/// its snapshot and `end` straight away.
pub async fn stream_message(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(message_id): Path<String>,
) -> ApiResult<Response> {
    let message = load_message(&state, &user, &message_id).await?;
    if message.role != MessageRole::Assistant {
        return Err(ApiError::bad_request("Only replies stream."));
    }
    Ok(reply_stream(state, user.id, message_id, None))
}

/// `POST /api/messages/:id/cancel`: the Stop button. Keeps the text so far.
pub async fn cancel_message(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(message_id): Path<String>,
) -> ApiResult<Json<Message>> {
    let message = load_message(&state, &user, &message_id).await?;
    if message.role != MessageRole::Assistant {
        return Err(ApiError::bad_request("Only replies can be stopped."));
    }
    state.replies.cancel(&message_id).await;
    Ok(Json(load_message(&state, &user, &message_id).await?))
}

#[derive(Deserialize, Default)]
pub struct RetryBody {
    /// Default: what the reply's job was aimed at (so a pooled reply
    /// retries pooled); for a reply from before the job queue, the client
    /// and model that answered last time.
    target: Option<Target>,
}

/// `POST /api/messages/:id/retry`: a new attempt at a `failed` or `orphaned`
/// reply, in place. The body (`{ target }`) is optional. Streams like a send.
pub async fn retry_message(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(message_id): Path<String>,
    body: Bytes,
) -> ApiResult<Response> {
    let body: RetryBody = if body.iter().all(u8::is_ascii_whitespace) {
        RetryBody::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(format!("Invalid request body: {e}")))?
    };
    let message = load_message(&state, &user, &message_id).await?;
    if message.role != MessageRole::Assistant {
        return Err(ApiError::bad_request("Only replies can be retried."));
    }
    let target = match body.target {
        Some(target) => target,
        None => {
            let gone = || ApiError::bad_request("The client that answered no longer exists. Choose another one.");
            let id = message_id.clone();
            match state.db.call(move |c| job_store::original_target(c, &id)).await? {
                OriginalTarget::Target(target) => target,
                OriginalTarget::ClientDeleted => return Err(gone()),
                OriginalTarget::NoJob => match (&message.client_id, &message.model) {
                    (Some(client_id), Some(model)) => Target::Client {
                        client_id: client_id.clone(),
                        model: model.clone(),
                    },
                    _ => return Err(gone()),
                },
            }
        }
    };
    check_target(&state, &user, &target).await?;
    match state.replies.retry(&user.id, &message_id, &target).await {
        Ok(()) => {}
        Err(RetryError::NotFound) => return Err(ApiError::not_found()),
        Err(RetryError::NotRetryable(_)) => {
            return Err(ApiError::conflict("Only failed or interrupted replies can be retried."))
        }
        Err(RetryError::Busy(busy)) => return Err(busy_error(busy)),
        Err(RetryError::Internal(e)) => return Err(e.into()),
    }
    let reply = load_message(&state, &user, &message_id).await?;
    let created = event("created", json!({ "reply": reply }));
    Ok(reply_stream(state, user.id, message_id, Some(created)))
}

/// 404 unless `user` owns the message's conversation.
async fn load_message(state: &AppState, user: &User, message_id: &str) -> ApiResult<Message> {
    let (owner, id) = (user.id.clone(), message_id.to_string());
    let mut message = state
        .db
        .call(move |c| conversations::get_message(c, &owner, &id))
        .await?
        .ok_or_else(ApiError::not_found)?;
    state.replies.overlay(std::slice::from_mut(&mut message));
    Ok(message)
}

fn event(name: &str, data: serde_json::Value) -> Event {
    Event::default().event(name).data(data.to_string())
}

/// An SSE response fed by a task that follows the reply's hub. Dropping the
/// response (the browser went away) only stops the task.
fn reply_stream(state: AppState, owner: String, message_id: String, first: Option<Event>) -> Response {
    let (tx, rx) = mpsc::channel::<Event>(64);
    tokio::spawn(pump(state, owner, message_id, first, tx));
    let events = stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|e| (Ok::<_, Infallible>(e), rx))
    });
    // Keep-alive comments stop proxies (Cloudflare drops idle responses
    // after 100 s) from cutting off a slow first token.
    Sse::new(events).keep_alive(KeepAlive::default()).into_response()
}

async fn pump(state: AppState, owner: String, message_id: String, first: Option<Event>, tx: mpsc::Sender<Event>) {
    if let Some(first) = first {
        if tx.send(first).await.is_err() {
            return;
        }
    }
    let mut misses = 0;
    loop {
        let Some((snapshot, mut rx)) = state.replies.attach(&message_id) else {
            // Finished: everything is in the database.
            let (o, id) = (owner.clone(), message_id.clone());
            let message = match state.db.call(move |c| conversations::get_message(c, &o, &id)).await {
                Ok(Some(m)) => m,
                Ok(None) => return,
                Err(e) => {
                    log(format!("[chat] stream for {message_id}: {e:#}"));
                    return;
                }
            };
            // Active with no hub only for a reply between being created and
            // submitted; give it a moment.
            if message.status.is_active() && misses < 10 {
                misses += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            let _ = tx.send(event("snapshot", snapshot_json(&message))).await;
            let _ = tx
                .send(event(
                    "end",
                    json!({ "status": message.status, "error": message.error }),
                ))
                .await;
            return;
        };
        if tx
            .send(event("snapshot", serde_json::to_value(&snapshot).unwrap_or_default()))
            .await
            .is_err()
        {
            return;
        }
        loop {
            let next = tokio::select! {
                _ = tx.closed() => return,
                next = rx.recv() => next,
            };
            let out = match next {
                Ok(ReplyEvent::Chunk(delta)) => event("chunk", json!({ "delta": delta })),
                Ok(ReplyEvent::Progress(stage)) => event("progress", json!({ "stage": stage })),
                Ok(ReplyEvent::Status(s)) => event("status", status_json(&s)),
                Ok(ReplyEvent::Requeued {
                    attempt,
                    max_attempts,
                    reason,
                    previous_client_label,
                }) => event(
                    "requeued",
                    json!({
                        "attempt": attempt,
                        "max_attempts": max_attempts,
                        "reason": reason,
                        "previous_client_label": previous_client_label,
                    }),
                ),
                Ok(ReplyEvent::End { status, error, meta }) => {
                    let _ = tx
                        .send(event("end", json!({ "status": status, "error": error, "meta": meta })))
                        .await;
                    return;
                }
                // Too slow to keep up, or the hub went: start over from a
                // fresh snapshot.
                Err(RecvError::Lagged(_)) | Err(RecvError::Closed) => break,
            };
            if tx.send(out).await.is_err() {
                return;
            }
        }
    }
}

/// A `status` event: the snapshot without the text.
fn status_json(s: &Snapshot) -> serde_json::Value {
    json!({
        "status": s.status,
        "client_id": s.client_id,
        "client_label": s.client_label,
        "model": s.model,
        "queue": s.queue,
        "attempt": s.attempt,
        "max_attempts": s.max_attempts,
    })
}

/// The snapshot of a reply that has ended, from the database.
fn snapshot_json(m: &Message) -> serde_json::Value {
    json!({
        "message_id": m.id,
        "status": m.status,
        "content": m.content,
        "error": m.error,
        "client_id": m.client_id,
        "client_label": m.client_label,
        "model": m.model,
        "stage": null,
        "queue": null,
        "attempt": m.attempts,
        "max_attempts": null,
    })
}
