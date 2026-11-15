//! The `jobs` and `job_attempts` tables: creating jobs, restart recovery,
//! and reading them for the API. The writes that follow a job through its
//! attempts are the reply runner's, in its writer task (`replies.rs`).

use super::{JobKind, JobStatus, Target, TargetKind};
use crate::conversations::active_sql;
use crate::db::{now_ts, parse_ts, ts};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use std::time::{Duration, SystemTime};

/// What an interrupted job and its reply say after a restart used up its
/// last attempt.
pub const INTERRUPTED: &str = "Interrupted by a server restart";

/// A new `queued` chat-reply job.
pub struct NewJob<'a> {
    pub id: &'a str,
    pub owner_user_id: &'a str,
    pub message_id: &'a str,
    pub target: &'a Target,
    /// Attempts already made at the reply (0 for a new one).
    pub attempts: u32,
    pub max_attempts: u32,
    pub now: &'a str,
    pub deadline_at: &'a str,
}

/// Inserts the job. Called inside the transaction that creates (or
/// re-queues) its reply, so a reply never exists without its job.
pub fn insert_job(conn: &Connection, job: &NewJob<'_>) -> Result<()> {
    conn.execute(
        "INSERT INTO jobs (id, kind, owner_user_id, message_id, status, target_kind, pinned_client_id, model,
           attempts, max_attempts, created_at, queued_at, deadline_at)
         VALUES (?1, ?2, ?3, ?4, 'queued', ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?11)",
        (
            job.id,
            JobKind::ChatReply.as_str(),
            job.owner_user_id,
            job.message_id,
            job.target.kind().as_str(),
            job.target.pinned_client(),
            job.target.model(),
            job.attempts,
            job.max_attempts,
            job.now,
            job.deadline_at,
        ),
    )?;
    Ok(())
}

