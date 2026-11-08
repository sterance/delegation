//! The reply runner: everything about generating an assistant reply.
//!
//! Each reply being generated has an in-memory hub holding its text so far
//! and a broadcast channel. HTTP handlers stream from the hub (`attach`),
//! but closing a stream never cancels anything: a reply keeps generating
//! when the browser goes away, until the client finishes, fails, goes
//! quiet, disconnects, or someone presses Stop (`cancel`).
//!
//! This module owns every status transition after a reply is created, the
//! periodic flushes of its text, and all of its database writes. Those
//! writes go through one writer task, in order, so a late write can never
//! overtake an earlier one. A reply's ending is announced to listeners only
//! once it has been committed, so anything that sees "completed" can rely
//! on the database saying so too.
//!
//! Every attempt at a reply gets a new `attempt_id`, sent to the client as
//! the relay's `request_id`. Chunks and endings are applied only from the
//! current attempt on the connection it was sent to; anything else (an old
//! attempt still talking after a retry, another connection) is dropped and
//! logged at debug level. The job queue will apply the same rule to every
//! job type.

use crate::config::ChatSettings;
use crate::conversations::{self, active_sql, Busy, Message, MessageStatus};
use crate::db::{now_ts, Db};
use crate::live::Live;
use crate::{debug, log};
use anyhow::Result;
use protocol::{ChatTurn, ServerMessage};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

/// Who should answer a message. Only `client` (directed assignment) exists
/// now. The job queue will add `{ "kind": "pool", "model": "…" }` ("any
/// client I can use that offers this model") without changing the API's
/// shape; until then anything but `client` fails to parse, which is a 400.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Target {
    Client { client_id: String, model: String },
}

/// What a client sent about an attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatEvent {
    Chunk(String),
    Done,
    Error(String),
}

/// What listeners on a hub receive after the snapshot.
#[derive(Debug, Clone, PartialEq)]
pub enum ReplyEvent {
    Status(Snapshot),
    Chunk(String),
    /// Last event: the final status, once it's in the database.
    End {
        status: MessageStatus,
        error: Option<String>,
    },
}

/// A reply's state at one moment.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Snapshot {
    pub message_id: String,
    pub status: MessageStatus,
    pub content: String,
    pub error: Option<String>,
    pub client_id: Option<String>,
    pub client_label: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug)]
pub enum StartError {
    NotFound,
    /// Only a `queued` reply can start.
    NotQueued,
    /// It was failed (and the failure recorded) before reaching a client.
    Failed(String),
    Internal(anyhow::Error),
}

#[derive(Debug)]
pub enum RetryError {
    NotFound,
    /// Only `failed` and `orphaned` replies can be retried.
    NotRetryable(MessageStatus),
    Busy(Busy),
    Internal(anyhow::Error),
}

/// Every allowed status change. Everything else is a bug and is refused.
pub fn can_move(from: MessageStatus, to: MessageStatus) -> bool {
    use MessageStatus::*;
    matches!(
        (from, to),
        (Queued, Assigned | Failed | Cancelled | Orphaned)
            | (Assigned, Running | Completed | Failed | Cancelled | Orphaned)
            | (Running, Completed | Failed | Cancelled | Orphaned)
            // Retry: a new attempt, in place.
            | (Failed | Orphaned, Queued)
    )
}

/// The one function every in-memory status change goes through.
fn transition(message_id: &str, from: MessageStatus, to: MessageStatus) -> bool {
    if can_move(from, to) {
        return true;
    }
    log(format!(
        "[reply] refused illegal status change for {message_id}: {} -> {}",
        from.as_str(),
        to.as_str()
    ));
    false
}

struct Hub {
    conversation_id: String,
    owner_user_id: String,
    attempt_id: Option<String>,
    conn_id: Option<u64>,
    client_tx: Option<mpsc::Sender<ServerMessage>>,
    client_id: Option<String>,
    client_label: Option<String>,
    model: Option<String>,
    /// Always an active status: what listeners see until the ending commits.
    status: MessageStatus,
    /// Set once the reply has ended; it's written, then announced.
    ending: Option<(MessageStatus, Option<String>)>,
    text: String,
    chars: usize,
    /// Text changed since the last flush.
    dirty: bool,
    last_activity: Instant,
    events: broadcast::Sender<ReplyEvent>,
    /// Woken once the ending is committed (Stop waits on this).
    acks: Vec<oneshot::Sender<()>>,
}

impl Hub {
    fn snapshot(&self, message_id: &str) -> Snapshot {
        Snapshot {
            message_id: message_id.to_string(),
            status: self.status,
            content: self.text.clone(),
            error: None,
            client_id: self.client_id.clone(),
            client_label: self.client_label.clone(),
            model: self.model.clone(),
        }
    }

    fn tell_client_to_stop(&self) {
        if let (Some(tx), Some(attempt_id)) = (&self.client_tx, &self.attempt_id) {
            let _ = tx.try_send(ServerMessage::ChatCancel {
                request_id: attempt_id.clone(),
            });
        }
    }
}

