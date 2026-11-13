//! Chat over HTTP: conversations, sending, and streaming replies.
//!
//! The server builds each request's history and owns each reply's
//! lifecycle (`replies.rs`). Replies stream to the dashboard as server-sent
//! events, from the reply's hub, so a stream can close and reconnect
//! without affecting generation:
//!
//! - `created` (send and retry only): the messages the request created,
//!   `{ conversation?, user_message?, reply }`
//! - `snapshot`: the full text so far and the current status
//! - `status`: a status change, with who's answering
//! - `progress`: `{ stage }`, before text: `loading_model`, then `generating`
//! - `chunk`: `{ delta }`
//! - `end`: the final status and error, last
//!
//! Errors found before a stream starts are plain JSON `{ error }`.

use crate::access::{self, Check, ClientAction};
use crate::auth::{CurrentUser, User};
use crate::conversations::{self, Busy, Message, MessageRole, SetLeaf, TurnOutcome, RENAME_MAX_CHARS};
use crate::http::{ApiError, ApiJson, ApiResult};
use crate::live::usable;
use crate::replies::{ReplyEvent, RetryError, StartError, Target};
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
use std::collections::HashMap;
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

/// `GET /api/chat/models`: the models on connected clients this user can
/// Use that can answer now (`available`, `loading` or `loaded`), `echo`
/// included while it's enabled.
pub async fn list_models(State(state): State<AppState>, CurrentUser(user): CurrentUser) -> ApiResult<Response> {
    let echo_on = state.settings.client.inference.echo_model_enabled;
    let live: Vec<_> = state
        .live
        .models()
        .into_iter()
        .filter(|(_, m)| echo_on || m.backend != protocol::Backend::Echo)
        .collect();
    if live.is_empty() {
        return Ok(Json(Vec::<ChatModel>::new()).into_response());
    }
    let viewer = user.clone();
    let rows = state.db.call(move |c| access::visible_clients(c, &viewer)).await?;
    let usable: HashMap<String, String> = rows
        .into_iter()
        .filter(|r| access::client_access(&user, r).is_some_and(|a| access::allows(a, ClientAction::Use)))
        .map(|r| (r.client_id, r.friendly_name))
        .collect();
    let mut list: Vec<ChatModel> = live
        .into_iter()
        .filter_map(|(client_id, model)| {
            Some(ChatModel {
                client_name: usable.get(&client_id)?.clone(),
                client_id,
                model: model.id,
                model_name: model.display_name,
                backend: model.backend,
                state: model.state,
            })
        })
        .collect();
    list.sort_by(|a, b| {
        (&a.client_name, &a.client_id, &a.model_name, &a.model).cmp(&(
            &b.client_name,
            &b.client_id,
            &b.model_name,
            &b.model,
        ))
    });
    Ok(Json(list).into_response())
}

/// The user may Use the target client, it's connected and offers the model.
async fn check_target(state: &AppState, user: &User, target: &Target) -> ApiResult<()> {
    let Target::Client { client_id, model } = target;
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
    let echo_off = !state.settings.client.inference.echo_model_enabled;
    match conn.model(model) {
        Some(m) if echo_off && m.backend == protocol::Backend::Echo => {}
        Some(m) if usable(m.state) => return Ok(()),
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
    let (owner, cid) = (user.id.clone(), conversation_id.clone());
    let (content, parent_id) = (body.content, body.parent_id);
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
                },
            )
        })
        .await?;
    let (new_conversation, user_message, reply) = match outcome {
        TurnOutcome::Created(turn) => (turn.new_conversation, turn.user_message, turn.reply),
        TurnOutcome::NotFound => return Err(ApiError::not_found()),
        TurnOutcome::BadParent(why) => return Err(ApiError::bad_request(why)),
        TurnOutcome::Busy(busy) => return Err(busy_error(busy)),
    };
    let created = event(
        "created",
        json!({ "conversation": new_conversation, "user_message": user_message, "reply": reply }),
    );
    if let Err(StartError::Internal(e)) = state.replies.start(&reply.id, &body.target).await {
        log(format!("[chat] couldn't start {}: {e:#}", reply.id));
    }
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
    /// Default: the client and model that answered last time.
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
        None => match (&message.client_id, &message.model) {
            (Some(client_id), Some(model)) => Target::Client {
                client_id: client_id.clone(),
                model: model.clone(),
            },
            _ => {
                return Err(ApiError::bad_request(
                    "The client that answered no longer exists. Choose another one.",
                ))
            }
        },
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
            // started; give it a moment.
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
                Ok(ReplyEvent::Status(s)) => event(
                    "status",
                    json!({
                        "status": s.status,
                        "client_id": s.client_id,
                        "client_label": s.client_label,
                        "model": s.model,
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
    })
}
