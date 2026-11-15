//! The reply runner: the executor for `chat_reply` jobs, and everything
//! about generating an assistant reply.
//!
//! A reply is a job (`jobs/`). Sending or retrying one submits it to the
//! queue (`submit`, `retry`); the scheduler decides which client runs it and
//! calls `assign`, for directed and pooled replies alike. An attempt that is
//! lost (its client disconnected, went quiet or refused it) puts the same
//! reply back in the queue, until its attempts run out.
//!
//! Each reply that hasn't ended has an in-memory hub holding its text so
//! far and a broadcast channel, from the moment it's queued. HTTP handlers
//! stream from the hub (`attach`), but closing a stream never cancels
//! anything: a reply keeps going when the browser goes away, until it
//! completes, fails, runs out of attempts, or someone presses Stop
//! (`cancel`).
//!
//! This module owns every status transition after a reply is created, the
//! periodic flushes of its text, and all of its database writes, which
//! change the job and its message in the same transaction so the two never
//! disagree. Those writes go through one writer task, in order, so a late
//! write can never overtake an earlier one. A reply's ending is announced
//! to listeners only once it has been committed, so anything that sees
//! "completed" can rely on the database saying so too.
//!
//! Every attempt at a reply gets a new `attempt_id`, sent to the client as
//! the relay's `request_id`. Chunks and endings are applied only from the
//! current attempt on the connection it was sent to; anything else (an old
//! attempt still talking after a retry or a re-queue, another connection)
//! is dropped and logged at debug level.

use crate::config::ChatSettings;
use crate::conversations::{self, active_sql, Busy, Message, MessageStatus};
use crate::db::{now_ts, ts, Db};
use crate::jobs::scheduler::{Enqueue, Placement, Scheduler};
use crate::jobs::store::{self, QueuedReply};
use crate::jobs::{can_move, AttemptOutcome, JobKind, JobSettings, QueueInfo};
use crate::live::usable;
use crate::live::Live;
use crate::{debug, log};
use anyhow::Result;
use protocol::{Backend, ChatErrorCode, ChatTurn, InferenceSettings, ServerMessage, Stage};
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

pub use crate::jobs::Target;

/// What a client sent about an attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatEvent {
    /// Alive without text yet (e.g. loading its model). Resets the idle timeout.
    Progress(Stage),
    Chunk(String),
    /// With the client's token counts and speed, if it sent them.
    Done(Option<protocol::Usage>),
    /// `code` is what newer clients say about why, so a busy client's
    /// request can go back to the queue. None from older clients.
    Error {
        error: String,
        code: Option<ChatErrorCode>,
    },
}

/// What listeners on a hub receive after the snapshot.
#[derive(Debug, Clone, PartialEq)]
pub enum ReplyEvent {
    Status(Snapshot),
    /// The client moved to a new stage (loading the model, generating).
    Progress(Stage),
    Chunk(String),
    /// The attempt was lost and the reply went back to the queue. Its text
    /// so far is discarded; a `Status` (queued) follows.
    Requeued {
        /// The attempt that will be made next, counting from 1.
        attempt: u32,
        max_attempts: u32,
        /// `lost`, `timed_out` or `refused`.
        reason: AttemptOutcome,
        previous_client_label: Option<String>,
    },
    /// Last event: the final status, once it's in the database.
    End {
        status: MessageStatus,
        error: Option<String>,
        /// What `meta_json` now holds (usage, history trimming).
        meta: Option<serde_json::Value>,
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
    /// The client's latest `chat_progress`, so a stream that attaches while
    /// the model loads can say so.
    pub stage: Option<Stage>,
    /// While it's queued: where it stands and what it's waiting for. Null
    /// until the scheduler has looked at it, and once it's assigned.
    pub queue: Option<QueueInfo>,
    /// Attempts made so far, the current one included.
    pub attempt: u32,
    pub max_attempts: u32,
}

#[derive(Debug)]
pub enum RetryError {
    NotFound,
    /// Only `failed` and `orphaned` replies can be retried.
    NotRetryable(MessageStatus),
    Busy(Busy),
    Internal(anyhow::Error),
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
    job_id: String,
    conversation_id: String,
    owner_user_id: String,
    /// Where the job may run: one client, or the owner's pool.
    target: Target,
    /// Which time in the queue this is; a placement for an earlier one is
    /// ignored.
    ticket: u64,
    /// Attempts that reached a client, refusals not counted.
    attempts: u32,
    max_attempts: u32,
    /// Refusals in a row.
    refusals: u32,
    attempt_id: Option<String>,
    conn_id: Option<u64>,
    client_tx: Option<mpsc::Sender<ServerMessage>>,
    client_id: Option<String>,
    client_label: Option<String>,
    model: Option<String>,
    /// This attempt's `meta_json` so far (history trimming); the usage is
    /// added when it's done.
    meta: Option<serde_json::Value>,
    /// Always an active status: what listeners see until the ending commits.
    status: MessageStatus,
    stage: Option<Stage>,
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
    fn new(job: QueuedReply, ticket: u64) -> Self {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        Hub {
            job_id: job.job_id,
            conversation_id: job.conversation_id,
            owner_user_id: job.owner_user_id,
            target: job.target,
            ticket,
            attempts: job.attempts,
            max_attempts: job.max_attempts,
            refusals: 0,
            attempt_id: None,
            conn_id: None,
            client_tx: None,
            client_id: None,
            client_label: None,
            model: None,
            meta: None,
            status: MessageStatus::Queued,
            stage: None,
            ending: None,
            text: String::new(),
            chars: 0,
            dirty: false,
            last_activity: Instant::now(),
            events,
            acks: Vec::new(),
        }
    }

    fn snapshot(&self, message_id: &str, scheduler: &Scheduler) -> Snapshot {
        let waiting = self.status == MessageStatus::Queued && self.ending.is_none();
        Snapshot {
            message_id: message_id.to_string(),
            status: self.status,
            content: self.text.clone(),
            error: None,
            client_id: self.client_id.clone(),
            client_label: self.client_label.clone(),
            model: self.model.clone(),
            stage: self.stage,
            queue: waiting.then(|| scheduler.queue_info(&self.job_id)).flatten(),
            attempt: self.attempts,
            max_attempts: self.max_attempts,
        }
    }

