//! The scheduler: the queue of jobs waiting for a client, and the one task
//! that places them.
//!
//! Every assignment, directed and pooled alike, is made by [`Scheduler::run`],
//! so two jobs can never take the same last slot. It doesn't poll. It runs
//! a pass when it's woken: a job was submitted or re-queued, an attempt
//! ended, a client connected, disconnected or sent a new inventory
//! (`Live::changed`), or a deadline or backoff came due.
//!
//! The database is the source of truth for jobs; this is an in-memory index
//! of the queued ones (and of which client each running one is on), rebuilt
//! at startup from `store::recover`. The executor (the reply runner, for
//! chat replies) tells it about every change with [`Scheduler::enqueue`] and
//! [`Scheduler::release`], and is told what to run with
//! `ReplyRunner::assign`. Locks are always taken executor first, then
//! scheduler: nothing here calls the executor while holding the state lock.

use super::pick::{pick, Candidate};
use super::{JobKind, JobSettings, QueueInfo, Target, WaitingFor};
use crate::access::{self, PlacementClient};
use crate::db::Db;
use crate::live::{usable, Live};
use crate::log;
use crate::replies::ReplyRunner;
use protocol::{Backend, ModelState};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::time::Instant;

/// A job entering the queue.
pub struct Enqueue {
    pub job_id: String,
    pub kind: JobKind,
    /// What the executor knows the job by: for a chat reply, its message id.
    pub subject: String,
    pub owner_user_id: String,
    pub target: Target,
    /// The client that just lost it, to try another one first.
    pub avoid_client: Option<String>,
    /// Don't place it before this (backoff after a refusal).
    pub not_before: Option<Instant>,
}

/// The scheduler's decision: run this job on this client.
#[derive(Debug, Clone)]
pub struct Placement {
    pub job_id: String,
    pub kind: JobKind,
    pub subject: String,
    /// Which time in the queue this is for, as `enqueue` returned it. The
    /// executor ignores a placement whose ticket isn't the job's current one.
    pub ticket: u64,
    pub target: Target,
    pub client_id: String,
    pub client_label: String,
}

struct Entry {
    job_id: String,
    kind: JobKind,
    subject: String,
    owner_user_id: String,
    target: Target,
    avoid_client: Option<String>,
    not_before: Option<Instant>,
    /// A queued job that isn't placed by then fails.
    deadline: Instant,
    /// Rises with every enqueue, so ascending is oldest in the queue first.
    ticket: u64,
    /// What listeners were last told, to tell them again only on a change.
    info: Option<QueueInfo>,
}

struct Placed {
    entry: Entry,
    client_id: String,
}

#[derive(Default)]
struct State {
    /// By job id.
    queued: HashMap<String, Entry>,
    /// Jobs with an attempt on a client (from the moment one is chosen), by
    /// job id.
    placed: HashMap<String, Placed>,
    /// When each client's last attempt ended, to spread the load.
    last_finished: HashMap<String, Instant>,
    next_ticket: u64,
}

pub struct Scheduler {
    db: Db,
    live: Arc<Live>,
    settings: JobSettings,
    /// `INFERENCE_MAX_CONCURRENT`, which every client is sent in `welcome`.
    max_concurrent: usize,
    echo_enabled: bool,
    state: Mutex<State>,
    wake: Arc<Notify>,
}

/// A job the pass gave up on.
struct Failure {
    subject: String,
    ticket: u64,
    error: String,
}

