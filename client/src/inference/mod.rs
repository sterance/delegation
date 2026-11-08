//! `Inference`: everything about models and running them, as one service
//! the rest of the client talks to.
//!
//! It's created once at startup, outside any connection, and shared with
//! every session, so a reconnect never stops a running `llama-server` or a
//! download in progress. Chat is one consumer (`chat.rs` only translates
//! between the protocol and [`Inference::chat`]); future job types become
//! more methods here, not new code paths in the session.
//!
//! - `store`: models on disk (pull, verify, delete, list)
//! - `runtime`: the pinned llama.cpp build, fetched on first use
//! - `llama_cpp`: the `llama-server` process and chat streaming
//! - `commands`: the server's model commands
//! - `echo`: the built-in test model

pub mod commands;
pub mod echo;
pub mod llama_cpp;
pub mod runtime;
pub mod store;

use log::info;
use protocol::{
    Backend, ChatTurn, ClientMessage, CommandState, InferenceSettings, Inventory, LocalModel, ModelCommandKind,
    ModelState, Progress, Stage, Storage,
};
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};

/// Bumped on every inventory change; the value itself means nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct InventoryVersion(pub u64);

/// What a chat completion produces.
#[derive(Debug, Clone, PartialEq)]
pub enum InferenceEvent {
    /// Where the reply is before (or as) text arrives.
    Status(Stage),
    /// The next piece of text.
    Delta(String),
}

/// A generic chat completion request.
#[derive(Debug, Clone)]
pub struct ChatCompletion {
    pub model_id: String,
    /// Oldest first, ending with the user's message.
    pub messages: Vec<ChatTurn>,
    /// The longest reply, in tokens. None: `INFERENCE_MAX_TOKENS`.
    pub max_tokens: Option<u32>,
    /// None: the model's default.
    pub temperature: Option<f32>,
}

/// Why a reply couldn't be generated; `Display` is what the user sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InferenceError {
    UnknownModel(String),
    StillDownloading(String),
    /// Already generating `max_concurrent` replies.
    Busy,
    /// Another model is loaded and in use.
    BusyWithAnotherModel(String),
    LoadFailed(String),
    Crashed(String),
    Backend(String),
    NoUserMessage,
    /// Nobody is listening for the reply any more.
    Closed,
}

impl std::fmt::Display for InferenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InferenceError::UnknownModel(id) => write!(f, "this client doesn't have the model {id:?}"),
            InferenceError::StillDownloading(id) => write!(f, "the model {id:?} is still downloading"),
            InferenceError::Busy => f.write_str(
                "client busy: it's already generating as many replies as it may at once (INFERENCE_MAX_CONCURRENT)",
            ),
            InferenceError::BusyWithAnotherModel(other) => {
                write!(f, "busy with another model ({other}); try again when its reply is done")
            }
            InferenceError::LoadFailed(why) => write!(f, "couldn't load the model: {why}"),
            InferenceError::Crashed(why) => write!(f, "llama-server crashed: {why}"),
            InferenceError::Backend(why) => write!(f, "llama-server: {why}"),
            InferenceError::NoUserMessage => f.write_str("there's no user message to answer"),
            InferenceError::Closed => f.write_str("the reply was abandoned"),
        }
    }
}

impl std::error::Error for InferenceError {}

/// A model command's progress or outcome, for `model_command_status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandStatus {
    pub command_id: String,
    pub state: CommandState,
    pub progress: Option<Progress>,
    pub error: Option<String>,
}

impl CommandStatus {
    pub fn into_message(self) -> ClientMessage {
        ClientMessage::ModelCommandStatus {
            command_id: self.command_id,
            state: self.state,
            progress: self.progress,
            error: self.error,
        }
    }
}

pub(crate) struct Shared {
    pub(crate) store: store::Store,
    pub(crate) runtime: runtime::Runtime,
    pub(crate) llama: llama_cpp::LlamaCpp,
    pub(crate) pulls: commands::Pulls,
    settings: Mutex<InferenceSettings>,
    version: watch::Sender<InventoryVersion>,
}