#[derive(Default)]
struct Inner {
    /// By message id.
    hubs: HashMap<String, Hub>,
    /// attempt_id -> message id, for the current attempt of each hub.
    by_attempt: HashMap<String, String>,
}

/// One database write, applied in order by the writer task.
#[derive(Debug)]
enum Write {
    Assigned {
        message_id: String,
        attempt_id: String,
        client_id: String,
        client_label: String,
        model: String,
    },
    Running {
        message_id: String,
        attempt_id: String,
    },
    /// The flush: (message id, attempt id, text so far).
    Progress(Vec<(String, String, String)>),
    Finished {
        message_id: String,
        attempt_id: Option<String>,
        conversation_id: String,
        status: MessageStatus,
        content: String,
        error: Option<String>,
    },
}

const EVENT_BUFFER: usize = 256;

pub struct ReplyRunner {
    db: Db,
    live: Arc<Live>,
    chat: ChatSettings,
    inner: Mutex<Inner>,
    writes: mpsc::UnboundedSender<Write>,
}

impl ReplyRunner {
    /// Starts the writer and the flush/idle-timeout task.
    pub fn new(db: Db, live: Arc<Live>, chat: ChatSettings) -> Arc<Self> {
        let (writes, rx) = mpsc::unbounded_channel();
        let runner = Arc::new(ReplyRunner {
            db,
            live,
            chat,
            inner: Mutex::new(Inner::default()),
            writes,
        });
        tokio::spawn(runner.clone().write_loop(rx));
        tokio::spawn(runner.clone().maintenance_loop());
        runner
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self, write: Write) {
        // Only fails if the writer task is gone, i.e. the runtime is.
        let _ = self.writes.send(write);
    }

    /// Server start: every reply that was being generated is orphaned,
    /// keeping its partial text. Runs before anything is served.
    pub fn orphan_interrupted(conn: &mut Connection) -> Result<usize> {
        for status in MessageStatus::ACTIVE {
            assert!(can_move(status, MessageStatus::Orphaned));
        }
        let active = active_sql();
        let now = now_ts();
        let tx = conn.transaction()?;
        tx.execute(
            &format!(
                "UPDATE conversations SET updated_at = ?1
                 WHERE id IN (SELECT conversation_id FROM messages WHERE status IN ({active}))"
            ),
            [&now],
        )?;
        let n = tx.execute(
            &format!(
                "UPDATE messages SET status = 'orphaned', error = 'Interrupted by a server restart',
                 completed_at = ?1 WHERE status IN ({active})"
            ),
            [&now],
        )?;
        tx.commit()?;
        Ok(n)
    }