impl Scheduler {
    pub fn new(db: Db, live: Arc<Live>, settings: JobSettings, max_concurrent: usize, echo_enabled: bool) -> Arc<Self> {
        Arc::new(Scheduler {
            db,
            // One signal for everything: clients coming and going, and the
            // queue changing.
            wake: live.changed.clone(),
            live,
            settings,
            max_concurrent: max_concurrent.max(1),
            echo_enabled,
            state: Mutex::new(State::default()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Puts a job at the back of the queue with a fresh deadline: a new job,
    /// a manual Retry, or one whose attempt was lost (its slot on the client
    /// is freed). Returns its ticket.
    pub fn enqueue(&self, job: Enqueue) -> u64 {
        let mut state = self.lock();
        forget(&mut state, &job.job_id);
        state.next_ticket += 1;
        let ticket = state.next_ticket;
        state.queued.insert(
            job.job_id.clone(),
            Entry {
                job_id: job.job_id,
                kind: job.kind,
                subject: job.subject,
                owner_user_id: job.owner_user_id,
                target: job.target,
                avoid_client: job.avoid_client,
                not_before: job.not_before,
                deadline: Instant::now() + self.settings.queue_timeout,
                ticket,
                info: None,
            },
        );
        drop(state);
        self.wake.notify_one();
        ticket
    }

    /// The job has ended (or was cancelled while it waited): forget it, and
    /// free its slot on the client it was on.
    pub fn release(&self, job_id: &str) {
        let mut state = self.lock();
        forget(&mut state, job_id);
        drop(state);
        self.wake.notify_one();
    }

    /// A placement that couldn't be used because the job has moved on
    /// (stopped, or queued again since): frees the slot it reserved, unless
    /// the job's entry is a newer one.
    pub fn release_ticket(&self, job_id: &str, ticket: u64) {
        let mut state = self.lock();
        if state.placed.get(job_id).is_some_and(|p| p.entry.ticket == ticket) {
            forget(&mut state, job_id);
            drop(state);
            self.wake.notify_one();
        }
    }

    /// A placement that couldn't be used because the client went away (or
    /// lost the model) between being chosen and being sent the job: the job
    /// goes back where it was in the queue, with the same deadline.
    pub fn put_back(&self, job_id: &str, ticket: u64) {
        let mut state = self.lock();
        if state.placed.get(job_id).is_some_and(|p| p.entry.ticket == ticket) {
            if let Some(placed) = state.placed.remove(job_id) {
                state.queued.insert(job_id.to_string(), placed.entry);
            }
            drop(state);
            self.wake.notify_one();
        }
    }

    /// Where a queued job stands, as of the last pass. None if it isn't
    /// queued, or no pass has looked at it yet.
    pub fn queue_info(&self, job_id: &str) -> Option<QueueInfo> {
        self.lock().queued.get(job_id)?.info.clone()
    }

    /// Jobs waiting in the queue, for `JOB_QUEUE_MAX`.
    pub fn queued_len(&self) -> usize {
        self.lock().queued.len()
    }

    /// Attempts each client is working on right now (from the moment it's
    /// chosen until the attempt ends), whoever owns them, by client id.
    /// Clients with none are left out.
    pub fn active_jobs(&self) -> HashMap<String, usize> {
        let state = self.lock();
        let mut jobs = HashMap::new();
        for placed in state.placed.values() {
            *jobs.entry(placed.client_id.clone()).or_insert(0) += 1;
        }
        jobs
    }

    /// Queued jobs pinned to each client, by client id.
    pub fn queued_for(&self) -> HashMap<String, usize> {
        let state = self.lock();
        let mut jobs = HashMap::new();
        for entry in state.queued.values() {
            if let Some(client_id) = entry.target.pinned_client() {
                *jobs.entry(client_id.to_string()).or_insert(0) += 1;
            }
        }
        jobs
    }

    /// The scheduler task. `runner` executes chat-reply jobs.
    pub async fn run(self: Arc<Self>, runner: Arc<ReplyRunner>) {
        loop {
            self.pass(&runner).await;
            // A wake during the pass left a permit, so nothing is missed.
            match self.next_timer() {
                Some(at) => {
                    tokio::select! {
                        _ = self.wake.notified() => {}
                        _ = tokio::time::sleep_until(at) => {}
                    }
                }
                None => self.wake.notified().await,
            }
        }
    }

    /// The next deadline or end of a backoff.
    fn next_timer(&self) -> Option<Instant> {
        let now = Instant::now();
        let state = self.lock();
        state
            .queued
            .values()
            .flat_map(|e| [Some(e.deadline), e.not_before.filter(|at| *at > now)])
            .flatten()
            .min()
    }

    /// One pass over the queue, oldest first. A job that can't be placed is
    /// skipped, so it doesn't hold up the jobs behind it.
    async fn pass(&self, runner: &ReplyRunner) {
        let owners: Vec<String> = {
            let state = self.lock();
            let mut owners: Vec<String> = state.queued.values().map(|e| e.owner_user_id.clone()).collect();
            owners.sort();
            owners.dedup();
            owners
        };
        if owners.is_empty() {
            return;
        }
        // Access is asked again at every placement, not remembered from
        // submit time.
        let loaded = self
            .db
            .call(move |c| {
                let mut by_owner = HashMap::new();
                for owner in owners {
                    if let Some(clients) = access::placement_clients(c, &owner)? {
                        by_owner.insert(owner, clients);
                    }
                }
                Ok(by_owner)
            })
            .await;
        let by_owner: HashMap<String, Vec<PlacementClient>> = match loaded {
            Ok(by_owner) => by_owner,
            Err(e) => {
                log(format!("[jobs] couldn't read who may use which client: {e:#}"));
                return;
            }
        };

        let (placements, failures, changed) = self.place(&by_owner);
        for failure in failures {
            log(format!("[jobs] {} left the queue: {}", failure.subject, failure.error));
            runner.fail_queued(&failure.subject, failure.ticket, failure.error);
        }
        for placement in placements {
            match placement.kind {
                JobKind::ChatReply => runner.assign(placement).await,
            }
        }
        if !changed.is_empty() {
            runner.queue_changed(&changed);
        }
    }

    /// The synchronous part of a pass: decides, under the lock, which queued
    /// jobs go to which client, which have run out of time, and which should
    /// tell their listeners that their place in the queue changed.
    fn place(&self, by_owner: &HashMap<String, Vec<PlacementClient>>) -> (Vec<Placement>, Vec<Failure>, Vec<String>) {
        let now = Instant::now();
        let secs = self.settings.queue_timeout.as_secs();
        let mut state = self.lock();
        let state = &mut *state;

        // What each client is doing: how many attempts, with which models.
        let mut load: HashMap<String, Vec<String>> = HashMap::new();
        for placed in state.placed.values() {
            load.entry(placed.client_id.clone())
                .or_default()
                .push(placed.entry.target.model().to_string());
        }

        let mut order: Vec<(u64, String)> = state.queued.values().map(|e| (e.ticket, e.job_id.clone())).collect();
        order.sort();
        let mut placements = Vec::new();
        let mut failures = Vec::new();
        let mut changed = Vec::new();
        // The clients each job still waiting (and ahead in the queue) may use.
        let mut waiting: Vec<Vec<&str>> = Vec::new();

        for (_, job_id) in order {
            let Some(entry) = state.queued.get(&job_id) else {
                continue;
            };
            // The owner is being deleted; their jobs are being cancelled.
            let Some(clients) = by_owner.get(&entry.owner_user_id) else {
                continue;
            };
            let model = entry.target.model().to_string();
            let permitted: Vec<&PlacementClient> = match &entry.target {
                Target::Client { client_id, .. } => clients
                    .iter()
                    .filter(|c| c.client_id == *client_id && c.can_use)
                    .collect(),
                Target::Pool { .. } => clients.iter().filter(|c| c.can_pool).collect(),
            };
            let pinned_label = entry
                .target
                .pinned_client()
                .and_then(|_| permitted.first().map(|c| c.label.clone()));
            if entry.target.pinned_client().is_some() && permitted.is_empty() {
                let entry = state.queued.remove(&job_id).expect("just read");
                failures.push(Failure {
                    subject: entry.subject,
                    ticket: entry.ticket,
                    error: "That client no longer exists.".into(),
                });
                continue;
            }

            let candidates: Vec<Candidate> = permitted
                .iter()
                .map(|client| {
                    let running = load.get(&client.client_id);
                    Candidate {
                        client_id: client.client_id.clone(),
                        permitted: true,
                        model_state: self.model_state(&client.client_id, &model),
                        active: running.map_or(0, Vec::len),
                        busy_with_another_model: running.is_some_and(|models| models.iter().any(|m| *m != model)),
                        last_finished: state.last_finished.get(&client.client_id).map(|at| at.into_std()),
                    }
                })
                .collect();
            let backing_off = entry.not_before.is_some_and(|at| at > now);
            let picked = if backing_off {
                None
            } else {
                pick(entry.avoid_client.as_deref(), self.max_concurrent, &candidates).map(str::to_string)
            };

            if let Some(client_id) = picked {
                let entry = state.queued.remove(&job_id).expect("just read");
                let client_label = permitted
                    .iter()
                    .find(|c| c.client_id == client_id)
                    .map(|c| c.label.clone())
                    .unwrap_or_default();
                load.entry(client_id.clone()).or_default().push(model);
                placements.push(Placement {
                    job_id: entry.job_id.clone(),
                    kind: entry.kind,
                    subject: entry.subject.clone(),
                    ticket: entry.ticket,
                    target: entry.target.clone(),
                    client_id: client_id.clone(),
                    client_label,
                });
                state.placed.insert(job_id, Placed { entry, client_id });
                continue;
            }
            if now >= entry.deadline {
                let entry = state.queued.remove(&job_id).expect("just read");
                let error = match pinned_label {
                    Some(label) => format!("{label} wasn't free (or connected) within {secs} s."),
                    None => format!("No client offering {model} was free within {secs} s."),
                };
                failures.push(Failure {
                    subject: entry.subject,
                    ticket: entry.ticket,
                    error,
                });
                continue;
            }

            // Still waiting. Its position is an approximation: it counts the
            // jobs ahead that may use one of the same clients, whether or
            // not they want the same model or would end up on the same one.
            let mine: Vec<&str> = permitted.iter().map(|c| c.client_id.as_str()).collect();
            let ahead = waiting
                .iter()
                .filter(|other| other.iter().any(|id| mine.contains(id)))
                .count();
            let offered = candidates.iter().any(|c| c.model_state.is_some());
            let info = QueueInfo {
                position: ahead + 1,
                waiting_for: if offered {
                    WaitingFor::AFreeClient
                } else {
                    WaitingFor::AClient
                },
                client_label: pinned_label,
            };
            waiting.push(mine);
            let entry = state.queued.get_mut(&job_id).expect("just read");
            if entry.info.as_ref() != Some(&info) {
                entry.info = Some(info);
                changed.push(entry.subject.clone());
            }
        }
        (placements, failures, changed)
    }

    /// The model's state on a connected client, if the client could answer
    /// with it now. Echo counts only while it's enabled.
    fn model_state(&self, client_id: &str, model: &str) -> Option<ModelState> {
        let conn = self.live.sender(client_id)?;
        let local = conn.model(model)?;
        if !usable(local.state) || (local.backend == Backend::Echo && !self.echo_enabled) {
            return None;
        }
        Some(local.state)
    }
}

/// Removes a job from the index. If it had a client, that client's slot is
/// free from now.
fn forget(state: &mut State, job_id: &str) {
    state.queued.remove(job_id);
    if let Some(placed) = state.placed.remove(job_id) {
        state.last_finished.insert(placed.client_id, Instant::now());
    }
}