/// Manual Retry of a job that exists: back to `queued` with `target` and a
/// fresh budget of `budget` attempts on top of those already made. Returns
/// (attempts so far, the new max_attempts).
pub fn requeue_for_retry(
    conn: &Connection,
    job_id: &str,
    target: &Target,
    budget: u32,
    now: &str,
    deadline_at: &str,
) -> Result<(u32, u32)> {
    conn.execute(
        "UPDATE jobs SET status = 'queued', target_kind = ?2, pinned_client_id = ?3, model = ?4,
           max_attempts = attempts + ?5, attempt_id = NULL, client_id = NULL, error = NULL,
           queued_at = ?6, deadline_at = ?7, not_before = NULL, finished_at = NULL
         WHERE id = ?1",
        (
            job_id,
            target.kind().as_str(),
            target.pinned_client(),
            target.model(),
            budget,
            now,
            deadline_at,
        ),
    )?;
    Ok(
        conn.query_row("SELECT attempts, max_attempts FROM jobs WHERE id = ?1", [job_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?,
    )
}

/// The job that produces a reply, if it has one (replies from before the
/// job queue don't).
pub fn job_of_message(conn: &Connection, message_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT id FROM jobs WHERE message_id = ?1", [message_id], |r| r.get(0))
        .optional()?)
}

/// What a reply's job was aimed at, for Retry's default.
pub enum OriginalTarget {
    /// The reply has no job (it's from before the job queue).
    NoJob,
    Target(Target),
    /// It was pinned to a client that has since been deleted.
    ClientDeleted,
}

pub fn original_target(conn: &Connection, message_id: &str) -> Result<OriginalTarget> {
    let row: Option<(String, Option<String>, String)> = conn
        .query_row(
            "SELECT target_kind, pinned_client_id, model FROM jobs WHERE message_id = ?1",
            [message_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(match row {
        None => OriginalTarget::NoJob,
        Some((kind, pinned, model)) => match target_from(&kind, pinned, model) {
            Some(target) => OriginalTarget::Target(target),
            None => OriginalTarget::ClientDeleted,
        },
    })
}

fn target_from(kind: &str, pinned_client_id: Option<String>, model: String) -> Option<Target> {
    if kind == TargetKind::Pool.as_str() {
        Some(Target::Pool { model })
    } else {
        Some(Target::Client {
            client_id: pinned_client_id?,
            model,
        })
    }
}

// ---------------- restart recovery ----------------

/// A queued chat-reply job, as the scheduler and the reply runner need it.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedReply {
    pub job_id: String,
    pub message_id: String,
    pub conversation_id: String,
    pub owner_user_id: String,
    pub target: Target,
    pub attempts: u32,
    pub max_attempts: u32,
}

#[derive(Debug, Default)]
pub struct Recovery {
    /// Every queued job, oldest first, to load into the scheduler.
    pub queued: Vec<QueuedReply>,
    /// Jobs that were mid-attempt and went back to the queue.
    pub requeued: usize,
    /// Jobs that were mid-attempt with no attempts left.
    pub orphaned: usize,
    /// Active replies without a job (created by an older build), orphaned.
    pub orphaned_without_job: usize,
    /// Active jobs whose reply an older build had already finished.
    pub settled: usize,
}

/// Server start, before anything is served: nothing is running any more, so
/// every job that was mid-attempt has its attempt closed as `interrupted`
/// and goes back to the queue (or ends `orphaned` if that was its last
/// attempt), and every queued job gets a fresh deadline, because clients
/// take a few seconds to reconnect.
///
/// A deploy that fails its health check rolls back to the previous build,
/// which knows nothing about jobs: it orphans active replies on its own
/// startup and creates replies without jobs. So this also handles what that
/// build left behind: an active reply with no job is orphaned (as that build
/// would), and where a job is active but its reply has finished, the reply
/// wins.
pub fn recover(conn: &mut Connection, queue_timeout: Duration) -> Result<Recovery> {
    let active = active_sql();
    let now = now_ts();
    let deadline = ts(SystemTime::now() + queue_timeout);
    let tx = conn.transaction()?;
    let mut recovery = Recovery::default();

    // 1. The reply already finished (an older build did it): the reply wins.
    let settled: Vec<String> = {
        let mut stmt = tx.prepare(&format!(
            "SELECT j.id FROM jobs j JOIN messages m ON m.id = j.message_id
             WHERE j.status IN ({active}) AND m.status NOT IN ({active})"
        ))?;
        let ids = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        ids
    };
    for job_id in &settled {
        close_open_attempts(&tx, job_id, &now)?;
        tx.execute(
            "UPDATE jobs SET
               status = (SELECT status FROM messages WHERE id = jobs.message_id),
               error = (SELECT error FROM messages WHERE id = jobs.message_id),
               finished_at = COALESCE((SELECT completed_at FROM messages WHERE id = jobs.message_id), ?2)
             WHERE id = ?1",
            (job_id, &now),
        )?;
    }
    recovery.settled = settled.len();

    // 2. Active replies with no job: orphaned, keeping their partial text.
    let jobless =
        format!("status IN ({active}) AND NOT EXISTS (SELECT 1 FROM jobs j WHERE j.message_id = messages.id)");
    tx.execute(
        &format!(
            "UPDATE conversations SET updated_at = ?1
             WHERE id IN (SELECT conversation_id FROM messages WHERE {jobless})"
        ),
        [&now],
    )?;
    recovery.orphaned_without_job = tx.execute(
        &format!("UPDATE messages SET status = 'orphaned', error = ?2, completed_at = ?1 WHERE {jobless}"),
        (&now, INTERRUPTED),
    )?;

    // 3. Jobs that were mid-attempt.
    let interrupted: Vec<(String, String, u32, u32)> = {
        let mut stmt = tx.prepare(
            "SELECT id, message_id, attempts, max_attempts FROM jobs
             WHERE status IN ('assigned', 'running') AND message_id IS NOT NULL",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        rows
    };
    for (job_id, message_id, attempts, max_attempts) in &interrupted {
        close_open_attempts(&tx, job_id, &now)?;
        if attempts < max_attempts {
            tx.execute(
                "UPDATE jobs SET status = 'queued', attempt_id = NULL, client_id = NULL, queued_at = ?2,
                   deadline_at = ?3, not_before = NULL
                 WHERE id = ?1",
                (job_id, &now, &deadline),
            )?;
            // The next attempt starts again from nothing.
            tx.execute(
                "UPDATE messages SET status = 'queued', content = '', error = NULL, attempt_id = NULL,
                   client_id = NULL, client_label = NULL, model = NULL, meta_json = NULL
                 WHERE id = ?1",
                [message_id],
            )?;
            recovery.requeued += 1;
        } else {
            tx.execute(
                "UPDATE jobs SET status = 'orphaned', error = ?2, finished_at = ?3 WHERE id = ?1",
                (job_id, INTERRUPTED, &now),
            )?;
            tx.execute(
                "UPDATE messages SET status = 'orphaned', error = ?2, completed_at = ?3 WHERE id = ?1",
                (message_id, INTERRUPTED, &now),
            )?;
            tx.execute(
                "UPDATE conversations SET updated_at = ?2
                 WHERE id = (SELECT conversation_id FROM messages WHERE id = ?1)",
                (message_id, &now),
            )?;
            recovery.orphaned += 1;
        }
    }

    // 4. Everything queued waits afresh for its clients to reconnect.
    tx.execute(
        "UPDATE jobs SET deadline_at = ?1, not_before = NULL WHERE status = 'queued'",
        [&deadline],
    )?;
    recovery.queued = {
        let mut stmt = tx.prepare(
            "SELECT j.id, j.message_id, m.conversation_id, j.owner_user_id, j.target_kind, j.pinned_client_id,
               j.model, j.attempts, j.max_attempts
             FROM jobs j JOIN messages m ON m.id = j.message_id
             WHERE j.status = 'queued' AND j.kind = 'chat_reply'
             ORDER BY j.queued_at, j.rowid",
        )?;
        let rows = stmt
            .query_map([], |r| {
                let kind: String = r.get(4)?;
                Ok((
                    target_from(&kind, r.get(5)?, r.get(6)?),
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .filter_map(
                |(target, job_id, message_id, conversation_id, owner_user_id, attempts, max_attempts)| {
                    Some(QueuedReply {
                        target: target?,
                        job_id,
                        message_id,
                        conversation_id,
                        owner_user_id,
                        attempts,
                        max_attempts,
                    })
                },
            )
            .collect()
    };
    tx.commit()?;
    Ok(recovery)
}

fn close_open_attempts(conn: &Connection, job_id: &str, now: &str) -> Result<()> {
    conn.execute(
        "UPDATE job_attempts SET ended_at = ?2, outcome = 'interrupted', error = ?3
         WHERE job_id = ?1 AND ended_at IS NULL",
        (job_id, now, INTERRUPTED),
    )?;
    Ok(())
}

// ---------------- reading, for the API ----------------

/// One attempt at a job.
#[derive(Debug, Serialize)]
pub struct AttemptView {
    pub id: String,
    pub number: i64,
    /// Null once that client has been deleted; the label keeps its name.
    pub client_id: Option<String>,
    pub client_label: String,
    pub model: String,
    pub assigned_at: String,
    pub first_output_at: Option<String>,
    pub ended_at: Option<String>,
    /// Null while it's running.
    pub outcome: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TargetView {
    pub kind: String,
    pub client_id: Option<String>,
    /// The pinned client's current name; null for a pool, or once deleted.
    pub client_label: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct JobOwner {
    pub user_id: String,
    pub username: String,
}

/// A job as `GET /api/jobs` returns it: metadata only, never the reply's
/// (or the conversation's) text.
#[derive(Debug, Serialize)]
pub struct JobView {
    pub id: String,
    pub kind: JobKind,
    pub status: JobStatus,
    pub owner: JobOwner,
    pub target: TargetView,
    pub model: String,
    /// Attempts that reached a client, refusals not counted.
    pub attempts: i64,
    pub max_attempts: i64,
    pub error: Option<String>,
    pub created_at: String,
    /// When it last entered the queue.
    pub queued_at: String,
    pub finished_at: Option<String>,
    pub message_id: Option<String>,
    pub conversation_id: Option<String>,
    /// How long it waited the last time it was queued: until its newest
    /// attempt was assigned, until now if it's still waiting, or until it
    /// gave up waiting.
    pub queue_wait_ms: Option<u64>,
    /// How long its newest attempt ran (so far, if it's still running).
    pub run_ms: Option<u64>,
    /// Every attempt, oldest first.
    pub history: Vec<AttemptView>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobFilter {
    /// Queued, assigned or running; oldest in the queue first.
    Active,
    /// Finished; most recently finished first.
    Recent,
    /// Everything, newest first.
    All,
}

const JOB_COLUMNS: &str = "j.id, j.kind, j.status, j.owner_user_id, u.username, j.target_kind, j.pinned_client_id,
    COALESCE(pc.friendly_name, pc.hostname), j.model, j.attempts, j.max_attempts, j.error, j.created_at,
    j.queued_at, j.finished_at, j.message_id, m.conversation_id";

const JOB_JOINS: &str = "FROM jobs j JOIN users u ON u.id = j.owner_user_id
    LEFT JOIN messages m ON m.id = j.message_id
    LEFT JOIN clients pc ON pc.client_id = j.pinned_client_id";

fn job_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<JobView> {
    Ok(JobView {
        id: r.get(0)?,
        kind: r.get(1)?,
        status: r.get(2)?,
        owner: JobOwner {
            user_id: r.get(3)?,
            username: r.get(4)?,
        },
        target: TargetView {
            kind: r.get(5)?,
            client_id: r.get(6)?,
            client_label: r.get(7)?,
        },
        model: r.get(8)?,
        attempts: r.get(9)?,
        max_attempts: r.get(10)?,
        error: r.get(11)?,
        created_at: r.get(12)?,
        queued_at: r.get(13)?,
        finished_at: r.get(14)?,
        message_id: r.get(15)?,
        conversation_id: r.get(16)?,
        queue_wait_ms: None,
        run_ms: None,
        history: Vec::new(),
    })
}

/// Fills in the attempts and the two durations.
fn complete(conn: &Connection, mut job: JobView, now: SystemTime) -> Result<JobView> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, number, client_id, client_label, model, assigned_at, first_output_at, ended_at, outcome, error
         FROM job_attempts WHERE job_id = ?1 ORDER BY assigned_at, rowid",
    )?;
    job.history = stmt
        .query_map([&job.id], |r| {
            Ok(AttemptView {
                id: r.get(0)?,
                number: r.get(1)?,
                client_id: r.get(2)?,
                client_label: r.get(3)?,
                model: r.get(4)?,
                assigned_at: r.get(5)?,
                first_output_at: r.get(6)?,
                ended_at: r.get(7)?,
                outcome: r.get(8)?,
                error: r.get(9)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    let between = |from: &str, to: SystemTime| -> Option<u64> {
        let ms = to.duration_since(parse_ts(from)?).unwrap_or_default().as_millis();
        Some(u64::try_from(ms).unwrap_or(u64::MAX))
    };
    // The attempt this wait in the queue led to, if it has led to one.
    let newest = job
        .history
        .last()
        .filter(|a| a.assigned_at.as_str() >= job.queued_at.as_str());
    let waited_until = match (newest, job.status, &job.finished_at) {
        (Some(attempt), _, _) => parse_ts(&attempt.assigned_at),
        (None, JobStatus::Queued, _) => Some(now),
        (None, _, Some(finished_at)) => parse_ts(finished_at),
        (None, _, None) => None,
    };
    job.queue_wait_ms = waited_until.and_then(|until| between(&job.queued_at, until));
    job.run_ms = job.history.last().and_then(|a| {
        let end = match &a.ended_at {
            Some(ended_at) => parse_ts(ended_at)?,
            None => now,
        };
        between(&a.assigned_at, end)
    });
    Ok(job)
}

/// `owner`'s jobs, or everyone's with None.
pub fn list_jobs(conn: &Connection, owner: Option<&str>, filter: JobFilter, limit: usize) -> Result<Vec<JobView>> {
    let active = active_sql();
    let (condition, order) = match filter {
        JobFilter::Active => (format!("j.status IN ({active})"), "j.queued_at, j.rowid"),
        JobFilter::Recent => (
            format!("j.status NOT IN ({active})"),
            "j.finished_at DESC, j.rowid DESC",
        ),
        JobFilter::All => ("1".to_string(), "j.created_at DESC, j.rowid DESC"),
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT {JOB_COLUMNS} {JOB_JOINS}
         WHERE (?1 IS NULL OR j.owner_user_id = ?1) AND {condition}
         ORDER BY {order} LIMIT ?2"
    ))?;
    let jobs: Vec<JobView> = stmt
        .query_map((owner, limit as i64), job_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    let now = SystemTime::now();
    jobs.into_iter().map(|job| complete(conn, job, now)).collect()
}

/// One job, if it's `owner`'s (or anyone's with None).
pub fn get_job(conn: &Connection, owner: Option<&str>, job_id: &str) -> Result<Option<JobView>> {
    let job = conn
        .query_row(
            &format!(
                "SELECT {JOB_COLUMNS} {JOB_JOINS}
                 WHERE j.id = ?1 AND (?2 IS NULL OR j.owner_user_id = ?2)"
            ),
            (job_id, owner),
            job_from_row,
        )
        .optional()?;
    job.map(|job| complete(conn, job, SystemTime::now())).transpose()
}

/// A job's and its reply's status, and the outcomes of its attempts in
/// order, for tests.
#[cfg(test)]
pub fn job_state(conn: &Connection, message_id: &str) -> Result<(JobStatus, JobStatus, Vec<String>)> {
    let (job_id, job, message): (String, JobStatus, JobStatus) = conn.query_row(
        "SELECT j.id, j.status, m.status FROM jobs j JOIN messages m ON m.id = j.message_id WHERE m.id = ?1",
        [message_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let mut stmt = conn
        .prepare("SELECT COALESCE(outcome, 'open') FROM job_attempts WHERE job_id = ?1 ORDER BY assigned_at, rowid")?;
    let outcomes = stmt
        .query_map([&job_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok((job, message, outcomes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversations::{create_turn, message_state, NewTurn, TurnOutcome};
    use crate::db::tests::{add_client, add_user, memory_db};

    fn pinned() -> Target {
        Target::Client {
            client_id: "c1".into(),
            model: "echo".into(),
        }
    }

    /// A conversation with one queued reply and its job; returns the reply id.
    fn turn(c: &mut Connection, owner: &str, conversation: &str, target: &Target, max_attempts: u32) -> String {
        match create_turn(
            c,
            NewTurn {
                owner,
                conversation_id: conversation,
                content: Some("hello"),
                parent_id: None,
                max_active_per_user: 10,
                target,
                max_attempts,
                queue_timeout: Duration::from_secs(120),
            },
        )
        .unwrap()
        {
            TurnOutcome::Created(turn) => turn.reply.id,
            _ => panic!("not created"),
        }
    }

    /// Puts a job and its reply mid-attempt, as the writer would.
    fn start_attempt(c: &Connection, message_id: &str, attempts: u32) {
        c.execute(
            "UPDATE jobs SET status = 'running', attempt_id = 'att', attempts = ?2, client_id = 'c1'
             WHERE message_id = ?1",
            (message_id, attempts),
        )
        .unwrap();
        c.execute(
            "UPDATE messages SET status = 'running', content = 'partial', attempt_id = 'att', attempts = ?2,
               client_id = 'c1', client_label = 'host', model = 'echo'
             WHERE id = ?1",
            (message_id, attempts),
        )
        .unwrap();
        c.execute(
            "INSERT INTO job_attempts (id, job_id, number, client_id, client_label, model, assigned_at)
             SELECT 'att-' || id, id, ?2, 'c1', 'host', 'echo', 'then' FROM jobs WHERE message_id = ?1",
            (message_id, attempts),
        )
        .unwrap();
    }

    #[test]
    fn a_restart_requeues_interrupted_jobs_until_their_attempts_run_out() {
        let db = memory_db();
        let owner = add_user(&db, "alice", "user");
        add_client(&db, "c1", &owner);
        db.blocking(|c| {
            let waiting = turn(c, &owner, "waiting", &Target::Pool { model: "echo".into() }, 3);
            let running = turn(c, &owner, "running", &pinned(), 3);
            start_attempt(c, &running, 1);
            let spent = turn(c, &owner, "spent", &pinned(), 3);
            start_attempt(c, &spent, 3);

            let recovery = recover(c, Duration::from_secs(60))?;
            assert_eq!(
                (
                    recovery.requeued,
                    recovery.orphaned,
                    recovery.orphaned_without_job,
                    recovery.settled
                ),
                (1, 1, 0, 0)
            );
            // Both queued jobs are loaded, the one that was waiting first.
            let loaded: Vec<&str> = recovery.queued.iter().map(|j| j.message_id.as_str()).collect();
            assert_eq!(loaded, vec![waiting.as_str(), running.as_str()]);
            assert_eq!(recovery.queued[0].target, Target::Pool { model: "echo".into() });
            assert_eq!(recovery.queued[1].target, pinned());
            assert_eq!((recovery.queued[1].attempts, recovery.queued[1].max_attempts), (1, 3));

            // Re-queued: the text is cleared and the attempt is recorded.
            assert_eq!(
                job_state(c, &running)?,
                (JobStatus::Queued, JobStatus::Queued, vec!["interrupted".to_string()])
            );
            assert_eq!(message_state(c, &running)?.unwrap().1, "");
            let label: Option<String> =
                c.query_row("SELECT client_label FROM messages WHERE id = ?1", [&running], |r| {
                    r.get(0)
                })?;
            assert_eq!(label, None);

            // Out of attempts: orphaned, keeping its partial text.
            assert_eq!(
                job_state(c, &spent)?,
                (
                    JobStatus::Orphaned,
                    JobStatus::Orphaned,
                    vec!["interrupted".to_string()]
                )
            );
            assert_eq!(message_state(c, &spent)?.unwrap().1, "partial");
            let error: String = c.query_row("SELECT error FROM jobs WHERE message_id = ?1", [&spent], |r| r.get(0))?;
            assert_eq!(error, INTERRUPTED);

            // Running it again changes nothing more.
            let again = recover(c, Duration::from_secs(60))?;
            assert_eq!((again.requeued, again.orphaned, again.queued.len()), (0, 0, 2));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn recovery_handles_what_a_rolled_back_build_left_behind() {
        let db = memory_db();
        let owner = add_user(&db, "alice", "user");
        add_client(&db, "c1", &owner);
        db.blocking(|c| {
            // The older build orphaned this reply on its own startup; its
            // job still says running.
            let orphaned_by_old = turn(c, &owner, "a", &pinned(), 3);
            start_attempt(c, &orphaned_by_old, 1);
            c.execute(
                "UPDATE messages SET status = 'orphaned', error = 'Interrupted by a server restart',
                   completed_at = 'earlier' WHERE id = ?1",
                [&orphaned_by_old],
            )?;
            // This one was retried and completed by the older build while
            // its job sat queued.
            let completed_by_old = turn(c, &owner, "b", &pinned(), 3);
            c.execute(
                "UPDATE messages SET status = 'completed', content = 'done', completed_at = 'later' WHERE id = ?1",
                [&completed_by_old],
            )?;
            // And the older build created this reply with no job at all.
            c.execute_batch(
                "INSERT INTO conversations VALUES ('old', (SELECT id FROM users), 'Old', 'r', 't', 't');
                 INSERT INTO messages (id, conversation_id, role, content, status, created_at)
                   VALUES ('q', 'old', 'user', 'hi', 'completed', 't');
                 INSERT INTO messages (id, conversation_id, parent_id, role, content, status, created_at)
                   VALUES ('r', 'old', 'q', 'assistant', 'half', 'running', 't');",
            )?;

            let recovery = recover(c, Duration::from_secs(60))?;
            assert_eq!(
                (
                    recovery.settled,
                    recovery.orphaned_without_job,
                    recovery.requeued,
                    recovery.orphaned
                ),
                (2, 1, 0, 0)
            );
            assert!(
                recovery.queued.is_empty(),
                "nothing is re-queued: {:?}",
                recovery.queued
            );

            // The message won, and the open attempt was closed.
            assert_eq!(
                job_state(c, &orphaned_by_old)?,
                (
                    JobStatus::Orphaned,
                    JobStatus::Orphaned,
                    vec!["interrupted".to_string()]
                )
            );
            let (error, finished): (String, String) = c.query_row(
                "SELECT error, finished_at FROM jobs WHERE message_id = ?1",
                [&orphaned_by_old],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert_eq!((error.as_str(), finished.as_str()), (INTERRUPTED, "earlier"));
            assert_eq!(
                job_state(c, &completed_by_old)?,
                (JobStatus::Completed, JobStatus::Completed, vec![])
            );
            assert_eq!(message_state(c, &completed_by_old)?.unwrap().1, "done");

            // The reply without a job is orphaned as before, text kept.
            assert_eq!(
                message_state(c, "r")?.unwrap(),
                (JobStatus::Orphaned, "half".to_string())
            );
            assert_eq!(job_of_message(c, "r")?, None);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn deleting_a_conversation_or_user_removes_their_jobs_and_attempts() {
        let db = memory_db();
        let owner = add_user(&db, "alice", "user");
        add_client(&db, "c1", &owner);
        db.blocking(|c| {
            let count = |c: &Connection, table: &str| -> i64 {
                c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                    .unwrap()
            };
            let a = turn(c, &owner, "a", &pinned(), 3);
            start_attempt(c, &a, 1);
            let b = turn(c, &owner, "b", &pinned(), 3);
            start_attempt(c, &b, 1);
            assert_eq!((count(c, "jobs"), count(c, "job_attempts")), (2, 2));

            // What a job was aimed at, for Retry's default.
            assert!(matches!(original_target(c, &a)?, OriginalTarget::Target(t) if t == pinned()));
            assert!(matches!(original_target(c, "nope")?, OriginalTarget::NoJob));

            c.execute("DELETE FROM conversations WHERE id = 'a'", [])?;
            assert_eq!((count(c, "jobs"), count(c, "job_attempts")), (1, 1));

            // A finished job outlives its client, which it then says is gone.
            c.execute("UPDATE jobs SET status = 'failed'", [])?;
            c.execute("DELETE FROM clients", [])?;
            assert!(matches!(original_target(c, &b)?, OriginalTarget::ClientDeleted));
            let label: String = c.query_row("SELECT client_label FROM job_attempts", [], |r| r.get(0))?;
            assert_eq!(label, "host", "the attempt still says who it was");

            c.execute("DELETE FROM users", [])?;
            assert_eq!((count(c, "jobs"), count(c, "job_attempts")), (0, 0));
            Ok(())
        })
        .unwrap();
    }
}