    fn working(&self) -> bool {
        self.ending.is_none() && matches!(self.status, MessageStatus::Assigned | MessageStatus::Running)
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

/// One database write, applied in order by the writer task. Each changes
/// the job and its message together.
#[derive(Debug)]
enum Write {
    Assigned {
        job_id: String,
        message_id: String,
        attempt_id: String,
        /// This attempt's number, from 1.
        number: u32,
        client_id: String,
        client_label: String,
        model: String,
        /// `meta_json` for this attempt (how much history was trimmed).
        meta: Option<String>,
        at: String,
    },
    Running {
        job_id: String,
        message_id: String,
        attempt_id: String,
        at: String,
    },
    /// The flush: (message id, attempt id, text so far).
    Progress(Vec<(String, String, String)>),
    /// The attempt ended without ending the job: back to the queue.
    Requeued {
        job_id: String,
        message_id: String,
        attempt_id: String,
        outcome: AttemptOutcome,
        error: String,
        /// A refusal doesn't use up an attempt: take it off the count again.
        refund: bool,
        queued_at: String,
        deadline_at: String,
        not_before: Option<String>,
    },
    Finished {
        job_id: String,
        message_id: String,
        /// The attempt this ends, if one was open.
        attempt_id: Option<String>,
        conversation_id: String,
        status: MessageStatus,
        outcome: AttemptOutcome,
        content: String,
        error: Option<String>,
        /// Replaces `meta_json` when set.
        meta: Option<String>,
        refund: bool,
    },
}

/// How a reply ends.
struct Ending {
    status: MessageStatus,
    /// What to record for the attempt this ends, if one is open.
    outcome: AttemptOutcome,
    error: Option<String>,
    refund: bool,
}

impl Ending {
    fn new(status: MessageStatus, outcome: AttemptOutcome, error: Option<String>) -> Self {
        Ending {
            status,
            outcome,
            error,
            refund: false,
        }
    }

    fn failed(error: String) -> Self {
        Ending::new(MessageStatus::Failed, AttemptOutcome::Failed, Some(error))
    }

    fn cancelled() -> Self {
        Ending::new(MessageStatus::Cancelled, AttemptOutcome::Cancelled, None)
    }
}

const EVENT_BUFFER: usize = 256;

pub struct ReplyRunner {
    db: Db,
    live: Arc<Live>,
    chat: ChatSettings,
    /// `max_tokens` and `default_context`, for trimming history.
    inference: InferenceSettings,
    jobs: JobSettings,
    /// The queue, and the task that decides which client runs what.
    scheduler: Arc<Scheduler>,
    inner: Mutex<Inner>,
    writes: mpsc::UnboundedSender<Write>,
}

impl ReplyRunner {
    /// Starts the writer, the flush/idle-timeout task and the scheduler.
    pub fn new(
        db: Db,
        live: Arc<Live>,
        chat: ChatSettings,
        inference: InferenceSettings,
        jobs: JobSettings,
    ) -> Arc<Self> {
        let (writes, rx) = mpsc::unbounded_channel();
        let scheduler = Scheduler::new(
            db.clone(),
            live.clone(),
            jobs,
            inference.max_concurrent as usize,
            inference.echo_model_enabled,
        );
        let runner = Arc::new(ReplyRunner {
            db,
            live,
            chat,
            inference,
            jobs,
            scheduler: scheduler.clone(),
            inner: Mutex::new(Inner::default()),
            writes,
        });
        tokio::spawn(runner.clone().write_loop(rx));
        tokio::spawn(runner.clone().maintenance_loop());
        tokio::spawn(scheduler.run(runner.clone()));
        runner
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self, write: Write) {
        // Only fails if the writer task is gone, i.e. the runtime is.
        let _ = self.writes.send(write);
    }

    /// The queue: how full it is, and what each client is doing.
    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    /// Puts a queued reply's job in the queue: creates its hub, so streams
    /// attach at once and see `queued`, and wakes the scheduler. The job row
    /// already exists (it's created with the reply, or by `retry`). Also
    /// how the jobs `store::recover` found queued are loaded at startup.
    pub fn submit(&self, job: QueuedReply) {
        let mut inner = self.lock();
        if inner.hubs.contains_key(&job.message_id) {
            return;
        }
        let ticket = self.scheduler.enqueue(Enqueue {
            job_id: job.job_id.clone(),
            kind: JobKind::ChatReply,
            subject: job.message_id.clone(),
            owner_user_id: job.owner_user_id.clone(),
            target: job.target.clone(),
            avoid_client: None,
            not_before: None,
        });
        inner.hubs.insert(job.message_id.clone(), Hub::new(job, ticket));
    }

    /// Manual Retry of a `failed` or `orphaned` reply, in place: its text
    /// and error are cleared and its job goes back to the queue aimed at
    /// `target`, with a fresh budget of `JOB_MAX_ATTEMPTS` attempts. A reply
    /// from before the job queue gets a job now. (Regenerate is different:
    /// it adds a sibling reply.)
    pub async fn retry(&self, owner: &str, message_id: &str, target: &Target) -> Result<(), RetryError> {
        // Still finishing its last attempt.
        if self.lock().hubs.contains_key(message_id) {
            return Err(RetryError::NotRetryable(MessageStatus::Running));
        }
        let (owner, mid, target) = (owner.to_string(), message_id.to_string(), target.clone());
        let max = self.chat.max_active_replies_per_user;
        let (budget, queue_timeout) = (self.jobs.max_attempts, self.jobs.queue_timeout);
        let outcome = self
            .db
            .call(move |c| {
                let tx = c.transaction()?;
                let row: Option<(MessageStatus, String, u32)> = tx
                    .query_row(
                        "SELECT m.status, m.conversation_id, m.attempts FROM messages m
                         JOIN conversations v ON v.id = m.conversation_id
                         WHERE m.id = ?1 AND v.owner_user_id = ?2 AND m.role = 'assistant'",
                        (&mid, &owner),
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()?;
                let Some((status, conversation_id, message_attempts)) = row else {
                    return Ok(Err(RetryError::NotFound));
                };
                if !transition(&mid, status, MessageStatus::Queued) {
                    return Ok(Err(RetryError::NotRetryable(status)));
                }
                if let Some(busy) = conversations::check_limits(&tx, &owner, &conversation_id, max)? {
                    return Ok(Err(RetryError::Busy(busy)));
                }
                let now = now_ts();
                let deadline_at = ts(SystemTime::now() + queue_timeout);
                let (job_id, attempts, max_attempts) = match store::job_of_message(&tx, &mid)? {
                    Some(job_id) => {
                        let (attempts, max_attempts) =
                            store::requeue_for_retry(&tx, &job_id, &target, budget, &now, &deadline_at)?;
                        (job_id, attempts, max_attempts)
                    }
                    // A reply from before the job queue: its attempts so far
                    // carry over, so the count on the message stays right.
                    None => {
                        let job_id = Uuid::new_v4().to_string();
                        let max_attempts = message_attempts + budget;
                        store::insert_job(
                            &tx,
                            &store::NewJob {
                                id: &job_id,
                                owner_user_id: &owner,
                                message_id: &mid,
                                target: &target,
                                attempts: message_attempts,
                                max_attempts,
                                now: &now,
                                deadline_at: &deadline_at,
                            },
                        )?;
                        (job_id, message_attempts, max_attempts)
                    }
                };
                tx.execute(
                    "UPDATE messages SET status = 'queued', content = '', error = NULL, completed_at = NULL,
                       attempt_id = NULL, attempts = ?2, client_id = NULL, client_label = NULL, model = NULL,
                       meta_json = NULL
                     WHERE id = ?1 AND status IN ('failed', 'orphaned')",
                    (&mid, attempts),
                )?;
                tx.execute(
                    "UPDATE conversations SET updated_at = ?1, active_leaf_id = ?2 WHERE id = ?3",
                    (&now, &mid, &conversation_id),
                )?;
                tx.commit()?;
                Ok(Ok(QueuedReply {
                    job_id,
                    message_id: mid,
                    conversation_id,
                    owner_user_id: owner,
                    target,
                    attempts,
                    max_attempts,
                }))
            })
            .await
            .map_err(RetryError::Internal)?;
        self.submit(outcome?);
        Ok(())
    }

    /// The scheduler chose `client_id` for a queued reply: starts an attempt
    /// and sends the request. The history is trimmed here, not at submit,
    /// because contexts differ from client to client.
    pub async fn assign(&self, placement: Placement) {
        let message_id = placement.subject.as_str();
        let (job_id, ticket) = (placement.job_id.as_str(), placement.ticket);
        let (client_id, label) = (placement.client_id.clone(), placement.client_label.clone());
        let model = placement.target.model().to_string();

        let mid = message_id.to_string();
        let history = match self.db.call(move |c| conversations::history_for(c, &mid)).await {
            Ok(history) => history,
            Err(e) => {
                log(format!("[reply] couldn't read the history of {message_id}: {e:#}"));
                self.scheduler.release_ticket(job_id, ticket);
                let mut inner = self.lock();
                self.finish(
                    &mut inner,
                    message_id,
                    Ending::failed("The server couldn't read the conversation.".into()),
                );
                return;
            }
        };
        // The client went away, or lost the model, since it was chosen: the
        // job waits on for another pass.
        let echo_off = !self.inference.echo_model_enabled;
        let conn = self.live.sender(&client_id);
        let local = conn.as_ref().and_then(|conn| {
            conn.model(&model)
                .filter(|m| usable(m.state) && !(echo_off && m.backend == Backend::Echo))
        });
        let (Some(conn), Some(local)) = (&conn, local) else {
            self.scheduler.put_back(job_id, ticket);
            return;
        };

        // Fit the history in the model's context, leaving room for the reply.
        let context = local
            .context_length
            .or((local.backend == Backend::LlamaCpp).then_some(self.inference.default_context));
        let (history, dropped) = match context {
            Some(context) => {
                let budget = u64::from(context).saturating_sub(u64::from(self.inference.max_tokens));
                conversations::trim_history(history, budget)
            }
            None => (history, 0),
        };
        let meta_value = (dropped > 0).then(|| serde_json::json!({ "history_turns_dropped": dropped }));
        let meta = meta_value.as_ref().map(|v| v.to_string());

        let attempt_id = Uuid::new_v4().to_string();
        let turns = history.len();
        let preview = history_preview(&history);
        let mut inner = self.lock();
        let Inner { hubs, by_attempt } = &mut *inner;
        // Stopped, deleted, or queued again while we were looking things up.
        let current = hubs.get_mut(message_id).filter(|hub| {
            hub.ticket == ticket
                && hub.ending.is_none()
                && hub.status == MessageStatus::Queued
                && transition(message_id, hub.status, MessageStatus::Assigned)
        });
        let Some(hub) = current else {
            drop(inner);
            self.scheduler.release_ticket(job_id, ticket);
            return;
        };
        hub.status = MessageStatus::Assigned;
        hub.attempts += 1;
        hub.attempt_id = Some(attempt_id.clone());
        hub.conn_id = Some(conn.conn_id);
        hub.client_tx = Some(conn.tx.clone());
        hub.client_id = Some(client_id.clone());
        hub.client_label = Some(label.clone());
        hub.model = Some(model.clone());
        hub.meta = meta_value;
        hub.stage = None;
        hub.last_activity = Instant::now();
        let number = hub.attempts;
        let _ = hub
            .events
            .send(ReplyEvent::Status(hub.snapshot(message_id, &self.scheduler)));
        by_attempt.insert(attempt_id.clone(), message_id.to_string());
        self.write(Write::Assigned {
            job_id: job_id.to_string(),
            message_id: message_id.to_string(),
            attempt_id: attempt_id.clone(),
            number,
            client_id: client_id.clone(),
            client_label: label.clone(),
            model: model.clone(),
            meta,
            at: now_ts(),
        });
        let request = ServerMessage::ChatRequest {
            request_id: attempt_id.clone(),
            model: model.clone(),
            messages: history,
            temperature: None,
        };
        if conn.tx.try_send(request).is_err() {
            // Its outbound queue is full: a refusal, not a failure.
            self.attempt_lost(
                &mut inner,
                message_id,
                AttemptOutcome::Refused,
                format!("{label} couldn't take the request (its outbound queue was full)"),
            );
            return;
        }
        drop(inner);
        let trimmed = if dropped > 0 {
            format!(", {dropped} older one(s) trimmed to fit the context")
        } else {
            String::new()
        };
        log(format!(
            "[reply] {message_id} attempt {number} ({attempt_id}) sent to {client_id} ({model}, {turns} \
             message(s){trimmed})"
        ));
        debug(|| format!("[reply] {message_id} history: {preview}"));
    }

    /// The scheduler gave up on a queued reply: it wasn't placed before its
    /// deadline, or the client it's pinned to no longer exists.
    pub fn fail_queued(&self, message_id: &str, ticket: u64, error: String) {
        let mut inner = self.lock();
        let waiting = inner
            .hubs
            .get(message_id)
            .is_some_and(|hub| hub.ticket == ticket && hub.status == MessageStatus::Queued);
        if waiting {
            self.finish(&mut inner, message_id, Ending::failed(error));
        }
    }

    /// The scheduler's view of these queued replies changed (their position,
    /// or what they're waiting for): tell their listeners.
    pub fn queue_changed(&self, message_ids: &[String]) {
        let inner = self.lock();
        for id in message_ids {
            if let Some(hub) = inner.hubs.get(id) {
                if hub.status == MessageStatus::Queued && hub.ending.is_none() {
                    let _ = hub.events.send(ReplyEvent::Status(hub.snapshot(id, &self.scheduler)));
                }
            }
        }
    }

    /// The current text and status, and a receiver for what follows. None
    /// once the reply has ended: read it from the database instead.
    pub fn attach(&self, message_id: &str) -> Option<(Snapshot, broadcast::Receiver<ReplyEvent>)> {
        let inner = self.lock();
        let hub = inner.hubs.get(message_id)?;
        Some((hub.snapshot(message_id, &self.scheduler), hub.events.subscribe()))
    }

    /// The Stop button: tells the client to stop (if one has the reply; a
    /// queued one just leaves the queue) and marks the reply `cancelled`,
    /// keeping its text. Returns once that's committed.
    pub async fn cancel(&self, message_id: &str) {
        let committed = {
            let mut inner = self.lock();
            self.end_now(&mut inner, message_id, Ending::cancelled())
        };
        match committed {
            Some(rx) => {
                let _ = tokio::time::timeout(Duration::from_secs(10), rx).await;
            }
            None => {
                // No hub: only possible for a reply caught between being
                // created and submitted. Mark it directly.
                let mid = message_id.to_string();
                let result = self
                    .db
                    .call(move |c| {
                        let tx = c.transaction()?;
                        let (active, now) = (active_sql(), now_ts());
                        tx.execute(
                            &format!(
                                "UPDATE jobs SET status = 'cancelled', finished_at = ?1
                                 WHERE message_id = ?2 AND status IN ({active})"
                            ),
                            (&now, &mid),
                        )?;
                        tx.execute(
                            &format!(
                                "UPDATE messages SET status = 'cancelled', completed_at = ?1
                                 WHERE id = ?2 AND status IN ({active})"
                            ),
                            (&now, &mid),
                        )?;
                        tx.commit()?;
                        Ok(())
                    })
                    .await;
                if let Err(e) = result {
                    log(format!("[reply] error cancelling {message_id}: {e:#}"));
                }
            }
        }
    }

    /// Ends a reply on someone's say-so (telling its client to stop), and
    /// returns a receiver that fires once the ending is committed. None if
    /// it has no hub.
    fn end_now(&self, inner: &mut Inner, message_id: &str, ending: Ending) -> Option<oneshot::Receiver<()>> {
        let still_going = {
            let hub = inner.hubs.get(message_id)?;
            if hub.ending.is_none() {
                hub.tell_client_to_stop();
            }
            hub.ending.is_none()
        };
        if still_going {
            self.finish(inner, message_id, ending);
        }
        let (ack, rx) = oneshot::channel();
        inner.hubs.get_mut(message_id)?.acks.push(ack);
        Some(rx)
    }

    /// Ends every reply `matches` picks, and waits for those endings to be
    /// committed.
    async fn end_all(&self, matches: impl Fn(&Hub) -> bool, ending: impl Fn() -> Ending) {
        let waits: Vec<oneshot::Receiver<()>> = {
            let mut inner = self.lock();
            let ids: Vec<String> = inner
                .hubs
                .iter()
                .filter(|(_, hub)| matches(hub))
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter()
                .filter_map(|id| self.end_now(&mut inner, id, ending()))
                .collect()
        };
        for rx in waits {
            let _ = tokio::time::timeout(Duration::from_secs(10), rx).await;
        }
    }

    /// Stops every reply in a conversation (before deleting it).
    pub async fn cancel_conversation(&self, conversation_id: &str) {
        self.end_all(|hub| hub.conversation_id == conversation_id, Ending::cancelled)
            .await;
    }

    /// Stops every reply in a user's conversations (before deleting them).
    pub async fn cancel_owner(&self, owner_user_id: &str) {
        self.end_all(|hub| hub.owner_user_id == owner_user_id, Ending::cancelled)
            .await;
    }

    /// Before a client is deleted: every reply pinned to it fails, whoever
    /// owns it, since it can never run there again. (Pooled replies it was
    /// running are re-queued when its connection closes.)
    pub async fn fail_pinned_to(&self, client_id: &str) {
        self.end_all(
            |hub| hub.target.pinned_client() == Some(client_id),
            || Ending::failed("That client was deleted.".into()),
        )
        .await;
    }

    /// Replies each client is working on right now (from the moment it's
    /// chosen for one until the attempt ends), whoever asked for them, by
    /// client id. Clients with none are left out.
    pub fn active_jobs(&self) -> HashMap<String, usize> {
        self.scheduler.active_jobs()
    }

    /// Brings messages read from the database up to date with replies in
    /// progress, whose text is only flushed every few seconds.
    pub fn overlay(&self, messages: &mut [Message]) {
        let inner = self.lock();
        for m in messages {
            if let Some(hub) = inner.hubs.get(&m.id) {
                m.content.clone_from(&hub.text);
                m.status = hub.status;
                m.attempts = i64::from(hub.attempts);
                m.client_id.clone_from(&hub.client_id);
                m.client_label.clone_from(&hub.client_label);
                m.model.clone_from(&hub.model);
            }
        }
    }

    /// Called by the WebSocket loop for `chat_progress`, `chat_chunk`,
    /// `chat_done` and `chat_error`.
    pub fn on_client_event(&self, conn_id: u64, attempt_id: &str, event: ChatEvent) {
        let mut inner = self.lock();
        let Some(message_id) = inner.by_attempt.get(attempt_id).cloned() else {
            // Normal after a Stop, a timeout or a re-queue: chunks already
            // in flight still arrive, and a client that reconnects may keep
            // talking about an attempt that's no longer the current one.
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
        /// What an event leads to.
        enum Next {
            Carry,
            End(Ending),
            Requeue(String),
        }
        let next = match event {
            ChatEvent::Progress(stage) => {
                hub.last_activity = Instant::now();
                if hub.stage != Some(stage) {
                    hub.stage = Some(stage);
                    let _ = hub.events.send(ReplyEvent::Progress(stage));
                }
                Next::Carry
            }
            ChatEvent::Chunk(delta) => {
                if hub.status == MessageStatus::Assigned && transition(&message_id, hub.status, MessageStatus::Running)
                {
                    hub.status = MessageStatus::Running;
                    // It took the request after all.
                    hub.refusals = 0;
                    let _ = hub
                        .events
                        .send(ReplyEvent::Status(hub.snapshot(&message_id, &self.scheduler)));
                    self.write(Write::Running {
                        job_id: hub.job_id.clone(),
                        message_id: message_id.clone(),
                        attempt_id: attempt_id.to_string(),
                        at: now_ts(),
                    });
                }
                hub.last_activity = Instant::now();
                let added = delta.chars().count();
                if hub.chars + added > self.chat.max_reply_chars {
                    hub.tell_client_to_stop();
                    Next::End(Ending::failed(format!(
                        "The reply grew past {} characters (CHAT_MAX_REPLY_CHARS) and was stopped.",
                        self.chat.max_reply_chars
                    )))
                } else {
                    hub.text.push_str(&delta);
                    hub.chars += added;
                    hub.dirty = true;
                    let _ = hub.events.send(ReplyEvent::Chunk(delta));
                    Next::Carry
                }
            }
            ChatEvent::Done(usage) => {
                if let Some(usage) = usage {
                    record_usage(hub, &usage);
                }
                Next::End(Ending::new(MessageStatus::Completed, AttemptOutcome::Completed, None))
            }
            ChatEvent::Error { error, code } => {
                let refused = match code {
                    Some(ChatErrorCode::Busy | ChatErrorCode::BusyWithAnotherModel) => true,
                    // Another client in the pool may have the model; a
                    // pinned job has nowhere else to go.
                    Some(ChatErrorCode::ModelUnavailable) => matches!(hub.target, Target::Pool { .. }),
                    // No code is an older client: a failure, as it always was.
                    Some(ChatErrorCode::Other) | None => false,
                };
                if refused {
                    Next::Requeue(error)
                } else {
                    // The same input usually fails the same way, so this
                    // isn't retried automatically. A person can Retry.
                    Next::End(Ending::failed(error))
                }
            }
        };
        match next {
            Next::Carry => {}
            Next::End(ending) => {
                self.finish(&mut inner, &message_id, ending);
            }
            Next::Requeue(error) => self.attempt_lost(&mut inner, &message_id, AttemptOutcome::Refused, error),
        }
    }

    /// Called by the WebSocket loop when a connection closes: the replies it
    /// was generating go back to the queue (or end `orphaned` once their
    /// attempts run out).
    pub fn on_connection_closed(&self, conn_id: u64) {
        let mut inner = self.lock();
        let ids: Vec<String> = inner
            .hubs
            .iter()
            .filter(|(_, h)| h.conn_id == Some(conn_id) && h.working())
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.attempt_lost(
                &mut inner,
                &id,
                AttemptOutcome::Lost,
                "The client disconnected before the reply finished".into(),
            );
        }
    }

    /// The current attempt ended without an answer and through no fault of
    /// the request: `lost`, `timed_out` or `refused`. The reply goes back to
    /// the queue to run again (on another client if it's pooled, on the same
    /// one once it's free or back if it's pinned), unless it has used up its
    /// attempts, or been refused too many times in a row. `detail` says what
    /// happened, without a full stop.
    fn attempt_lost(&self, inner: &mut Inner, message_id: &str, reason: AttemptOutcome, detail: String) {
        let Some(hub) = inner.hubs.get_mut(message_id) else {
            return;
        };
        if !hub.working() {
            return;
        }
        let refused = reason == AttemptOutcome::Refused;
        if refused {
            hub.refusals += 1;
            if hub.refusals >= self.jobs.max_refusals {
                let error = format!("Refused {} times in a row; the last time: {detail}", hub.refusals);
                let ending = Ending {
                    refund: true,
                    ..Ending::new(MessageStatus::Failed, reason, Some(error))
                };
                self.finish(inner, message_id, ending);
                return;
            }
        } else {
            hub.refusals = 0;
            if hub.attempts >= hub.max_attempts {
                let n = hub.attempts;
                let error = format!("{detail} ({n} attempt{}).", if n == 1 { "" } else { "s" });
                self.finish(
                    inner,
                    message_id,
                    Ending::new(MessageStatus::Orphaned, reason, Some(error)),
                );
                return;
            }
        }
        if !transition(message_id, hub.status, MessageStatus::Queued) {
            return;
        }
        let Some(attempt_id) = hub.attempt_id.take() else {
            return;
        };
        let previous_client_label = hub.client_label.take();
        let lost_by = hub.client_id.take();
        hub.status = MessageStatus::Queued;
        hub.conn_id = None;
        hub.client_tx = None;
        hub.model = None;
        hub.meta = None;
        hub.stage = None;
        // The next attempt starts again from nothing.
        hub.text.clear();
        hub.chars = 0;
        hub.dirty = false;
        if refused {
            // A refusal doesn't use up an attempt.
            hub.attempts = hub.attempts.saturating_sub(1);
        }
        let backoff = refused.then_some(self.jobs.refused_backoff);
        hub.ticket = self.scheduler.enqueue(Enqueue {
            job_id: hub.job_id.clone(),
            kind: JobKind::ChatReply,
            subject: message_id.to_string(),
            owner_user_id: hub.owner_user_id.clone(),
            target: hub.target.clone(),
            avoid_client: lost_by,
            not_before: backoff.map(|wait| tokio::time::Instant::now() + wait),
        });
        log(format!(
            "[reply] {message_id} back in the queue ({}): {detail}",
            reason.as_str()
        ));
        let _ = hub.events.send(ReplyEvent::Requeued {
            attempt: hub.attempts + 1,
            max_attempts: hub.max_attempts,
            reason,
            previous_client_label,
        });
        let _ = hub
            .events
            .send(ReplyEvent::Status(hub.snapshot(message_id, &self.scheduler)));
        let now = SystemTime::now();
        let write = Write::Requeued {
            job_id: hub.job_id.clone(),
            message_id: message_id.to_string(),
            attempt_id: attempt_id.clone(),
            outcome: reason,
            error: detail,
            refund: refused,
            queued_at: ts(now),
            deadline_at: ts(now + self.jobs.queue_timeout),
            not_before: backoff.map(|wait| ts(now + wait)),
        };
        inner.by_attempt.remove(&attempt_id);
        self.write(write);
    }

    /// Ends a reply: records the final status (if the move is legal), frees
    /// its place in the queue or on its client, and queues the write.
    /// Listeners hear `End` once it's committed.
    fn finish(&self, inner: &mut Inner, message_id: &str, ending: Ending) -> bool {
        let Some(hub) = inner.hubs.get_mut(message_id) else {
            return false;
        };
        if hub.ending.is_some() || !transition(message_id, hub.status, ending.status) {
            return false;
        }
        let Ending {
            status,
            outcome,
            error,
            refund,
        } = ending;
        hub.ending = Some((status, error.clone()));
        hub.dirty = false;
        if refund {
            hub.attempts = hub.attempts.saturating_sub(1);
        }
        if let Some(attempt_id) = &hub.attempt_id {
            inner.by_attempt.remove(attempt_id);
        }
        let hub = &inner.hubs[message_id];
        self.scheduler.release(&hub.job_id);
        match &error {
            Some(error) => log(format!("[reply] {message_id} {}: {error}", status.as_str())),
            None => log(format!(
                "[reply] {message_id} {} ({} chars)",
                status.as_str(),
                hub.chars
            )),
        }
        self.write(Write::Finished {
            job_id: hub.job_id.clone(),
            message_id: message_id.to_string(),
            attempt_id: hub.attempt_id.clone(),
            conversation_id: hub.conversation_id.clone(),
            status,
            outcome,
            content: hub.text.clone(),
            error,
            meta: hub.meta.as_ref().map(|m| m.to_string()),
            refund,
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
                    let _ = hub.events.send(ReplyEvent::End {
                        status,
                        error,
                        meta: hub.meta,
                    });
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

    /// Replies whose client has sent nothing for `CHAT_IDLE_TIMEOUT_MS`: the
    /// client is told to stop, and the reply goes back to the queue.
    fn expire_idle(&self) {
        let limit = self.chat.idle_timeout;
        let mut inner = self.lock();
        let quiet: Vec<String> = inner
            .hubs
            .iter()
            .filter(|(_, h)| h.working() && h.last_activity.elapsed() >= limit)
            .map(|(id, h)| {
                h.tell_client_to_stop();
                id.clone()
            })
            .collect();
        for id in quiet {
            let detail = format!("The client sent nothing for {} s", limit.as_secs());
            self.attempt_lost(&mut inner, &id, AttemptOutcome::TimedOut, detail);
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
    for write in batch {
        match apply_one(&tx, &write) {
            Ok(true) => {}
            // The job wasn't in the state this write expects, so nothing
            // was changed. Only a bug gets here.
            Ok(false) => log(format!("[reply] write skipped, the job had moved on ({write:?})")),
            Err(e) => log(format!("[reply] write failed ({write:?}): {e}")),
        }
    }
    tx.commit()?;
    Ok(())
}

/// One write: the job first, and the message and attempt only if the job
/// changed, so `jobs.status` and `messages.status` always agree. Returns
/// whether it applied.
fn apply_one(tx: &Transaction<'_>, write: &Write) -> rusqlite::Result<bool> {
    let active = active_sql();
    match write {
        Write::Assigned {
            job_id,
            message_id,
            attempt_id,
            number,
            client_id,
            client_label,
            model,
            meta,
            at,
        } => {
            // The client may have been deleted meanwhile; then there's no
            // row to point at, and the label still says who it was.
            let changed = tx.execute(
                "UPDATE jobs SET status = 'assigned', attempt_id = ?2, attempts = attempts + 1,
                   client_id = (SELECT client_id FROM clients WHERE client_id = ?3), error = NULL,
                   not_before = NULL
                 WHERE id = ?1 AND status = 'queued'",
                (job_id, attempt_id, client_id),
            )?;
            if changed == 0 {
                return Ok(false);
            }
            tx.execute(
                "UPDATE messages SET status = 'assigned', attempt_id = ?2, attempts = attempts + 1,
                   client_id = (SELECT client_id FROM clients WHERE client_id = ?3),
                   client_label = ?4, model = ?5, error = NULL, meta_json = ?6
                 WHERE id = ?1",
                (message_id, attempt_id, client_id, client_label, model, meta),
            )?;
            tx.execute(
                "INSERT INTO job_attempts (id, job_id, number, client_id, client_label, model, assigned_at)
                 VALUES (?1, ?2, ?3, (SELECT client_id FROM clients WHERE client_id = ?4), ?5, ?6, ?7)",
                (attempt_id, job_id, number, client_id, client_label, model, at),
            )?;
        }
        Write::Running {
            job_id,
            message_id,
            attempt_id,
            at,
        } => {
            let changed = tx.execute(
                "UPDATE jobs SET status = 'running' WHERE id = ?1 AND attempt_id = ?2 AND status = 'assigned'",
                (job_id, attempt_id),
            )?;
            if changed == 0 {
                return Ok(false);
            }
            tx.execute("UPDATE messages SET status = 'running' WHERE id = ?1", [message_id])?;
            tx.execute(
                "UPDATE job_attempts SET first_output_at = ?2 WHERE id = ?1",
                (attempt_id, at),
            )?;
        }
        Write::Progress(rows) => {
            for (message_id, attempt_id, content) in rows {
                tx.execute(
                    "UPDATE messages SET content = ?3
                     WHERE id = ?1 AND attempt_id = ?2 AND status IN ('assigned', 'running')",
                    (message_id, attempt_id, content),
                )?;
            }
        }
        Write::Requeued {
            job_id,
            message_id,
            attempt_id,
            outcome,
            error,
            refund,
            queued_at,
            deadline_at,
            not_before,
        } => {
            let changed = tx.execute(
                "UPDATE jobs SET status = 'queued', attempt_id = NULL, client_id = NULL,
                   attempts = attempts - ?3, queued_at = ?4, deadline_at = ?5, not_before = ?6
                 WHERE id = ?1 AND attempt_id = ?2 AND status IN ('assigned', 'running')",
                (job_id, attempt_id, refund, queued_at, deadline_at, not_before),
            )?;
            if changed == 0 {
                return Ok(false);
            }
            tx.execute(
                "UPDATE messages SET status = 'queued', content = '', error = NULL, attempt_id = NULL,
                   attempts = attempts - ?2, client_id = NULL, client_label = NULL, model = NULL,
                   meta_json = NULL
                 WHERE id = ?1",
                (message_id, refund),
            )?;
            tx.execute(
                "UPDATE job_attempts SET ended_at = ?2, outcome = ?3, error = ?4 WHERE id = ?1 AND ended_at IS NULL",
                (attempt_id, queued_at, outcome.as_str(), error),
            )?;
        }
        Write::Finished {
            job_id,
            message_id,
            attempt_id,
            conversation_id,
            status,
            outcome,
            content,
            error,
            meta,
            refund,
        } => {
            let now = now_ts();
            let changed = tx.execute(
                &format!(
                    "UPDATE jobs SET status = ?2, error = ?3, finished_at = ?4, attempts = attempts - ?6
                     WHERE id = ?1 AND status IN ({active}) AND (?5 IS NULL OR attempt_id = ?5)"
                ),
                (job_id, status, error, &now, attempt_id, refund),
            )?;
            if changed == 0 {
                return Ok(false);
            }
            tx.execute(
                "UPDATE messages SET status = ?2, content = ?3, error = ?4, completed_at = ?5,
                   meta_json = COALESCE(?6, meta_json), attempts = attempts - ?7
                 WHERE id = ?1",
                (message_id, status, content, error, &now, meta, refund),
            )?;
            if let Some(attempt_id) = attempt_id {
                tx.execute(
                    "UPDATE job_attempts SET ended_at = ?2, outcome = ?3, error = ?4
                     WHERE id = ?1 AND ended_at IS NULL",
                    (attempt_id, &now, outcome.as_str(), error),
                )?;
            }
            tx.execute(
                "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
                (&now, conversation_id),
            )?;
        }
    }
    Ok(true)
}

/// Adds a finished reply's usage to its `meta_json`:
/// `"usage": { prompt_tokens, completion_tokens, tokens_per_second, … }`,
/// with the runtime variant and GPU layers that produced it. The first
/// piece of usage metering, and real CPU-versus-GPU numbers.
fn record_usage(hub: &mut Hub, usage: &protocol::Usage) {
    let meta = hub.meta.get_or_insert_with(|| serde_json::json!({}));
    if let (Some(map), Ok(value)) = (meta.as_object_mut(), serde_json::to_value(usage)) {
        map.insert("usage".into(), value);
    }
}

fn kind(event: &ChatEvent) -> &'static str {
    match event {
        ChatEvent::Progress(_) => "progress",
        ChatEvent::Chunk(_) => "chunk",
        ChatEvent::Done(_) => "done",
        ChatEvent::Error { .. } => "error",
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
    use protocol::{Inventory, LocalModel, ModelState};

    pub fn local_model(id: &str, backend: Backend, context_length: Option<u32>) -> LocalModel {
        LocalModel {
            id: id.into(),
            display_name: id.into(),
            backend,
            state: ModelState::Loaded,
            size_bytes: 0,
            context_length,
            meta: Default::default(),
            error: None,
            settings: Default::default(),
            run: None,
        }
    }

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

    fn inference_settings() -> InferenceSettings {
        InferenceSettings {
            max_tokens: 100,
            echo_model_enabled: true,
            ..InferenceSettings::default()
        }
    }

    fn job_settings() -> JobSettings {
        JobSettings {
            queue_timeout: Duration::from_secs(60),
            max_attempts: 2,
            max_refusals: 3,
            refused_backoff: Duration::from_millis(100),
            queue_max: 10,
        }
    }

    struct Fixture {
        db: Db,
        live: Arc<Live>,
        runner: Arc<ReplyRunner>,
        owner: String,
        conn: ConnHandle,
    }

    fn inventory() -> Inventory {
        Inventory {
            models: vec![
                local_model("echo", Backend::Echo, None),
                // 600 tokens, 100 of them kept for the reply.
                local_model("tiny", Backend::LlamaCpp, Some(600)),
            ],
            ..Inventory::default()
        }
    }

    /// Connects (or reconnects) the fixture's client.
    fn connect(live: &Live) -> ConnHandle {
        let conn = live.register("c1");
        live.set_inventory("c1", conn.id, inventory());
        conn
    }

    fn fixture_with(chat: ChatSettings, jobs: JobSettings) -> Fixture {
        let db = memory_db();
        let live = Arc::new(Live::new(MetricsLimits {
            history_samples: 10,
            min_spacing: Duration::ZERO,
            max_gpus: 4,
            max_string_len: 64,
        }));
        let owner = add_user(&db, "alice", "user");
        add_client(&db, "c1", &owner);
        let conn = connect(&live);
        let runner = ReplyRunner::new(db.clone(), live.clone(), chat, inference_settings(), jobs);
        Fixture {
            db,
            live,
            runner,
            owner,
            conn,
        }
    }

    fn fixture() -> Fixture {
        fixture_with(chat_settings(), job_settings())
    }

    fn target() -> Target {
        Target::Client {
            client_id: "c1".into(),
            model: "echo".into(),
        }
    }

    /// A turn in `conversation` whose reply is queued for `target`; returns
    /// the reply id.
    fn send_to(f: &Fixture, conversation: &str, content: &str, parent: Option<&str>, target: &Target) -> String {
        let turn =
            f.db.blocking(|c| {
                match create_turn(
                    c,
                    NewTurn {
                        owner: &f.owner,
                        conversation_id: conversation,
                        content: Some(content),
                        parent_id: parent,
                        max_active_per_user: 10,
                        target,
                        max_attempts: f.runner.jobs.max_attempts,
                        queue_timeout: f.runner.jobs.queue_timeout,
                    },
                )? {
                    TurnOutcome::Created(turn) => Ok(turn),
                    _ => panic!("not created"),
                }
            })
            .unwrap();
        let id = turn.reply.id.clone();
        f.runner.submit(turn.job);
        id
    }

    /// A new conversation with one reply submitted to the fixture's client.
    fn queued_reply(f: &Fixture, conversation: &str) -> String {
        send_to(f, conversation, "hello", None, &target())
    }

    /// The attempt id of the request the client was sent.
    async fn sent_attempt(conn: &mut ConnHandle) -> String {
        loop {
            match tokio::time::timeout(Duration::from_secs(2), conn.outbound.recv()).await {
                Ok(Some(ServerMessage::ChatRequest { request_id, .. })) => return request_id,
                Ok(Some(ServerMessage::ChatCancel { .. })) => continue,
                other => panic!("expected a chat_request, got {other:?}"),
            }
        }
    }

    async fn db_state(f: &Fixture, id: &str) -> (MessageStatus, String) {
        let id = id.to_string();
        f.db.call(move |c| Ok(conversations::message_state(c, &id)?.unwrap()))
            .await
            .unwrap()
    }

    /// Waits until the writer has brought the job to `status`, then checks
    /// the message agrees. Returns the attempts' outcomes.
    async fn settled(f: &Fixture, id: &str, status: MessageStatus) -> Vec<String> {
        for _ in 0..200 {
            let mid = id.to_string();
            let (job, message, outcomes) = f.db.call(move |c| store::job_state(c, &mid)).await.unwrap();
            assert_eq!(job, message, "the job and its message disagree");
            if job == status {
                return outcomes;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the job never became {}", status.as_str());
    }

    async fn wait_for_end(rx: &mut broadcast::Receiver<ReplyEvent>) -> (MessageStatus, Option<String>) {
        loop {
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Ok(ReplyEvent::End { status, error, .. })) => return (status, error),
                Ok(Ok(_)) => continue,
                other => panic!("expected End, got {other:?}"),
            }
        }
    }

    /// The next `Requeued` event's (attempt, reason, previous client).
    async fn wait_for_requeue(rx: &mut broadcast::Receiver<ReplyEvent>) -> (u32, AttemptOutcome, Option<String>) {
        loop {
            match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
                Ok(Ok(ReplyEvent::Requeued {
                    attempt,
                    reason,
                    previous_client_label,
                    ..
                })) => return (attempt, reason, previous_client_label),
                Ok(Ok(_)) => continue,
                other => panic!("expected Requeued, got {other:?}"),
            }
        }
    }

    async fn attempts_of(f: &Fixture, id: &str) -> i64 {
        let id = id.to_string();
        f.db.call(move |c| Ok(c.query_row("SELECT attempts FROM messages WHERE id = ?1", [&id], |r| r.get(0))?))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_reply_streams_flushes_and_completes() {
        let mut f = fixture();
        assert!(f.runner.active_jobs().is_empty());
        let id = queued_reply(&f, "conv");
        // The hub exists from the moment it's submitted.
        assert!(f.runner.attach(&id).is_some());
        let attempt = sent_attempt(&mut f.conn).await;
        let (snap, mut rx) = f.runner.attach(&id).unwrap();
        assert_eq!(snap.status, MessageStatus::Assigned);
        assert_eq!((snap.attempt, snap.max_attempts, &snap.queue), (1, 2, &None));
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
        f.runner.on_client_event(f.conn.id, &attempt, ChatEvent::Done(None));
        assert!(f.runner.active_jobs().is_empty(), "idle as soon as it ends");
        assert_eq!(wait_for_end(&mut rx).await, (MessageStatus::Completed, None));
        // Announced only after the commit.
        assert_eq!(db_state(&f, &id).await, (MessageStatus::Completed, "hello".into()));
        assert_eq!(settled(&f, &id, MessageStatus::Completed).await, vec!["completed"]);
        assert!(f.runner.attach(&id).is_none(), "the hub is gone");
        assert_eq!(f.runner.hub_count(), 0);
    }

    #[tokio::test]
    async fn usage_from_chat_done_is_stored_in_meta_json() {
        let mut f = fixture();
        let id = queued_reply(&f, "conv");
        let attempt = sent_attempt(&mut f.conn).await;
        let (_, mut rx) = f.runner.attach(&id).unwrap();
        f.runner
            .on_client_event(f.conn.id, &attempt, ChatEvent::Chunk("hi".into()));
        let usage = protocol::Usage {
            prompt_tokens: 12,
            completion_tokens: 1,
            tokens_per_second: Some(42.5),
            prompt_tokens_per_second: None,
            run: Some(protocol::RunInfo {
                variant: "cuda-13".into(),
                gpu_layers: Some(protocol::GpuOffload {
                    offloaded: 29,
                    total: 29,
                }),
                device: Some("NVIDIA GeForce RTX 3070".into()),
            }),
        };
        f.runner
            .on_client_event(f.conn.id, &attempt, ChatEvent::Done(Some(usage)));
        assert_eq!(wait_for_end(&mut rx).await, (MessageStatus::Completed, None));
        let rid = id.clone();
        let meta: Option<String> =
            f.db.call(move |c| Ok(c.query_row("SELECT meta_json FROM messages WHERE id = ?1", [&rid], |r| r.get(0))?))
                .await
                .unwrap();
        let meta: serde_json::Value = serde_json::from_str(&meta.unwrap()).unwrap();
        assert_eq!(meta["usage"]["tokens_per_second"], 42.5);
        assert_eq!(meta["usage"]["completion_tokens"], 1);
        assert_eq!(meta["usage"]["run"]["variant"], "cuda-13");
        assert_eq!(meta["usage"]["run"]["gpu_layers"]["offloaded"], 29);
    }

    #[tokio::test]
    async fn events_from_an_old_attempt_or_another_connection_are_ignored() {
        let mut f = fixture();
        let id = queued_reply(&f, "conv");
        let attempt = sent_attempt(&mut f.conn).await;
        let (_, mut rx) = f.runner.attach(&id).unwrap();

        f.runner
            .on_client_event(f.conn.id + 1, &attempt, ChatEvent::Chunk("injected".into()));
        f.runner
            .on_client_event(f.conn.id, "some-old-attempt", ChatEvent::Chunk("stale".into()));
        f.runner.on_client_event(f.conn.id + 1, &attempt, ChatEvent::Done(None));
        assert!(rx.try_recv().is_err(), "nothing reached the hub");

        // The connection drops: the reply goes back to the queue, without
        // the text it had, and waits for its (pinned) client.
        f.runner
            .on_client_event(f.conn.id, &attempt, ChatEvent::Chunk("part".into()));
        f.live.unregister("c1", f.conn.id);
        f.runner.on_connection_closed(f.conn.id);
        assert_eq!(
            wait_for_requeue(&mut rx).await,
            (2, AttemptOutcome::Lost, Some("host".into()))
        );
        assert_eq!(settled(&f, &id, MessageStatus::Queued).await, vec!["lost"]);
        assert_eq!(db_state(&f, &id).await, (MessageStatus::Queued, String::new()));
        let (snap, _) = f.runner.attach(&id).unwrap();
        assert_eq!((snap.status, snap.content.as_str()), (MessageStatus::Queued, ""));
        assert_eq!(snap.client_label, None);
        assert!(f.runner.active_jobs().is_empty(), "its slot is free");

        // It reconnects: a fresh attempt. The old one's late chunks, and
        // even its chat_done, are dropped.
        let old_conn = f.conn.id;
        f.conn = connect(&f.live);
        let second = sent_attempt(&mut f.conn).await;
        assert_ne!(second, attempt);
        for conn_id in [old_conn, f.conn.id] {
            f.runner
                .on_client_event(conn_id, &attempt, ChatEvent::Chunk("late".into()));
            f.runner.on_client_event(conn_id, &attempt, ChatEvent::Done(None));
        }
        f.runner
            .on_client_event(f.conn.id, &second, ChatEvent::Chunk("fresh".into()));
        f.runner.on_client_event(f.conn.id, &second, ChatEvent::Done(None));
        assert_eq!(wait_for_end(&mut rx).await.0, MessageStatus::Completed);
        assert_eq!(db_state(&f, &id).await, (MessageStatus::Completed, "fresh".into()));
        assert_eq!(
            settled(&f, &id, MessageStatus::Completed).await,
            vec!["lost", "completed"]
        );
        assert_eq!(attempts_of(&f, &id).await, 2);
    }

    #[tokio::test]
    async fn job_and_message_statuses_agree_through_every_transition() {
        let mut f = fixture();
        let id = queued_reply(&f, "conv");
        // queued -> assigned -> running
        let first = sent_attempt(&mut f.conn).await;
        settled(&f, &id, MessageStatus::Assigned).await;
        f.runner
            .on_client_event(f.conn.id, &first, ChatEvent::Chunk("a".into()));
        settled(&f, &id, MessageStatus::Running).await;
        // running -> queued (the attempt is lost) -> assigned
        f.live.unregister("c1", f.conn.id);
        f.runner.on_connection_closed(f.conn.id);
        settled(&f, &id, MessageStatus::Queued).await;
        f.conn = connect(&f.live);
        let second = sent_attempt(&mut f.conn).await;
        settled(&f, &id, MessageStatus::Assigned).await;
        // assigned -> orphaned: that was the last of its two attempts.
        f.live.unregister("c1", f.conn.id);
        f.runner.on_connection_closed(f.conn.id);
        assert_eq!(settled(&f, &id, MessageStatus::Orphaned).await, vec!["lost", "lost"]);
        let (_, text) = db_state(&f, &id).await;
        assert_eq!(text, "", "the second attempt had produced nothing");
        f.runner.on_client_event(f.conn.id, &second, ChatEvent::Done(None));

        // orphaned -> queued (manual Retry, a fresh budget) -> assigned -> failed
        f.conn = connect(&f.live);
        f.runner.retry(&f.owner, &id, &target()).await.unwrap();
        let third = sent_attempt(&mut f.conn).await;
        settled(&f, &id, MessageStatus::Assigned).await;
        f.runner.on_client_event(
            f.conn.id,
            &third,
            ChatEvent::Error {
                error: "out of memory".into(),
                code: None,
            },
        );
        assert_eq!(
            settled(&f, &id, MessageStatus::Failed).await,
            vec!["lost", "lost", "failed"]
        );
        assert_eq!(attempts_of(&f, &id).await, 3);

        // failed -> queued -> assigned -> running -> cancelled
        f.runner.retry(&f.owner, &id, &target()).await.unwrap();
        let fourth = sent_attempt(&mut f.conn).await;
        f.runner
            .on_client_event(f.conn.id, &fourth, ChatEvent::Chunk("b".into()));
        settled(&f, &id, MessageStatus::Running).await;
        f.runner.cancel(&id).await;
        assert_eq!(
            settled(&f, &id, MessageStatus::Cancelled).await,
            vec!["lost", "lost", "failed", "cancelled"]
        );

        // And the plain path, to completed; then cancelled while queued.
        let other = queued_reply(&f, "conv2");
        let attempt = sent_attempt(&mut f.conn).await;
        let waiting = queued_reply(&f, "conv3");
        settled(&f, &waiting, MessageStatus::Queued).await;
        f.runner.cancel(&waiting).await;
        assert!(settled(&f, &waiting, MessageStatus::Cancelled).await.is_empty());
        f.runner.on_client_event(f.conn.id, &attempt, ChatEvent::Done(None));
        assert_eq!(settled(&f, &other, MessageStatus::Completed).await, vec!["completed"]);

        // Nothing anywhere disagrees, and no attempt is left open.
        let (disagreeing, open): (i64, i64) =
            f.db.call(|c| {
                Ok((
                    c.query_row(
                        "SELECT COUNT(*) FROM jobs j JOIN messages m ON m.id = j.message_id
                         WHERE j.status <> m.status OR j.attempts <> m.attempts",
                        [],
                        |r| r.get(0),
                    )?,
                    c.query_row("SELECT COUNT(*) FROM job_attempts WHERE ended_at IS NULL", [], |r| {
                        r.get(0)
                    })?,
                ))
            })
            .await
            .unwrap();
        assert_eq!((disagreeing, open), (0, 0));
    }

    #[tokio::test]
    async fn stop_keeps_the_text_and_tells_the_client() {
        let mut f = fixture();
        let id = queued_reply(&f, "conv");
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
    async fn a_reply_waits_for_a_disconnected_client_until_the_deadline() {
        let f = fixture_with(
            chat_settings(),
            JobSettings {
                queue_timeout: Duration::from_millis(200),
                ..job_settings()
            },
        );
        f.live.unregister("c1", f.conn.id);
        let id = queued_reply(&f, "conv");
        let (snap, mut rx) = f.runner.attach(&id).unwrap();
        assert_eq!(snap.status, MessageStatus::Queued, "it waits rather than failing");
        let (status, error) = wait_for_end(&mut rx).await;
        assert_eq!(status, MessageStatus::Failed);
        assert_eq!(error.as_deref(), Some("host wasn't free (or connected) within 0 s."));
        assert!(
            settled(&f, &id, MessageStatus::Failed).await.is_empty(),
            "no attempt was made"
        );
    }

    #[tokio::test]
    async fn quiet_clients_time_out_and_the_reply_is_requeued() {
        let mut f = fixture_with(
            ChatSettings {
                idle_timeout: Duration::from_millis(100),
                ..chat_settings()
            },
            job_settings(),
        );
        let id = queued_reply(&f, "conv");
        let attempt = sent_attempt(&mut f.conn).await;
        let (_, mut rx) = f.runner.attach(&id).unwrap();
        assert_eq!(
            wait_for_requeue(&mut rx).await,
            (2, AttemptOutcome::TimedOut, Some("host".into()))
        );
        // The client is told to stop, then (it's pinned, and still
        // connected) gets the next attempt.
        assert!(matches!(
            f.conn.outbound.recv().await,
            Some(ServerMessage::ChatCancel { request_id }) if request_id == attempt
        ));
        let second = sent_attempt(&mut f.conn).await;
        assert_ne!(second, attempt);
        // Quiet again: that was its last attempt.
        let (status, error) = wait_for_end(&mut rx).await;
        assert_eq!(status, MessageStatus::Orphaned);
        assert_eq!(error.as_deref(), Some("The client sent nothing for 0 s (2 attempts)."));
        assert_eq!(
            settled(&f, &id, MessageStatus::Orphaned).await,
            vec!["timed_out", "timed_out"]
        );
    }

    #[tokio::test]
    async fn progress_keeps_a_slow_reply_alive_and_reaches_listeners() {
        let mut f = fixture_with(
            ChatSettings {
                idle_timeout: Duration::from_millis(300),
                ..chat_settings()
            },
            JobSettings {
                max_attempts: 1,
                ..job_settings()
            },
        );
        let runner = f.runner.clone();
        let id = queued_reply(&f, "conv");
        let attempt = sent_attempt(&mut f.conn).await;
        let (_, mut rx) = runner.attach(&id).unwrap();
        // Loading a model for 0.9 s, three times the idle timeout.
        for _ in 0..9 {
            runner.on_client_event(f.conn.id, &attempt, ChatEvent::Progress(Stage::LoadingModel));
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(runner.attach(&id).is_some(), "still running");
        assert_eq!(rx.recv().await.unwrap(), ReplyEvent::Progress(Stage::LoadingModel));
        assert_eq!(
            runner.attach(&id).unwrap().0.stage,
            Some(Stage::LoadingModel),
            "a stream attaching now hears it's loading"
        );
        runner.on_client_event(f.conn.id, &attempt, ChatEvent::Progress(Stage::Generating));
        runner.on_client_event(f.conn.id, &attempt, ChatEvent::Chunk("hi".into()));
        // Then silence: it times out as before, and with one attempt
        // allowed that ends it, keeping its text.
        let (status, error) = wait_for_end(&mut rx).await;
        assert_eq!(status, MessageStatus::Orphaned);
        assert_eq!(error.as_deref(), Some("The client sent nothing for 0 s (1 attempt)."));
        assert_eq!(db_state(&f, &id).await.1, "hi");
    }

    #[tokio::test]
    async fn history_is_trimmed_to_the_models_context_and_recorded() {
        let mut f = fixture();
        // Three long completed turns, then a new message: 2,000 chars each
        // is about 500 tokens, and "tiny" has room for 500 in all.
        let mut parent: Option<String> = None;
        for i in 0..3 {
            let reply = send_to(
                &f,
                "conv",
                &format!("{i}{}", "x".repeat(2_000)),
                parent.as_deref(),
                &target(),
            );
            let attempt = sent_attempt(&mut f.conn).await;
            f.runner
                .on_client_event(f.conn.id, &attempt, ChatEvent::Chunk("ok".into()));
            f.runner.on_client_event(f.conn.id, &attempt, ChatEvent::Done(None));
            settled(&f, &reply, MessageStatus::Completed).await;
            parent = Some(reply);
        }
        let tiny = Target::Client {
            client_id: "c1".into(),
            model: "tiny".into(),
        };
        let reply_id = send_to(&f, "conv", "the newest question", parent.as_deref(), &tiny);
        let messages = match tokio::time::timeout(Duration::from_secs(2), f.conn.outbound.recv()).await {
            Ok(Some(ServerMessage::ChatRequest { messages, model, .. })) => {
                assert_eq!(model, "tiny");
                messages
            }
            other => panic!("expected a chat_request, got {other:?}"),
        };
        // Of 7 turns (3 x user+reply, then the question), only the last
        // reply and the question fit; a reply can't open the history.
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert_eq!(messages[0].content, "the newest question");
        settled(&f, &reply_id, MessageStatus::Assigned).await;
        let rid = reply_id.clone();
        let meta: Option<String> =
            f.db.call(move |c| Ok(c.query_row("SELECT meta_json FROM messages WHERE id = ?1", [&rid], |r| r.get(0))?))
                .await
                .unwrap();
        assert_eq!(meta.as_deref(), Some(r#"{"history_turns_dropped":6}"#));
        f.runner.cancel(&reply_id).await;

        // Echo has no context limit: nothing is trimmed.
        let other = queued_reply(&f, "conv2");
        let _ = sent_attempt(&mut f.conn).await;
        settled(&f, &other, MessageStatus::Assigned).await;
        let meta: Option<String> = f
            .db
            .call(move |c| Ok(c.query_row("SELECT meta_json FROM messages WHERE id = ?1", [&other], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(meta, None);
    }
}