    /// Assigns a queued reply to `target`, starts an attempt and sends the
    /// request. The caller has checked the user may Use the client. If it
    /// can't be sent, the reply is failed with the reason.
    pub async fn start(&self, message_id: &str, target: &Target) -> Result<(), StartError> {
        let Target::Client { client_id, model } = target.clone();
        let (mid, cid) = (message_id.to_string(), client_id.clone());
        let ctx = self
            .db
            .call(move |c| {
                let row: Option<(String, String, MessageStatus)> = c
                    .query_row(
                        "SELECT m.conversation_id, v.owner_user_id, m.status
                         FROM messages m JOIN conversations v ON v.id = m.conversation_id
                         WHERE m.id = ?1 AND m.role = 'assistant'",
                        [&mid],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                let Some(row) = row else { return Ok(None) };
                let history = conversations::history_for(c, &mid)?;
                let label: Option<String> = c
                    .query_row(
                        "SELECT COALESCE(friendly_name, hostname) FROM clients WHERE client_id = ?1",
                        [&cid],
                        |r| r.get(0),
                    )
                    .optional()?;
                Ok(Some((row, history, label)))
            })
            .await
            .map_err(StartError::Internal)?;
        let Some(((conversation_id, owner_user_id, status), history, label)) = ctx else {
            return Err(StartError::NotFound);
        };
        if status != MessageStatus::Queued {
            return Err(StartError::NotQueued);
        }

        {
            let mut inner = self.lock();
            if inner.hubs.contains_key(message_id) {
                return Err(StartError::NotQueued);
            }
            let (events, _) = broadcast::channel(EVENT_BUFFER);
            inner.hubs.insert(
                message_id.to_string(),
                Hub {
                    conversation_id,
                    owner_user_id,
                    attempt_id: None,
                    conn_id: None,
                    client_tx: None,
                    client_id: None,
                    client_label: None,
                    model: None,
                    status: MessageStatus::Queued,
                    ending: None,
                    text: String::new(),
                    chars: 0,
                    dirty: false,
                    last_activity: Instant::now(),
                    events,
                    acks: Vec::new(),
                },
            );
        }

        let fail = |reason: String| {
            let mut inner = self.lock();
            self.finish(&mut inner, message_id, MessageStatus::Failed, Some(reason.clone()));
            Err(StartError::Failed(reason))
        };
        let Some(label) = label else {
            return fail("That client no longer exists.".into());
        };
        let Some(conn) = self.live.sender(&client_id) else {
            return fail(format!("{label} isn't connected."));
        };
        if !conn.models.iter().any(|m| m.id == model) {
            return fail(format!("{label} doesn't offer the model {model:?}."));
        }

        let attempt_id = Uuid::new_v4().to_string();
        let turns = history.len();
        let preview = history_preview(&history);
        let mut inner = self.lock();
        let Inner { hubs, by_attempt } = &mut *inner;
        let Some(hub) = hubs.get_mut(message_id) else {
            return Err(StartError::NotFound);
        };
        // Stopped (or deleted) while we were looking things up.
        if hub.ending.is_some() || !transition(message_id, hub.status, MessageStatus::Assigned) {
            return Err(StartError::NotQueued);
        }
        hub.status = MessageStatus::Assigned;
        hub.attempt_id = Some(attempt_id.clone());
        hub.conn_id = Some(conn.conn_id);
        hub.client_tx = Some(conn.tx.clone());
        hub.client_id = Some(client_id.clone());
        hub.client_label = Some(label.clone());
        hub.model = Some(model.clone());
        hub.last_activity = Instant::now();
        let _ = hub.events.send(ReplyEvent::Status(hub.snapshot(message_id)));
        by_attempt.insert(attempt_id.clone(), message_id.to_string());
        self.write(Write::Assigned {
            message_id: message_id.to_string(),
            attempt_id: attempt_id.clone(),
            client_id: client_id.clone(),
            client_label: label,
            model: model.clone(),
        });
        let request = ServerMessage::ChatRequest {
            request_id: attempt_id.clone(),
            model: model.clone(),
            messages: history,
        };
        if conn.tx.try_send(request).is_err() {
            self.finish(
                &mut inner,
                message_id,
                MessageStatus::Failed,
                Some("That client is busy. Try again.".into()),
            );
            return Err(StartError::Failed("busy".into()));
        }
        drop(inner);
        log(format!(
            "[reply] {message_id} attempt {attempt_id} sent to {client_id} ({model}, {turns} message(s))"
        ));
        debug(|| format!("[reply] {message_id} history: {preview}"));
        Ok(())
    }

    /// A new attempt at a `failed` or `orphaned` reply, in place: its text
    /// and error are cleared and it goes back to `queued`, then starts.
    /// (Regenerate is different: it adds a sibling reply.)
    pub async fn retry(&self, owner: &str, message_id: &str, target: &Target) -> Result<(), RetryError> {
        // Still finishing its last attempt.
        if self.lock().hubs.contains_key(message_id) {
            return Err(RetryError::NotRetryable(MessageStatus::Running));
        }
        let (owner, mid) = (owner.to_string(), message_id.to_string());
        let max = self.chat.max_active_replies_per_user;
        let outcome = self
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let row: Option<(MessageStatus, String)> = tx
                    .query_row(
                        "SELECT m.status, m.conversation_id FROM messages m
                         JOIN conversations v ON v.id = m.conversation_id
                         WHERE m.id = ?1 AND v.owner_user_id = ?2 AND m.role = 'assistant'",
                        (&mid, &owner),
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let Some((status, conversation_id)) = row else {
                    return Ok(Err(RetryError::NotFound));
                };
                if !transition(&mid, status, MessageStatus::Queued) {
                    return Ok(Err(RetryError::NotRetryable(status)));
                }
                if let Some(busy) = conversations::check_limits(&tx, &owner, &conversation_id, max)? {
                    return Ok(Err(RetryError::Busy(busy)));
                }
                let now = now_ts();
                tx.execute(
                    "UPDATE messages SET status = 'queued', content = '', error = NULL, completed_at = NULL
                     WHERE id = ?1 AND status IN ('failed', 'orphaned')",
                    [&mid],
                )?;
                tx.execute(
                    "UPDATE conversations SET updated_at = ?1, active_leaf_id = ?2 WHERE id = ?3",
                    (&now, &mid, &conversation_id),
                )?;
                tx.commit()?;
                Ok(Ok(()))
            })
            .await
            .map_err(RetryError::Internal)?;
        outcome?;
        match self.start(message_id, target).await {
            Ok(()) | Err(StartError::Failed(_)) => Ok(()),
            Err(StartError::Internal(e)) => Err(RetryError::Internal(e)),
            Err(StartError::NotFound) => Err(RetryError::NotFound),
            Err(StartError::NotQueued) => Err(RetryError::NotRetryable(MessageStatus::Running)),
        }
    }

    /// The current text and status, and a receiver for what follows. None
    /// once the reply has ended: read it from the database instead.
    pub fn attach(&self, message_id: &str) -> Option<(Snapshot, broadcast::Receiver<ReplyEvent>)> {
        let inner = self.lock();
        let hub = inner.hubs.get(message_id)?;
        Some((hub.snapshot(message_id), hub.events.subscribe()))
    }