impl Shared {
    pub(crate) fn settings(&self) -> InferenceSettings {
        *self.settings.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The inventory changed; sessions send a fresh one (debounced).
    pub(crate) fn changed(&self) {
        self.version.send_modify(|v| v.0 += 1);
    }
}

#[derive(Clone)]
pub struct Inference {
    shared: Arc<Shared>,
}

impl Inference {
    /// Opens the data directory (models and runtimes). Blocking, briefly:
    /// it lists the models on disk. Needs no tokio runtime.
    pub fn new(data_dir: &Path) -> Inference {
        info!(target: "models", "data directory {}", data_dir.display());
        Self::from_parts(store::Store::open(data_dir), runtime::Runtime::new(data_dir))
    }

    pub(crate) fn from_parts(store: store::Store, runtime: runtime::Runtime) -> Inference {
        Inference {
            shared: Arc::new(Shared {
                store,
                runtime,
                llama: llama_cpp::LlamaCpp::default(),
                pulls: commands::Pulls::default(),
                settings: Mutex::new(InferenceSettings::default()),
                version: watch::channel(InventoryVersion::default()).0,
            }),
        }
    }

    /// Models on disk plus their runtime state, for `model_inventory`.
    pub fn inventory(&self) -> Inventory {
        let s = &self.shared;
        let settings = s.settings();
        let loaded = s.llama.status();
        let pulls = s.pulls.snapshot();
        let mut models: Vec<LocalModel> = Vec::new();
        if settings.echo_model_enabled {
            models.push(echo::local_model());
        }
        let on_disk = s.store.list();
        for m in &on_disk {
            let state = match &loaded {
                llama_cpp::ProcStatus::Loaded(id) if *id == m.id => ModelState::Loaded,
                llama_cpp::ProcStatus::Loading(id) if *id == m.id => ModelState::Loading,
                _ => ModelState::Available,
            };
            models.push(LocalModel {
                id: m.id.clone(),
                display_name: m.display_name.clone(),
                backend: m.backend,
                state,
                size_bytes: m.size_bytes,
                context_length: m.context_length,
                meta: m.meta.clone(),
                error: None,
            });
        }
        let present = |id: &str| on_disk.iter().any(|m| m.id == id);
        for spec in pulls.downloading.iter().filter(|spec| !present(&spec.id)) {
            models.push(from_spec(spec, ModelState::Downloading, None));
        }
        for (spec, why) in &pulls.failed {
            let pulling = pulls.downloading.iter().any(|d| d.id == spec.id);
            if !present(&spec.id) && !pulling {
                models.push(from_spec(spec, ModelState::Failed, Some(why.clone())));
            }
        }
        Inventory {
            models,
            storage: Storage {
                models_bytes: s.store.models_bytes(),
                free_bytes: s.store.free_bytes().unwrap_or(0),
            },
            runtimes: s.runtime.info().into_iter().collect(),
        }
    }

    /// Notified whenever the inventory changes (debounce in the session).
    pub fn subscribe(&self) -> watch::Receiver<InventoryVersion> {
        self.shared.version.subscribe()
    }

    /// Runs a model command from the server; status updates go to
    /// `status_tx`. Returns at once. Must be called within a tokio runtime.
    pub fn execute(&self, command_id: String, command: ModelCommandKind, status_tx: mpsc::Sender<CommandStatus>) {
        commands::execute(&self.shared, command_id, command, status_tx);
    }

    /// Stops a pull. Returns whether there was one to stop.
    pub fn cancel_command(&self, command_id: &str) -> bool {
        self.shared.pulls.cancel_command(command_id)
    }

