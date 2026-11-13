//! Model commands sent to clients, tracked in memory: who asked for what,
//! where it's at, and how it ended. Finished ones are kept for an hour so
//! the dashboard can show them; a server restart forgets them all.
//!
//! A command belongs to the connection it was sent on. If that connection
//! closes first, the command fails (a pull's partial file stays on the
//! client, so pulling again resumes it), and statuses from any other
//! connection are ignored.

use crate::live::Live;
use crate::{debug, log};
use protocol::{CommandState, ModelCommandKind, Progress, ServerMessage};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};
use uuid::Uuid;

/// How long finished commands stay listed.
pub const KEEP_FINISHED: Duration = Duration::from_secs(3_600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandKind {
    Pull,
    Delete,
    /// `select_runtime`; `model_id` is empty and `model_name` names the variant.
    SelectRuntime,
    Configure,
}

/// The client's states, plus `sent` for before it has answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackedState {
    Sent,
    Accepted,
    InProgress,
    Done,
    Failed,
    Cancelled,
    Unsupported,
}

impl TrackedState {
    pub fn is_final(self) -> bool {
        !matches!(
            self,
            TrackedState::Sent | TrackedState::Accepted | TrackedState::InProgress
        )
    }
}

impl From<CommandState> for TrackedState {
    fn from(s: CommandState) -> Self {
        match s {
            CommandState::Accepted => TrackedState::Accepted,
            CommandState::InProgress => TrackedState::InProgress,
            CommandState::Done => TrackedState::Done,
            CommandState::Failed => TrackedState::Failed,
            CommandState::Cancelled => TrackedState::Cancelled,
            CommandState::Unsupported => TrackedState::Unsupported,
        }
    }
}

/// One command, as the API returns it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TrackedCommand {
    pub command_id: String,
    pub client_id: String,
    pub kind: CommandKind,
    pub model_id: String,
    pub model_name: String,
    pub state: TrackedState,
    pub progress: Option<Progress>,
    pub error: Option<String>,
    /// The username of whoever asked.
    pub requested_by: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    #[serde(skip)]
    conn_id: Option<u64>,
    /// Creation order, for sorting commands made in the same millisecond.
    #[serde(skip)]
    seq: u64,
    #[serde(skip)]
    finished: Option<Instant>,
}

impl TrackedCommand {
    fn finish(&mut self, state: TrackedState, error: Option<String>) {
        self.state = state;
        self.error = error;
        self.finished_at = Some(now());
        self.finished = Some(Instant::now());
    }
}

/// What the dashboard asked for.
pub struct NewCommand<'a> {
    pub client_id: &'a str,
    pub kind: CommandKind,
    pub model_id: &'a str,
    pub model_name: &'a str,
    pub requested_by: &'a str,
    pub command: ModelCommandKind,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CancelError {
    NotFound,
    /// Only pulls in progress can be cancelled.
    NotCancellable,
    Offline,
}

#[derive(Default)]
pub struct ModelCommands {
    inner: Mutex<HashMap<String, TrackedCommand>>,
    next_seq: std::sync::atomic::AtomicU64,
}

fn now() -> String {
    protocol::utc_timestamp(SystemTime::now(), true)
}

