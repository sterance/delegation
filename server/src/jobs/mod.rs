//! The job queue: generic jobs, placed on clients by one scheduler.
//!
//! A job is a unit of work for a client, persisted in the `jobs` table so a
//! restart doesn't lose it. Chat replies are the first (and so far only)
//! kind; the reply runner (`replies.rs`) is their executor. A job is aimed
//! at a [`Target`]: one client (a queue entry pinned to it) or the owner's
//! pool (the next free client that offers the model). Both go through the
//! same queue, and the scheduler (`scheduler.rs`) makes every assignment.
//!
//! `server/JOBS.md` describes the states, the placement rules, the attempt
//! outcomes and recovery.

pub mod api;
pub mod pick;
pub mod scheduler;
pub mod store;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ValueRef};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A job's state. Chat replies came first, so the type lives with messages;
/// a reply's `messages.status` always equals its job's status.
pub use crate::conversations::MessageStatus as JobStatus;

/// What a job does. The database's CHECK list is a literal in the migration
/// that created `jobs`; a test compares it with `ALL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    ChatReply,
}

impl JobKind {
    pub const ALL: [JobKind; 1] = [JobKind::ChatReply];

    pub fn as_str(self) -> &'static str {
        match self {
            JobKind::ChatReply => "chat_reply",
        }
    }
}

impl FromSql for JobKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| FromSqlError::Other(format!("unknown job kind {s:?}").into()))
    }
}

/// `jobs.target_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    Client,
    Pool,
}

impl TargetKind {
    pub const ALL: [TargetKind; 2] = [TargetKind::Client, TargetKind::Pool];

    pub fn as_str(self) -> &'static str {
        match self {
            TargetKind::Client => "client",
            TargetKind::Pool => "pool",
        }
    }
}

/// How one attempt at a job ended (`job_attempts.outcome`). The last four
/// are the ones that send the job back to the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    Completed,
    Failed,
    Cancelled,
    /// The client's connection closed.
    Lost,
    /// The client sent nothing for `CHAT_IDLE_TIMEOUT_MS`.
    TimedOut,
    /// The client was busy, or its outbound queue was full. Doesn't use up
    /// an attempt.
    Refused,
    /// The server restarted mid-attempt.
    Interrupted,
}

impl AttemptOutcome {
    pub const ALL: [AttemptOutcome; 7] = [
        AttemptOutcome::Completed,
        AttemptOutcome::Failed,
        AttemptOutcome::Cancelled,
        AttemptOutcome::Lost,
        AttemptOutcome::TimedOut,
        AttemptOutcome::Refused,
        AttemptOutcome::Interrupted,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AttemptOutcome::Completed => "completed",
            AttemptOutcome::Failed => "failed",
            AttemptOutcome::Cancelled => "cancelled",
            AttemptOutcome::Lost => "lost",
            AttemptOutcome::TimedOut => "timed_out",
            AttemptOutcome::Refused => "refused",
            AttemptOutcome::Interrupted => "interrupted",
        }
    }
}

/// Who should run a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Target {
    /// This client (directed assignment): a queue entry pinned to it.
    Client { client_id: String, model: String },
    /// Any client the user may pool that offers `model` (a catalog id, or
    /// `echo` while it's enabled). Catalog ids are the same on every
    /// client, which is what makes this meaningful.
    Pool { model: String },
}

impl Target {
    pub fn model(&self) -> &str {
        match self {
            Target::Client { model, .. } | Target::Pool { model } => model,
        }
    }

    pub fn kind(&self) -> TargetKind {
        match self {
            Target::Client { .. } => TargetKind::Client,
            Target::Pool { .. } => TargetKind::Pool,
        }
    }

    pub fn pinned_client(&self) -> Option<&str> {
        match self {
            Target::Client { client_id, .. } => Some(client_id),
            Target::Pool { .. } => None,
        }
    }
}

/// Every allowed status change, for every job kind. Everything else is a
/// bug and is refused.
pub fn can_move(from: JobStatus, to: JobStatus) -> bool {
    use JobStatus::*;
    matches!(
        (from, to),
        (Queued, Assigned | Failed | Cancelled | Orphaned)
            // To Queued is the re-queue: the attempt was lost, timed out or
            // refused, and the same job runs again.
            | (Assigned, Running | Completed | Failed | Cancelled | Orphaned | Queued)
            | (Running, Completed | Failed | Cancelled | Orphaned | Queued)
            // Manual Retry: a new set of attempts, in place.
            | (Failed | Orphaned, Queued)
    )
}

/// `JOB_*` settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JobSettings {
    /// How long a job may wait in the queue each time it's queued.
    pub queue_timeout: Duration,
    /// Attempts per submit or Retry, refusals not counted.
    pub max_attempts: u32,
    /// Refusals in a row before the job fails.
    pub max_refusals: u32,
    /// Wait after a refusal before that job can be placed again.
    pub refused_backoff: Duration,
    /// Queued jobs across all users before submits get 429.
    pub queue_max: usize,
}

/// What a queued job is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitingFor {
    /// Clients that could run it are connected, but busy.
    AFreeClient,
    /// No client that could run it is connected (or offers the model).
    AClient,
}

/// Where a queued job stands, for the reply's snapshot and `status` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueueInfo {
    /// 1-based, among queued jobs this one could share a client with.
    pub position: usize,
    pub waiting_for: WaitingFor,
    /// The pinned client's name, for "Waiting for {label} to reconnect…".
    pub client_label: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_allowed_status_change_and_only_those() {
        use JobStatus::*;
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
            (Assigned, Queued),
            (Running, Completed),
            (Running, Failed),
            (Running, Cancelled),
            (Running, Orphaned),
            (Running, Queued),
            (Failed, Queued),
            (Orphaned, Queued),
        ];
        for from in JobStatus::ALL {
            for to in JobStatus::ALL {
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
        assert!(!can_move(Cancelled, Queued), "only failed and orphaned jobs retry");
        assert!(!can_move(Completed, Queued), "a finished job is never re-queued");
    }

    #[test]
    fn targets_parse_from_the_api_shape() {
        let pool: Target = serde_json::from_str(r#"{"kind":"pool","model":"m"}"#).unwrap();
        assert_eq!(pool, Target::Pool { model: "m".into() });
        assert_eq!(
            (pool.kind(), pool.model(), pool.pinned_client()),
            (TargetKind::Pool, "m", None)
        );
        let client: Target = serde_json::from_str(r#"{"kind":"client","client_id":"c","model":"m"}"#).unwrap();
        assert_eq!(client.pinned_client(), Some("c"));
        assert!(serde_json::from_str::<Target>(r#"{"kind":"anyone","model":"m"}"#).is_err());
        assert_eq!(
            serde_json::to_value(AttemptOutcome::TimedOut).unwrap(),
            serde_json::json!(AttemptOutcome::TimedOut.as_str())
        );
        assert_eq!(
            serde_json::to_value(JobKind::ChatReply).unwrap(),
            serde_json::json!(JobKind::ChatReply.as_str())
        );
    }
}