    /// The Stop button: tells the client to stop and marks the reply
    /// `cancelled`, keeping its text. Returns once that's committed.
    pub async fn cancel(&self, message_id: &str) {
        let committed = {
            let mut inner = self.lock();
            let still_running = inner.hubs.get(message_id).map(|hub| {
                if hub.ending.is_none() {
                    hub.tell_client_to_stop();
                }
                hub.ending.is_none()
            });
            match still_running {
                Some(running) => {
                    if running {
                        self.finish(&mut inner, message_id, MessageStatus::Cancelled, None);
                    }
                    let (ack, rx) = oneshot::channel();
                    if let Some(hub) = inner.hubs.get_mut(message_id) {
                        hub.acks.push(ack);
                    }
                    Some(rx)
                }
                None => None,
            }
        };
        match committed {
            Some(rx) => {
                let _ = tokio::time::timeout(Duration::from_secs(10), rx).await;
            }
            None => {
                // No hub: only possible for a reply caught between being
                // created and started. Mark it directly.
                let mid = message_id.to_string();
                let result = self
                    .db
                    .call(move |c| {
                        Ok(c.execute(
                            &format!(
                                "UPDATE messages SET status = 'cancelled', completed_at = ?1
                                 WHERE id = ?2 AND status IN ({})",
                                active_sql()
                            ),
                            (now_ts(), &mid),
                        )?)
                    })
                    .await;
                if let Err(e) = result {
                    log(format!("[reply] error cancelling {message_id}: {e:#}"));
                }
            }
        }
    }