    /// Generic chat completion: streams `Status` and `Delta` events on
    /// `out`. Dropping the future stops generation.
    pub async fn chat(&self, req: ChatCompletion, out: mpsc::Sender<InferenceEvent>) -> Result<(), InferenceError> {
        let settings = self.shared.settings();
        if req.model_id == echo::ECHO_MODEL && settings.echo_model_enabled {
            return echo::chat(&req.messages, &out).await;
        }
        let Some(manifest) = self.shared.store.get(&req.model_id) else {
            if self
                .shared
                .pulls
                .snapshot()
                .downloading
                .iter()
                .any(|s| s.id == req.model_id)
            {
                return Err(InferenceError::StillDownloading(req.model_id));
            }
            return Err(InferenceError::UnknownModel(req.model_id));
        };
        match manifest.backend {
            Backend::LlamaCpp => {
                let max_tokens = req.max_tokens.unwrap_or(settings.max_tokens).max(1);
                llama_cpp::chat(&self.shared, &manifest, &req, max_tokens, &out).await
            }
            Backend::Echo => Err(InferenceError::UnknownModel(req.model_id)),
        }
    }

    /// Applies the settings the server pushed in `welcome`.
    pub fn apply_settings(&self, s: &InferenceSettings) {
        let new = s.sanitized();
        let old = std::mem::replace(
            &mut *self.shared.settings.lock().unwrap_or_else(|e| e.into_inner()),
            new,
        );
        if old.echo_model_enabled != new.echo_model_enabled {
            self.shared.changed();
        }
    }

