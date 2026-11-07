//! Chat relay: `POST /api/chat` sends the dashboard's conversation to a
//! connected client as a `chat_request`, and streams the client's
//! `chat_chunk`s back to the dashboard as server-sent events.
//!
//! The server never generates anything itself. It routes by `request_id`,
//! checks each reply comes from the connection the request went to, and
//! cancels the request on the client if nobody is listening any more.

use crate::{db, log, AppState};
use axum::{
    extract::State,
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Json,
};
use futures_util::stream;
use protocol::{ChatTurn, ServerMessage};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

/// One piece of a client's reply, on its way to the dashboard.
#[derive(Debug, PartialEq)]
pub enum ChatEvent {
    Chunk(String),
    Done,
    Error(String),
}

struct Pending {
    /// The connection the request was sent on: only it may answer.
    conn_id: u64,
    /// Unbounded so delivering never blocks (or drops pieces in) the
    /// client's socket loop; a reply is only ever as big as the reply.
    tx: mpsc::UnboundedSender<ChatEvent>,
}

/// Requests sent to a client that haven't finished yet, by request_id.
#[derive(Default)]
pub struct Chats {
    pending: Mutex<HashMap<String, Pending>>,
}

impl Chats {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn register(&self, request_id: &str, conn_id: u64) -> mpsc::UnboundedReceiver<ChatEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.lock().insert(request_id.to_string(), Pending { conn_id, tx });
        rx
    }

    /// Removes the request; returns whether it was still pending.
    fn remove(&self, request_id: &str) -> bool {
        self.lock().remove(request_id).is_some()
    }

    /// Passes an event from connection `conn_id` to whoever is waiting on
    /// `request_id`. Ignored (returns false) if the request is unknown or
    /// was sent to a different connection. `Done` and `Error` finish it.
    pub fn deliver(&self, conn_id: u64, request_id: &str, event: ChatEvent) -> bool {
        let mut pending = self.lock();
        let Some(p) = pending.get(request_id).filter(|p| p.conn_id == conn_id) else {
            return false;
        };
        let finished = !matches!(event, ChatEvent::Chunk(_));
        let _ = p.tx.send(event);
        if finished {
            pending.remove(request_id);
        }
        true
    }

    /// Fails every request still waiting on `conn_id`, which has closed.
    pub fn fail_conn(&self, conn_id: u64, reason: &str) {
        self.lock().retain(|_, p| {
            if p.conn_id != conn_id {
                return true;
            }
            let _ = p.tx.send(ChatEvent::Error(reason.to_string()));
            false
        });
    }
}

/// Lives as long as the response stream. If the stream goes away while
/// the request is still pending (the browser left, or it timed out), the
/// client is told to stop generating.
struct CancelOnDrop {
    chats: Arc<Chats>,
    request_id: String,
    tx: mpsc::Sender<ServerMessage>,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.chats.remove(&self.request_id) {
            log(format!("[chat] {} cancelled", self.request_id));
            let _ = self.tx.try_send(ServerMessage::ChatCancel {
                request_id: self.request_id.clone(),
            });
        }
    }
}

#[derive(Deserialize)]
pub struct ChatBody {
    client_id: String,
    model: String,
    /// The conversation so far, oldest first, ending with the new message.
    messages: Vec<ChatTurn>,
}

fn reject(status: StatusCode, error: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": error.into() }))).into_response()
}

fn sse(name: &str, data: serde_json::Value) -> Result<Event, Infallible> {
    Ok(Event::default().event(name).data(data.to_string()))
}