    /// Stops every reply in a conversation (before deleting it).
    pub async fn cancel_conversation(&self, conversation_id: &str) {
        let ids: Vec<String> = {
            let inner = self.lock();
            inner
                .hubs
                .iter()
                .filter(|(_, h)| h.conversation_id == conversation_id)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in ids {
            self.cancel(&id).await;
        }
    }

    /// Stops every reply in a user's conversations (before deleting them).
    pub async fn cancel_owner(&self, owner_user_id: &str) {
        let ids: Vec<String> = {
            let inner = self.lock();
            inner
                .hubs
                .iter()
                .filter(|(_, h)| h.owner_user_id == owner_user_id)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in ids {
            self.cancel(&id).await;
        }
    }

    /// Replies each client is working on right now (sent to it and not yet
    /// ended), whoever asked for them, by client id. Clients with none are
    /// left out.
    pub fn active_jobs(&self) -> HashMap<String, usize> {
        let inner = self.lock();
        let mut jobs = HashMap::new();
        for hub in inner.hubs.values() {
            let working = matches!(hub.status, MessageStatus::Assigned | MessageStatus::Running);
            if let (true, None, Some(client_id)) = (working, &hub.ending, &hub.client_id) {
                *jobs.entry(client_id.clone()).or_insert(0) += 1;
            }
        }
        jobs
    }

    /// Brings messages read from the database up to date with replies in
    /// progress, whose text is only flushed every few seconds.
    pub fn overlay(&self, messages: &mut [Message]) {
        let inner = self.lock();
        for m in messages {
            if let Some(hub) = inner.hubs.get(&m.id) {
                m.content.clone_from(&hub.text);
                m.status = hub.status;
                if hub.client_label.is_some() {
                    m.client_id.clone_from(&hub.client_id);
                    m.client_label.clone_from(&hub.client_label);
                    m.model.clone_from(&hub.model);
                }
            }
        }
    }

    /// Called by the WebSocket loop for `chat_chunk`, `chat_done` and
    /// `chat_error`.
    pub fn on_client_event(&self, conn_id: u64, attempt_id: &str, event: ChatEvent) {
        let mut inner = self.lock();
        let Some(message_id) = inner.by_attempt.get(attempt_id).cloned() else {
            // Normal after a Stop or a timeout: chunks already in flight
            // still arrive.
            debug(|| {
                format!(
                    "[reply] dropped {} for unknown or finished attempt {attempt_id}",
                    kind(&event)
                )
            });
            return;
        };
        let Some(hub) = inner.hubs.get_mut(&message_id) else {
            return;
        };
        if hub.conn_id != Some(conn_id) || hub.ending.is_some() || hub.attempt_id.as_deref() != Some(attempt_id) {
            debug(|| {
                format!(
                    "[reply] dropped {} for {message_id} attempt {attempt_id} from connection {conn_id}",
                    kind(&event)
                )
            });
            return;
        }
        let ending = match event {
            ChatEvent::Chunk(delta) => {
                if hub.status == MessageStatus::Assigned && transition(&message_id, hub.status, MessageStatus::Running)
                {
                    hub.status = MessageStatus::Running;
                    let _ = hub.events.send(ReplyEvent::Status(hub.snapshot(&message_id)));
                    self.write(Write::Running {
                        message_id: message_id.clone(),
                        attempt_id: attempt_id.to_string(),
                    });
                }
                hub.last_activity = Instant::now();
                let added = delta.chars().count();
                if hub.chars + added > self.chat.max_reply_chars {
                    hub.tell_client_to_stop();
                    Some((
                        MessageStatus::Failed,
                        Some(format!(
                            "The reply grew past {} characters (CHAT_MAX_REPLY_CHARS) and was stopped.",
                            self.chat.max_reply_chars
                        )),
                    ))
                } else {
                    hub.text.push_str(&delta);
                    hub.chars += added;
                    hub.dirty = true;
                    let _ = hub.events.send(ReplyEvent::Chunk(delta));
                    None
                }
            }
            ChatEvent::Done => Some((MessageStatus::Completed, None)),
            ChatEvent::Error(error) => Some((MessageStatus::Failed, Some(error))),
        };
        if let Some((status, error)) = ending {
            self.finish(&mut inner, &message_id, status, error);
        }
    }

    /// Called by the WebSocket loop when a connection closes: its replies
    /// in progress are orphaned, keeping their partial text.
    pub fn on_connection_closed(&self, conn_id: u64) {
        let mut inner = self.lock();
        let ids: Vec<String> = inner
            .hubs
            .iter()
            .filter(|(_, h)| h.conn_id == Some(conn_id) && h.ending.is_none())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.finish(
                &mut inner,
                &id,
                MessageStatus::Orphaned,
                Some("The client disconnected before the reply finished.".into()),
            );
        }
    }

    /// Ends a reply: records the final status (if the move is legal) and
    /// queues the write. Listeners hear `End` once it's committed.
    fn finish(&self, inner: &mut Inner, message_id: &str, status: MessageStatus, error: Option<String>) -> bool {
        let Some(hub) = inner.hubs.get_mut(message_id) else {
            return false;
        };
        if hub.ending.is_some() || !transition(message_id, hub.status, status) {
            return false;
        }
        hub.ending = Some((status, error.clone()));
        hub.dirty = false;
        if let Some(attempt_id) = &hub.attempt_id {
            inner.by_attempt.remove(attempt_id);
        }
        let hub = &inner.hubs[message_id];
        match &error {
            Some(error) => log(format!("[reply] {message_id} {}: {error}", status.as_str())),
            None => log(format!(
                "[reply] {message_id} {} ({} chars)",
                status.as_str(),
                hub.chars
            )),
        }
        self.write(Write::Finished {
            message_id: message_id.to_string(),
            attempt_id: hub.attempt_id.clone(),
            conversation_id: hub.conversation_id.clone(),
            status,
            content: hub.text.clone(),
            error,
        });
        true
    }

    /// Once an ending is committed: tell listeners, wake Stop, drop the hub.
    fn after_commit(&self, finished: &[String]) {
        let mut inner = self.lock();
        for id in finished {
            if inner.hubs.get(id).is_some_and(|h| h.ending.is_some()) {
                let hub = inner.hubs.remove(id).expect("checked above");
                if let Some((status, error)) = hub.ending {
                    let _ = hub.events.send(ReplyEvent::End { status, error });
                }
                for ack in hub.acks {
                    let _ = ack.send(());
                }
            }
        }
    }

    async fn write_loop(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<Write>) {
        while let Some(first) = rx.recv().await {
            let mut batch = vec![first];
            while batch.len() < 256 {
                match rx.try_recv() {
                    Ok(write) => batch.push(write),
                    Err(_) => break,
                }
            }
            let finished: Vec<String> = batch
                .iter()
                .filter_map(|w| match w {
                    Write::Finished { message_id, .. } => Some(message_id.clone()),
                    _ => None,
                })
                .collect();
            let flushed: usize = batch
                .iter()
                .map(|w| match w {
                    Write::Progress(rows) => rows.len(),
                    _ => 0,
                })
                .sum();
            if let Err(e) = self.db.call(move |c| apply(c, batch)).await {
                log(format!("[reply] database write failed: {e:#}"));
            }
            if flushed > 0 {
                debug(|| format!("[reply] flushed the text of {flushed} reply(s) in progress"));
            }
            if !finished.is_empty() {
                self.after_commit(&finished);
            }
        }
    }

    async fn maintenance_loop(self: Arc<Self>) {
        let mut flush = tokio::time::interval(self.chat.flush_interval);
        flush.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut idle = tokio::time::interval(Duration::from_secs(1).min(self.chat.idle_timeout));
        idle.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = flush.tick() => self.flush(),
                _ = idle.tick() => self.expire_idle(),
            }
        }
    }

    /// Writes the text of every reply that changed since the last flush.
    fn flush(&self) {
        let mut inner = self.lock();
        let rows: Vec<(String, String, String)> = inner
            .hubs
            .iter_mut()
            .filter(|(_, h)| h.dirty && h.ending.is_none())
            .filter_map(|(id, h)| {
                h.dirty = false;
                Some((id.clone(), h.attempt_id.clone()?, h.text.clone()))
            })
            .collect();
        if !rows.is_empty() {
            self.write(Write::Progress(rows));
        }
    }

    /// Fails replies whose client has sent nothing for `CHAT_IDLE_TIMEOUT_MS`.
    fn expire_idle(&self) {
        let limit = self.chat.idle_timeout;
        let mut inner = self.lock();
        let quiet: Vec<String> = inner
            .hubs
            .iter()
            .filter(|(_, h)| {
                h.ending.is_none()
                    && matches!(h.status, MessageStatus::Assigned | MessageStatus::Running)
                    && h.last_activity.elapsed() >= limit
            })
            .map(|(id, h)| {
                h.tell_client_to_stop();
                id.clone()
            })
            .collect();
        for id in quiet {
            let error = format!("The client sent nothing for {} s.", limit.as_secs());
            self.finish(&mut inner, &id, MessageStatus::Failed, Some(error));
        }
    }

    #[cfg(test)]
    pub fn hub_count(&self) -> usize {
        self.lock().hubs.len()
    }
}