impl ModelCommands {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, TrackedCommand>> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.retain(|_, c| c.finished.is_none_or(|t| t.elapsed() < KEEP_FINISHED));
        inner
    }

    /// Sends a command to a client and starts tracking it. A disconnected
    /// client's command fails at once ("offline"), and so does one whose
    /// queue is full. A pull of a model the client is already pulling for
    /// this server returns that command instead of sending another.
    pub fn send(&self, live: &Live, new: NewCommand<'_>) -> TrackedCommand {
        if new.kind == CommandKind::Pull {
            let existing = self
                .lock()
                .values()
                .find(|c| {
                    c.client_id == new.client_id
                        && c.kind == CommandKind::Pull
                        && c.model_id == new.model_id
                        && !c.state.is_final()
                })
                .cloned();
            if let Some(existing) = existing {
                return existing;
            }
        }
        let mut command = TrackedCommand {
            command_id: Uuid::new_v4().to_string(),
            client_id: new.client_id.to_string(),
            kind: new.kind,
            model_id: new.model_id.to_string(),
            model_name: new.model_name.to_string(),
            state: TrackedState::Sent,
            progress: None,
            error: None,
            requested_by: new.requested_by.to_string(),
            started_at: now(),
            finished_at: None,
            conn_id: None,
            seq: self.next_seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            finished: None,
        };
        match live.sender(new.client_id) {
            None => command.finish(TrackedState::Failed, Some("The client is offline.".into())),
            Some(conn) => {
                let message = ServerMessage::ModelCommand {
                    command_id: command.command_id.clone(),
                    command: new.command,
                };
                if conn.tx.try_send(message).is_err() {
                    command.finish(TrackedState::Failed, Some("The client is busy. Try again.".into()));
                } else {
                    command.conn_id = Some(conn.conn_id);
                }
            }
        }
        log(format!(
            "[models] {} asked {} to {} {}: {}",
            new.requested_by,
            new.client_id,
            match new.kind {
                CommandKind::Pull => "pull",
                CommandKind::Delete => "delete",
                CommandKind::SelectRuntime => "select runtime",
                CommandKind::Configure => "configure",
            },
            new.model_id,
            match &command.error {
                Some(error) => error.clone(),
                None => format!("sent as {}", command.command_id),
            }
        ));
        self.lock().insert(command.command_id.clone(), command.clone());
        command
    }

    /// A `model_command_status` from connection `conn_id`.
    pub fn on_status(
        &self,
        conn_id: u64,
        command_id: &str,
        state: CommandState,
        progress: Option<Progress>,
        error: Option<String>,
    ) {
        let mut inner = self.lock();
        let Some(command) = inner.get_mut(command_id) else {
            // A cancel's own status, or a command from before a restart.
            debug(|| format!("[models] status for unknown command {command_id}"));
            return;
        };
        if command.conn_id != Some(conn_id) || command.state.is_final() {
            debug(|| format!("[models] ignored a late or misdirected status for {command_id}"));
            return;
        }
        if progress.is_some() {
            command.progress = progress;
        }
        let state = TrackedState::from(state);
        if state.is_final() {
            if let Some(error) = &error {
                log(format!(
                    "[models] {command_id} ({} on {}) failed: {error}",
                    command.model_id, command.client_id
                ));
            } else {
                log(format!(
                    "[models] {command_id} ({} on {}): {state:?}",
                    command.model_id, command.client_id
                ));
            }
            command.finish(state, error);
        } else {
            command.state = state;
        }
    }

    /// The connection closed: everything still running on it fails.
    pub fn on_connection_closed(&self, conn_id: u64) {
        let mut inner = self.lock();
        for command in inner.values_mut() {
            if command.conn_id == Some(conn_id) && !command.state.is_final() {
                let why = match command.kind {
                    CommandKind::Pull => {
                        "The client disconnected before the download finished. Install it again to resume."
                    }
                    CommandKind::Delete | CommandKind::SelectRuntime | CommandKind::Configure => {
                        "The client disconnected before it finished."
                    }
                };
                command.finish(TrackedState::Failed, Some(why.into()));
            }
        }
    }

    /// The client's commands, newest first.
    pub fn for_client(&self, client_id: &str) -> Vec<TrackedCommand> {
        let mut list: Vec<TrackedCommand> = self
            .lock()
            .values()
            .filter(|c| c.client_id == client_id)
            .cloned()
            .collect();
        list.sort_by_key(|c| std::cmp::Reverse(c.seq));
        list
    }

    /// Pulls in progress, by client id.
    pub fn active_pulls(&self) -> HashMap<String, Vec<TrackedCommand>> {
        let mut by_client: HashMap<String, Vec<TrackedCommand>> = HashMap::new();
        for c in self.lock().values() {
            if c.kind == CommandKind::Pull && !c.state.is_final() {
                by_client.entry(c.client_id.clone()).or_default().push(c.clone());
            }
        }
        by_client
    }

    /// Asks the client to stop pull `command_id`. Its own status then
    /// becomes `cancelled`.
    pub fn cancel(&self, live: &Live, client_id: &str, command_id: &str) -> Result<TrackedCommand, CancelError> {
        let command = self
            .lock()
            .get(command_id)
            .filter(|c| c.client_id == client_id)
            .cloned()
            .ok_or(CancelError::NotFound)?;
        if command.kind != CommandKind::Pull || command.state.is_final() {
            return Err(CancelError::NotCancellable);
        }
        let conn = live
            .sender(client_id)
            .filter(|c| Some(c.conn_id) == command.conn_id)
            .ok_or(CancelError::Offline)?;
        let message = ServerMessage::ModelCommand {
            command_id: Uuid::new_v4().to_string(),
            command: ModelCommandKind::Cancel {
                target_command_id: command_id.to_string(),
            },
        };
        conn.tx.try_send(message).map_err(|_| CancelError::Offline)?;
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MetricsLimits;
    use protocol::ProgressPhase;

    fn live() -> Live {
        Live::new(MetricsLimits {
            history_samples: 2,
            min_spacing: Duration::ZERO,
            max_gpus: 1,
            max_string_len: 8,
        })
    }

    fn pull<'a>(client_id: &'a str) -> NewCommand<'a> {
        NewCommand {
            client_id,
            kind: CommandKind::Pull,
            model_id: "m",
            model_name: "M",
            requested_by: "alice",
            command: ModelCommandKind::Delete { model_id: "m".into() },
        }
    }

    #[test]
    fn offline_clients_fail_at_once() {
        let commands = ModelCommands::default();
        let c = commands.send(&live(), pull("gone"));
        assert_eq!(c.state, TrackedState::Failed);
        assert_eq!(c.error.as_deref(), Some("The client is offline."));
        assert!(c.finished_at.is_some());
        assert_eq!(commands.for_client("gone"), vec![c]);
    }

    #[tokio::test]
    async fn statuses_from_the_right_connection_move_a_command_along() {
        let live = live();
        let mut conn = live.register("c");
        let commands = ModelCommands::default();
        let c = commands.send(&live, pull("c"));
        assert_eq!(c.state, TrackedState::Sent);
        assert!(matches!(
            conn.outbound.recv().await,
            Some(ServerMessage::ModelCommand { command_id, .. }) if command_id == c.command_id
        ));
        // A second pull of the same model is the same command.
        assert_eq!(commands.send(&live, pull("c")).command_id, c.command_id);
        assert_eq!(commands.active_pulls()["c"].len(), 1);

        let progress = Progress {
            done_bytes: 5,
            total_bytes: 10,
            phase: ProgressPhase::Model,
        };
        commands.on_status(conn.id + 1, &c.command_id, CommandState::Done, None, None);
        assert_eq!(
            commands.for_client("c")[0].state,
            TrackedState::Sent,
            "another connection's status"
        );
        commands.on_status(conn.id, &c.command_id, CommandState::InProgress, Some(progress), None);
        let now = &commands.for_client("c")[0];
        assert_eq!((now.state, now.progress), (TrackedState::InProgress, Some(progress)));

        // Cancel goes out as a new command naming this one.
        commands.cancel(&live, "c", &c.command_id).unwrap();
        assert!(matches!(
            conn.outbound.recv().await,
            Some(ServerMessage::ModelCommand { command: ModelCommandKind::Cancel { target_command_id }, .. })
                if target_command_id == c.command_id
        ));
        assert_eq!(
            commands.cancel(&live, "other", &c.command_id),
            Err(CancelError::NotFound)
        );

        commands.on_status(conn.id, &c.command_id, CommandState::Cancelled, None, None);
        assert_eq!(commands.for_client("c")[0].state, TrackedState::Cancelled);
        assert_eq!(
            commands.cancel(&live, "c", &c.command_id),
            Err(CancelError::NotCancellable)
        );
        assert!(commands.active_pulls().is_empty());
    }

    #[test]
    fn a_closed_connection_fails_what_it_had_running() {
        let live = live();
        let conn = live.register("c");
        let commands = ModelCommands::default();
        let c = commands.send(&live, pull("c"));
        commands.on_connection_closed(conn.id + 1);
        assert_eq!(commands.for_client("c")[0].state, TrackedState::Sent);
        commands.on_connection_closed(conn.id);
        let after = &commands.for_client("c")[0];
        assert_eq!(after.state, TrackedState::Failed);
        assert!(after.error.as_deref().unwrap().contains("resume"));
        // Late statuses don't revive it.
        commands.on_status(conn.id, &c.command_id, CommandState::Done, None, None);
        assert_eq!(commands.for_client("c")[0].state, TrackedState::Failed);
    }
}