/// `POST /api/chat`. Errors found before anything is sent to the client
/// are plain JSON `{error}` responses; after that, everything arrives as
/// SSE: `chunk` `{delta}`s, then exactly one `done` `{}` or `error` `{error}`.
pub async fn post_chat(State(state): State<AppState>, Json(body): Json<ChatBody>) -> Response {
    if body.messages.is_empty() {
        return reject(StatusCode::BAD_REQUEST, "messages is empty");
    }
    let Some(conn) = state.live.sender(&body.client_id) else {
        return reject(StatusCode::NOT_FOUND, "that client isn't connected");
    };
    if !conn.models.iter().any(|m| m.id == body.model) {
        return reject(
            StatusCode::BAD_REQUEST,
            format!("that client doesn't offer the model {:?}", body.model),
        );
    }

    let request_id = Uuid::new_v4().to_string();
    let rx = state.chats.register(&request_id, conn.conn_id);
    let turns = body.messages.len();
    let request = ServerMessage::ChatRequest {
        request_id: request_id.clone(),
        model: body.model.clone(),
        messages: body.messages,
    };
    if conn.tx.try_send(request).is_err() {
        state.chats.remove(&request_id);
        return reject(StatusCode::SERVICE_UNAVAILABLE, "that client is busy, try again");
    }
    log(format!(
        "[chat] {request_id} sent to {} ({}, {turns} message(s))",
        body.client_id, body.model
    ));

    let guard = CancelOnDrop {
        chats: state.chats.clone(),
        request_id,
        tx: conn.tx,
    };
    let idle = state.settings.chat_idle_timeout;
    // Keep-alive comments stop proxies (Cloudflare drops idle responses
    // after 100 s) from cutting off a slow first token.
    Sse::new(reply_stream(rx, guard, idle))
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn reply_stream(
    rx: mpsc::UnboundedReceiver<ChatEvent>,
    guard: CancelOnDrop,
    idle: Duration,
) -> impl futures_util::Stream<Item = Result<Event, Infallible>> {
    // The state is None once the final event has gone out, which ends the
    // stream and drops the guard.
    stream::unfold(Some((rx, guard)), move |state| async move {
        let (mut rx, guard) = state?;
        let last = match tokio::time::timeout(idle, rx.recv()).await {
            Ok(Some(ChatEvent::Chunk(delta))) => {
                return Some((sse("chunk", serde_json::json!({ "delta": delta })), Some((rx, guard))));
            }
            Ok(Some(ChatEvent::Done)) => sse("done", serde_json::json!({})),
            Ok(Some(ChatEvent::Error(error))) => sse("error", serde_json::json!({ "error": error })),
            Ok(None) => sse("error", serde_json::json!({ "error": "the reply ended unexpectedly" })),
            Err(_) => sse(
                "error",
                serde_json::json!({
                    "error": format!("the client sent nothing for {} s", idle.as_secs())
                }),
            ),
        };
        drop(guard);
        Some((last, None))
    })
}

#[derive(Serialize)]
struct ChatModel {
    client_id: String,
    /// The client's friendly name (or hostname), for the picker.
    client_name: String,
    model: String,
}

/// `GET /api/chat/models`: every model offered by a connected client.
pub async fn list_models(State(state): State<AppState>) -> Response {
    let models = state.live.models();
    if models.is_empty() {
        return Json(Vec::<ChatModel>::new()).into_response();
    }
    let names: HashMap<String, String> = match db::list_clients(&state.db) {
        Ok(rows) => rows.into_iter().map(|r| (r.client_id, r.friendly_name)).collect(),
        Err(e) => {
            log(format!("[chat] error listing clients: {e:#}"));
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let mut list: Vec<ChatModel> = models
        .into_iter()
        .map(|(client_id, model)| ChatModel {
            client_name: names.get(&client_id).cloned().unwrap_or_else(|| client_id.clone()),
            client_id,
            model: model.id,
        })
        .collect();
    list.sort_by(|a, b| (&a.client_name, &a.model).cmp(&(&b.client_name, &b.model)));
    Json(list).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_reach_the_right_request_and_finish_it() {
        let chats = Chats::default();
        let mut a = chats.register("a", 1);
        let mut b = chats.register("b", 1);

        assert!(chats.deliver(1, "a", ChatEvent::Chunk("hi".into())));
        assert!(chats.deliver(1, "a", ChatEvent::Done));
        assert_eq!(a.try_recv(), Ok(ChatEvent::Chunk("hi".into())));
        assert_eq!(a.try_recv(), Ok(ChatEvent::Done));
        assert!(b.try_recv().is_err(), "b got nothing");

        // Finished: later events are ignored, and removing is a no-op.
        assert!(!chats.deliver(1, "a", ChatEvent::Chunk("late".into())));
        assert!(!chats.remove("a"));

        assert!(chats.deliver(1, "b", ChatEvent::Error("boom".into())));
        assert_eq!(b.try_recv(), Ok(ChatEvent::Error("boom".into())));
        assert!(!chats.remove("b"));
    }

    #[test]
    fn events_from_another_connection_are_ignored() {
        let chats = Chats::default();
        let mut rx = chats.register("r", 1);
        assert!(!chats.deliver(2, "r", ChatEvent::Chunk("injected".into())));
        assert!(!chats.deliver(2, "r", ChatEvent::Done));
        assert!(rx.try_recv().is_err());
        assert!(chats.remove("r"), "still pending");
    }

    #[test]
    fn a_closed_connection_fails_only_its_own_requests() {
        let chats = Chats::default();
        let mut on_1 = chats.register("x", 1);
        let mut on_2 = chats.register("y", 2);
        chats.fail_conn(1, "the client disconnected");
        assert_eq!(on_1.try_recv(), Ok(ChatEvent::Error("the client disconnected".into())));
        assert!(!chats.remove("x"));
        assert!(on_2.try_recv().is_err());
        assert!(chats.remove("y"));
    }

    #[tokio::test]
    async fn dropping_a_pending_stream_cancels_it_on_the_client() {
        let chats = Arc::new(Chats::default());
        let (tx, mut client) = mpsc::channel(4);

        // Dropped while pending: cancel sent.
        let rx = chats.register("r1", 1);
        drop(reply_stream(
            rx,
            CancelOnDrop {
                chats: chats.clone(),
                request_id: "r1".into(),
                tx: tx.clone(),
            },
            Duration::from_secs(60),
        ));
        assert!(matches!(
            client.try_recv(),
            Ok(ServerMessage::ChatCancel { request_id }) if request_id == "r1"
        ));

        // Finished normally: no cancel.
        let rx = chats.register("r2", 1);
        chats.deliver(1, "r2", ChatEvent::Done);
        drop(reply_stream(
            rx,
            CancelOnDrop {
                chats: chats.clone(),
                request_id: "r2".into(),
                tx,
            },
            Duration::from_secs(60),
        ));
        assert!(client.try_recv().is_err());
    }
}