/// The writer's transaction. Every update is guarded by the status (and
/// attempt) it expects, as a second line of defence behind the ordering.
fn apply(conn: &mut Connection, batch: Vec<Write>) -> Result<()> {
    let tx = conn.transaction()?;
    let active = active_sql();
    for write in batch {
        let result = match &write {
            Write::Assigned {
                message_id,
                attempt_id,
                client_id,
                client_label,
                model,
            } => tx.execute(
                // The client may have been deleted meanwhile; then there's
                // no row to point at, and the label still says who it was.
                "UPDATE messages SET status = 'assigned', attempt_id = ?2, attempts = attempts + 1,
                   client_id = (SELECT client_id FROM clients WHERE client_id = ?3),
                   client_label = ?4, model = ?5, error = NULL
                 WHERE id = ?1 AND status = 'queued'",
                (message_id, attempt_id, client_id, client_label, model),
            ),
            Write::Running { message_id, attempt_id } => tx.execute(
                "UPDATE messages SET status = 'running' WHERE id = ?1 AND attempt_id = ?2 AND status = 'assigned'",
                (message_id, attempt_id),
            ),
            Write::Progress(rows) => {
                let mut result = Ok(0);
                for (message_id, attempt_id, content) in rows {
                    result = tx.execute(
                        "UPDATE messages SET content = ?3
                         WHERE id = ?1 AND attempt_id = ?2 AND status IN ('assigned', 'running')",
                        (message_id, attempt_id, content),
                    );
                    if result.is_err() {
                        break;
                    }
                }
                result
            }
            Write::Finished {
                message_id,
                attempt_id,
                conversation_id,
                status,
                content,
                error,
            } => {
                let now = now_ts();
                tx.execute(
                    &format!(
                        "UPDATE messages SET status = ?2, content = ?3, error = ?4, completed_at = ?5
                         WHERE id = ?1 AND status IN ({active}) AND (?6 IS NULL OR attempt_id = ?6)"
                    ),
                    (message_id, status, content, error, &now, attempt_id),
                )
                .and_then(|n| {
                    tx.execute(
                        "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
                        (&now, conversation_id),
                    )
                    .map(|_| n)
                })
            }
        };
        if let Err(e) = result {
            log(format!("[reply] write failed ({write:?}): {e}"));
        }
    }
    tx.commit()?;
    Ok(())
}

fn kind(event: &ChatEvent) -> &'static str {
    match event {
        ChatEvent::Chunk(_) => "chunk",
        ChatEvent::Done => "done",
        ChatEvent::Error(_) => "error",
    }
}