    /// Clean shutdown: stops downloads (keeping what they have, so they
    /// resume) and `llama-server`. Downloads run on blocking threads, which
    /// tokio waits for when its runtime shuts down, so they must stop.
    pub async fn shutdown(&self) {
        self.shared.pulls.cancel_all();
        llama_cpp::stop(&self.shared).await;
    }
}

fn from_spec(spec: &protocol::ModelSpec, state: ModelState, error: Option<String>) -> LocalModel {
    LocalModel {
        id: spec.id.clone(),
        display_name: spec.display_name.clone(),
        backend: spec.backend,
        state,
        size_bytes: spec.source.size_bytes,
        context_length: spec.context_length,
        meta: spec.meta.clone(),
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::store::tests::{blob, serve, spec, Quirks, TempDir};
    use super::*;
    use protocol::{ChatRole, ModelSpec};
    use std::time::Duration;

    /// An Inference over a temp dir that may download over plain HTTP and
    /// has a runtime that's already "installed" (a dummy file), so pulls
    /// only fetch the model.
    fn inference(dir: &TempDir) -> Inference {
        let store = store::Store::open_with(&dir.0, true, Arc::new(|_| None));
        let asset = runtime::pinned_for_this_platform().or(runtime::PINNED.first());
        let runtime = runtime::Runtime::with_asset(&dir.0, asset);
        if let Some(asset) = asset {
            let install = dir.0.join(format!("runtime/cpu-{}", runtime::LLAMA_CPP_VERSION));
            std::fs::create_dir_all(&install).unwrap();
            std::fs::write(install.join(runtime::SERVER_EXE), b"").unwrap();
            std::fs::write(install.join(".installed"), asset.sha256).unwrap();
        }
        Inference::from_parts(store, runtime)
    }

    async fn statuses_until_final(rx: &mut mpsc::Receiver<CommandStatus>) -> Vec<CommandStatus> {
        let mut got = Vec::new();
        loop {
            let s = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .expect("no status")
                .expect("channel closed");
            let last = s.state.is_final();
            got.push(s);
            if last {
                return got;
            }
        }
    }

    fn pull(model: ModelSpec) -> ModelCommandKind {
        ModelCommandKind::Pull { model }
    }

    #[tokio::test]
    async fn unknown_commands_are_unsupported() {
        let dir = TempDir::new("inf-unknown");
        let inf = inference(&dir);
        let (tx, mut rx) = mpsc::channel(8);
        inf.execute("c1".into(), ModelCommandKind::Unknown, tx);
        let got = statuses_until_final(&mut rx).await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].state, CommandState::Unsupported);
        assert_eq!(got[0].command_id, "c1");
        assert!(matches!(
            got[0].clone().into_message(),
            ClientMessage::ModelCommandStatus {
                state: CommandState::Unsupported,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn pull_then_delete_through_commands_updates_the_inventory() {
        let dir = TempDir::new("inf-pull");
        let inf = inference(&dir);
        let body = blob(300_000);
        let (url, _) = serve(body.clone(), Quirks::default());
        let mut version = inf.subscribe();
        let initial: Vec<String> = inf.inventory().models.into_iter().map(|m| m.id).collect();
        assert_eq!(initial, vec!["echo"]);

        let (tx, mut rx) = mpsc::channel(64);
        inf.execute("c1".into(), pull(spec("m1", &url, &body)), tx.clone());
        let got = statuses_until_final(&mut rx).await;
        assert_eq!(got.first().map(|s| s.state), Some(CommandState::Accepted));
        let last = got.last().unwrap();
        assert_eq!((last.state, last.error.as_deref()), (CommandState::Done, None));
        assert!(version.has_changed().unwrap());
        version.borrow_and_update();

        let inv = inf.inventory();
        let m1 = inv.models.iter().find(|m| m.id == "m1").unwrap();
        assert_eq!((m1.state, m1.size_bytes), (ModelState::Available, 300_000));
        assert!(inv.storage.models_bytes >= 300_000);
        assert_eq!(inv.runtimes[0].state, protocol::RuntimeState::Ready);

        // Idempotent: again is done at once.
        inf.execute("c2".into(), pull(spec("m1", &url, &body)), tx.clone());
        let again = statuses_until_final(&mut rx).await;
        assert_eq!(again.last().unwrap().state, CommandState::Done);

        inf.execute(
            "c3".into(),
            ModelCommandKind::Delete { model_id: "m1".into() },
            tx.clone(),
        );
        assert_eq!(
            statuses_until_final(&mut rx).await.last().unwrap().state,
            CommandState::Done
        );
        assert!(inf.inventory().models.iter().all(|m| m.id != "m1"));
        // Deleting a missing model is done, not an error.
        inf.execute(
            "c4".into(),
            ModelCommandKind::Delete { model_id: "m1".into() },
            tx.clone(),
        );
        assert_eq!(
            statuses_until_final(&mut rx).await.last().unwrap().state,
            CommandState::Done
        );
        inf.execute(
            "c5".into(),
            ModelCommandKind::Delete {
                model_id: "../x".into(),
            },
            tx,
        );
        assert_eq!(
            statuses_until_final(&mut rx).await.last().unwrap().state,
            CommandState::Failed
        );
    }

    #[tokio::test]
    async fn a_failed_pull_shows_in_the_inventory_until_pulled_again() {
        let dir = TempDir::new("inf-fail");
        let inf = inference(&dir);
        let body = blob(1_000);
        let (url, _) = serve(body.clone(), Quirks::default());
        let mut bad = spec("m1", &url, &body);
        bad.source.sha256 = "0".repeat(64);
        let (tx, mut rx) = mpsc::channel(64);
        inf.execute("c1".into(), pull(bad), tx.clone());
        let last = statuses_until_final(&mut rx).await.pop().unwrap();
        assert_eq!(last.state, CommandState::Failed);
        assert!(last.error.unwrap().contains("hash mismatch"));
        let m1 = inf.inventory().models.into_iter().find(|m| m.id == "m1").unwrap();
        assert_eq!(m1.state, ModelState::Failed);
        assert!(m1.error.unwrap().contains("hash mismatch"));

        inf.execute("c2".into(), pull(spec("m1", &url, &body)), tx);
        assert_eq!(
            statuses_until_final(&mut rx).await.pop().unwrap().state,
            CommandState::Done
        );
        let m1 = inf.inventory().models.into_iter().find(|m| m.id == "m1").unwrap();
        assert_eq!((m1.state, m1.error), (ModelState::Available, None));
    }

    #[tokio::test]
    async fn cancel_stops_a_pull_and_a_second_pull_joins_the_first() {
        let dir = TempDir::new("inf-cancel");
        let inf = inference(&dir);
        // Slow enough (about 0.6 s) not to finish before the cancel lands.
        let body = blob(2_000_000);
        let (url, requests) = serve(
            body.clone(),
            Quirks {
                slow: true,
                ..Quirks::default()
            },
        );
        let (tx1, mut rx1) = mpsc::channel(64);
        let (tx2, mut rx2) = mpsc::channel(64);
        inf.execute("c1".into(), pull(spec("big", &url, &body)), tx1);
        assert_eq!(rx1.recv().await.unwrap().state, CommandState::Accepted);
        inf.execute("c2".into(), pull(spec("big", &url, &body)), tx2.clone());
        assert_eq!(rx2.recv().await.unwrap().state, CommandState::Accepted);
        let downloading = inf.inventory().models.into_iter().find(|m| m.id == "big").unwrap();
        assert_eq!(downloading.state, ModelState::Downloading);

        inf.execute(
            "c3".into(),
            ModelCommandKind::Cancel {
                target_command_id: "c1".into(),
            },
            tx2,
        );
        let mut c3_done = false;
        let mut c2_final = None;
        while c2_final.is_none() || !c3_done {
            let s = tokio::time::timeout(Duration::from_secs(10), rx2.recv())
                .await
                .unwrap()
                .unwrap();
            match s.command_id.as_str() {
                "c3" => c3_done = s.state == CommandState::Done,
                "c2" if s.state.is_final() => c2_final = Some(s.state),
                _ => {}
            }
        }
        assert_eq!(c2_final, Some(CommandState::Cancelled), "both share one download");
        assert_eq!(
            statuses_until_final(&mut rx1).await.pop().unwrap().state,
            CommandState::Cancelled
        );
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one download, not two"
        );
        assert!(dir.0.join("models/big/model.gguf.partial").exists());
        assert!(inf.inventory().models.iter().all(|m| m.id != "big"));
    }

    #[tokio::test]
    async fn shutdown_stops_downloads_but_keeps_them_resumable() {
        let dir = TempDir::new("inf-shutdown");
        let inf = inference(&dir);
        let body = blob(2_000_000);
        let (url, _) = serve(
            body.clone(),
            Quirks {
                slow: true,
                ..Quirks::default()
            },
        );
        let (tx, mut rx) = mpsc::channel(64);
        inf.execute("c1".into(), pull(spec("m1", &url, &body)), tx);
        assert_eq!(rx.recv().await.unwrap().state, CommandState::Accepted);
        tokio::time::sleep(Duration::from_millis(100)).await;
        inf.shutdown().await;
        let last = statuses_until_final(&mut rx).await.pop().unwrap();
        assert_eq!(last.state, CommandState::Cancelled);
        assert!(dir.0.join("models/m1/model.gguf.partial").exists());
    }

    #[tokio::test]
    async fn chat_uses_echo_and_reports_unknown_models() {
        let dir = TempDir::new("inf-chat");
        let inf = inference(&dir);
        let (tx, mut rx) = mpsc::channel(64);
        let req = |model: &str| ChatCompletion {
            model_id: model.into(),
            messages: vec![ChatTurn {
                role: ChatRole::User,
                content: "hi there".into(),
            }],
            max_tokens: None,
            temperature: None,
        };
        inf.chat(req("echo"), tx.clone()).await.unwrap();
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        assert_eq!(
            events,
            vec![
                InferenceEvent::Status(Stage::Generating),
                InferenceEvent::Delta("hi ".into()),
                InferenceEvent::Delta("there".into())
            ]
        );
        assert_eq!(
            inf.chat(req("nope"), tx.clone()).await,
            Err(InferenceError::UnknownModel("nope".into()))
        );
        // Echo can be switched off by the server.
        inf.apply_settings(&InferenceSettings {
            echo_model_enabled: false,
            ..InferenceSettings::default()
        });
        assert!(inf.inventory().models.is_empty());
        assert!(inf.chat(req("echo"), tx).await.is_err());
    }
}