/// "user: first words… | assistant: …", for the debug log.
fn history_preview(history: &[ChatTurn]) -> String {
    history
        .iter()
        .map(|t| {
            let text: String = t.content.chars().take(40).collect();
            let more = if t.content.chars().count() > 40 { "…" } else { "" };
            format!("{:?}: {text:?}{more}", t.role).to_lowercase()
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MetricsLimits;
    use crate::conversations::{create_turn, NewTurn, TurnOutcome};
    use crate::db::tests::{add_client, add_user, memory_db};
    use crate::live::ConnHandle;
    use protocol::ModelInfo;

    fn chat_settings() -> ChatSettings {
        ChatSettings {
            idle_timeout: Duration::from_secs(60),
            flush_interval: Duration::from_millis(50),
            retention_days: 0,
            max_message_chars: 1000,
            max_reply_chars: 20,
            max_active_replies_per_user: 3,
        }
    }

    struct Fixture {
        db: Db,
        live: Arc<Live>,
        runner: Arc<ReplyRunner>,
        owner: String,
        conn: ConnHandle,
    }

    fn fixture() -> Fixture {
        let db = memory_db();
        let live = Arc::new(Live::new(MetricsLimits {
            history_samples: 10,
            min_spacing: Duration::ZERO,
            max_gpus: 4,
            max_string_len: 64,
        }));
        let owner = add_user(&db, "alice", "user");
        add_client(&db, "c1", &owner);
        let conn = live.register("c1");
        live.set_models("c1", conn.id, vec![ModelInfo { id: "echo".into() }]);
        let runner = ReplyRunner::new(db.clone(), live.clone(), chat_settings());
        Fixture {
            db,
            live,
            runner,
            owner,
            conn,
        }
    }

    fn target() -> Target {
        Target::Client {
            client_id: "c1".into(),
            model: "echo".into(),
        }
    }

    /// A new conversation with one queued reply; returns the reply id.
    fn queued_reply(f: &Fixture, conversation: &str) -> String {
        let owner = f.owner.clone();
        let conversation = conversation.to_string();
        f.db.blocking(|c| {
            match create_turn(
                c,
                NewTurn {
                    owner: &owner,
                    conversation_id: &conversation,
                    content: Some("hello"),
                    parent_id: None,
                    max_active_per_user: 10,
                },
            )? {
                TurnOutcome::Created(turn) => Ok(turn.reply.id),
                _ => panic!("not created"),
            }
        })
        .unwrap()
    }

    /// The attempt id of the request the client was sent.
    async fn sent_attempt(conn: &mut ConnHandle) -> String {
        match tokio::time::timeout(Duration::from_secs(2), conn.outbound.recv()).await {
            Ok(Some(ServerMessage::ChatRequest { request_id, .. })) => request_id,
            other => panic!("expected a chat_request, got {other:?}"),
        }
    }

    async fn db_state(f: &Fixture, id: &str) -> (MessageStatus, String) {
        let id = id.to_string();
        f.db.call(move |c| Ok(conversations::message_state(c, &id)?.unwrap()))
            .await
            .unwrap()
    }

    async fn wait_for_end(rx: &mut broadcast::Receiver<ReplyEvent>) -> (MessageStatus, Option<String>) {
        loop {
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Ok(ReplyEvent::End { status, error })) => return (status, error),
                Ok(Ok(_)) => continue,
                other => panic!("expected End, got {other:?}"),
            }
        }
    }

    #[test]
    fn every_allowed_status_change_and_only_those() {
        use MessageStatus::*;
        let allowed = [
            (Queued, Assigned),
            (Queued, Failed),
            (Queued, Cancelled),
            (Queued, Orphaned),
            (Assigned, Running),
            (Assigned, Completed),
            (Assigned, Failed),
            (Assigned, Cancelled),
            (Assigned, Orphaned),
            (Running, Completed),
            (Running, Failed),
            (Running, Cancelled),
            (Running, Orphaned),
            (Failed, Queued),
            (Orphaned, Queued),
        ];
        for from in MessageStatus::ALL {
            for to in MessageStatus::ALL {
                assert_eq!(
                    can_move(from, to),
                    allowed.contains(&(from, to)),
                    "{} -> {}",
                    from.as_str(),
                    to.as_str()
                );
            }
        }
        assert!(!can_move(Completed, Running));
        assert!(!can_move(Cancelled, Queued), "only failed and orphaned replies retry");
    }

    #[tokio::test]
    async fn a_reply_streams_flushes_and_completes() {
        let mut f = fixture();
        let id = queued_reply(&f, "conv");
        assert!(f.runner.active_jobs().is_empty());
        f.runner.start(&id, &target()).await.unwrap();
        let attempt = sent_attempt(&mut f.conn).await;
        let (snap, mut rx) = f.runner.attach(&id).unwrap();
        assert_eq!(snap.status, MessageStatus::Assigned);
        // Busy from the moment it's sent, before the first chunk.
        assert_eq!(f.runner.active_jobs().get("c1"), Some(&1));
        assert_eq!(snap.client_label.as_deref(), Some("host"));

        f.runner
            .on_client_event(f.conn.id, &attempt, ChatEvent::Chunk("hel".into()));
        f.runner
            .on_client_event(f.conn.id, &attempt, ChatEvent::Chunk("lo".into()));
        assert!(matches!(rx.recv().await.unwrap(), ReplyEvent::Status(s) if s.status == MessageStatus::Running));
        assert_eq!(rx.recv().await.unwrap(), ReplyEvent::Chunk("hel".into()));
        // Not written per chunk, but within a flush interval.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(db_state(&f, &id).await, (MessageStatus::Running, "hello".into()));

        assert_eq!(f.runner.active_jobs().get("c1"), Some(&1));
        f.runner.on_client_event(f.conn.id, &attempt, ChatEvent::Done);
        assert!(f.runner.active_jobs().is_empty(), "idle as soon as it ends");
        assert_eq!(wait_for_end(&mut rx).await, (MessageStatus::Completed, None));
        // Announced only after the commit.
        assert_eq!(db_state(&f, &id).await, (MessageStatus::Completed, "hello".into()));
        assert!(f.runner.attach(&id).is_none(), "the hub is gone");
        assert_eq!(f.runner.hub_count(), 0);
    }

    #[tokio::test]
    async fn events_from_an_old_attempt_or_another_connection_are_ignored() {
        let mut f = fixture();
        let id = queued_reply(&f, "conv");
        f.runner.start(&id, &target()).await.unwrap();
        let attempt = sent_attempt(&mut f.conn).await;
        let (_, mut rx) = f.runner.attach(&id).unwrap();

        f.runner
            .on_client_event(f.conn.id + 1, &attempt, ChatEvent::Chunk("injected".into()));
        f.runner
            .on_client_event(f.conn.id, "some-old-attempt", ChatEvent::Chunk("stale".into()));
        f.runner.on_client_event(f.conn.id + 1, &attempt, ChatEvent::Done);
        assert!(rx.try_recv().is_err(), "nothing reached the hub");

        // The connection drops: orphaned with what it had.
        f.runner
            .on_client_event(f.conn.id, &attempt, ChatEvent::Chunk("part".into()));
        f.runner.on_connection_closed(f.conn.id);
        assert_eq!(
            wait_for_end(&mut rx).await.0,
            MessageStatus::Orphaned,
            "a closed connection orphans its replies"
        );
        assert_eq!(db_state(&f, &id).await, (MessageStatus::Orphaned, "part".into()));

        // Retry: a fresh attempt. The old one's late chunks are dropped.
        f.runner.retry(&f.owner, &id, &target()).await.unwrap();
        let second = sent_attempt(&mut f.conn).await;
        assert_ne!(second, attempt);
        let (snap, mut rx) = f.runner.attach(&id).unwrap();
        assert_eq!(snap.content, "", "retry clears the text");
        f.runner
            .on_client_event(f.conn.id, &attempt, ChatEvent::Chunk("late".into()));
        f.runner
            .on_client_event(f.conn.id, &second, ChatEvent::Chunk("fresh".into()));
        f.runner.on_client_event(f.conn.id, &second, ChatEvent::Done);
        assert_eq!(wait_for_end(&mut rx).await.0, MessageStatus::Completed);
        assert_eq!(db_state(&f, &id).await, (MessageStatus::Completed, "fresh".into()));
        let id2 = id.clone();
        let attempts: i64 =
            f.db.call(move |c| Ok(c.query_row("SELECT attempts FROM messages WHERE id = ?1", [&id2], |r| r.get(0))?))
                .await
                .unwrap();
        assert_eq!(attempts, 2);
    }

    #[tokio::test]
    async fn stop_keeps_the_text_and_tells_the_client() {
        let mut f = fixture();
        let id = queued_reply(&f, "conv");
        f.runner.start(&id, &target()).await.unwrap();
        let attempt = sent_attempt(&mut f.conn).await;
        f.runner
            .on_client_event(f.conn.id, &attempt, ChatEvent::Chunk("so far".into()));
        f.runner.cancel(&id).await;
        assert!(matches!(
            f.conn.outbound.recv().await,
            Some(ServerMessage::ChatCancel { request_id }) if request_id == attempt
        ));
        assert_eq!(db_state(&f, &id).await, (MessageStatus::Cancelled, "so far".into()));
        // A cancelled reply can't be retried.
        assert!(matches!(
            f.runner.retry(&f.owner, &id, &target()).await,
            Err(RetryError::NotRetryable(MessageStatus::Cancelled))
        ));
    }

    #[tokio::test]
    async fn a_runaway_reply_is_stopped_at_the_limit() {
        let mut f = fixture();
        let id = queued_reply(&f, "conv");
        f.runner.start(&id, &target()).await.unwrap();
        let attempt = sent_attempt(&mut f.conn).await;
        let (_, mut rx) = f.runner.attach(&id).unwrap();
        f.runner
            .on_client_event(f.conn.id, &attempt, ChatEvent::Chunk("x".repeat(15)));
        f.runner
            .on_client_event(f.conn.id, &attempt, ChatEvent::Chunk("y".repeat(15)));
        let (status, error) = wait_for_end(&mut rx).await;
        assert_eq!(status, MessageStatus::Failed);
        assert!(error.unwrap().contains("CHAT_MAX_REPLY_CHARS"));
        assert_eq!(db_state(&f, &id).await.1, "x".repeat(15));
    }

    #[tokio::test]
    async fn a_disconnected_client_fails_the_reply_before_it_starts() {
        let f = fixture();
        f.live.unregister("c1", f.conn.id);
        let id = queued_reply(&f, "conv");
        assert!(matches!(
            f.runner.start(&id, &target()).await,
            Err(StartError::Failed(_))
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(db_state(&f, &id).await.0, MessageStatus::Failed);
    }

    #[tokio::test]
    async fn a_server_restart_orphans_replies_in_progress() {
        let f = fixture();
        let id = queued_reply(&f, "conv");
        let n = f.db.blocking(ReplyRunner::orphan_interrupted).unwrap();
        assert_eq!(n, 1);
        assert_eq!(db_state(&f, &id).await.0, MessageStatus::Orphaned);
    }

    #[tokio::test]
    async fn quiet_clients_time_out() {
        let mut f = fixture();
        let runner = ReplyRunner::new(
            f.db.clone(),
            f.live.clone(),
            ChatSettings {
                idle_timeout: Duration::from_millis(100),
                ..chat_settings()
            },
        );
        let id = queued_reply(&f, "conv");
        runner.start(&id, &target()).await.unwrap();
        let attempt = sent_attempt(&mut f.conn).await;
        let (_, mut rx) = runner.attach(&id).unwrap();
        let (status, error) = wait_for_end(&mut rx).await;
        assert_eq!(status, MessageStatus::Failed);
        assert!(error.unwrap().contains("sent nothing"));
        assert!(matches!(
            f.conn.outbound.recv().await,
            Some(ServerMessage::ChatCancel { request_id }) if request_id == attempt
        ));
    }
}
