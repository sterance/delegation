//! Model commands from the server: pull, delete, cancel, choosing the
//! runtime variant (`select_runtime`), per-model settings (`configure`),
//! and `unsupported` for anything newer than this build.
//!
//! Commands outlive connections. A pull keeps going if the session drops
//! (its statuses just have nowhere to go), and a pull of a model that's
//! already being pulled joins that download instead of starting another,
//! so after a reconnect the server can simply pull again and pick up the
//! progress. Every command is idempotent: pulling a model that's present
//! is `done` at once, and so is deleting one that's missing.

use super::store::{validate_spec, PullError, PullOutcome};
use super::{llama_cpp, CommandStatus, Shared};
use log::{info, warn};
use protocol::{
    valid_model_id, CommandState, LlamaCppVersion, ModelCommandKind, ModelSettings, ModelSpec, Progress, ProgressPhase,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

/// How long a delete waits for a pull of the same model to stop.
const CANCEL_WAIT: Duration = Duration::from_secs(30);

struct Subscriber {
    command_id: String,
    tx: mpsc::Sender<CommandStatus>,
}

struct Job {
    spec: ModelSpec,
    cancel: Arc<AtomicBool>,
    subscribers: Vec<Subscriber>,
    progress: Option<Progress>,
}

#[derive(Default)]
struct Inner {
    /// Pulls in progress, by model id.
    jobs: HashMap<String, Job>,
    /// command id -> model id, for cancelling.
    commands: HashMap<String, String>,
    /// The last failed pull of each model, until it's pulled or deleted
    /// again, so the dashboard can show why.
    failed: BTreeMap<String, (ModelSpec, String)>,
}

/// What the inventory needs to know about pulls.
pub struct PullsSnapshot {
    pub downloading: Vec<ModelSpec>,
    pub failed: Vec<(ModelSpec, String)>,
}

#[derive(Default)]
pub struct Pulls(Mutex<Inner>);

impl Pulls {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn snapshot(&self) -> PullsSnapshot {
        let inner = self.lock();
        PullsSnapshot {
            downloading: inner.jobs.values().map(|j| j.spec.clone()).collect(),
            failed: inner.failed.values().cloned().collect(),
        }
    }

    /// Stops the pull `command_id` belongs to. Returns whether there was one.
    pub fn cancel_command(&self, command_id: &str) -> bool {
        let inner = self.lock();
        let Some(job) = inner.commands.get(command_id).and_then(|m| inner.jobs.get(m)) else {
            return false;
        };
        job.cancel.store(true, Ordering::Relaxed);
        true
    }

    /// Stops every pull (clean shutdown). Their partial files stay for resuming.
    pub fn cancel_all(&self) {
        for job in self.lock().jobs.values() {
            job.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn cancel_model(&self, model_id: &str) -> bool {
        let inner = self.lock();
        let Some(job) = inner.jobs.get(model_id) else {
            return false;
        };
        job.cancel.store(true, Ordering::Relaxed);
        true
    }

    fn is_pulling(&self, model_id: &str) -> bool {
        self.lock().jobs.contains_key(model_id)
    }
}

fn status(command_id: &str, state: CommandState, progress: Option<Progress>, error: Option<String>) -> CommandStatus {
    CommandStatus {
        command_id: command_id.to_string(),
        state,
        progress,
        error,
    }
}

/// Runs one command on its own task; statuses go to `tx`.
pub(crate) fn execute(
    shared: &Arc<Shared>,
    command_id: String,
    command: ModelCommandKind,
    tx: mpsc::Sender<CommandStatus>,
) {
    let shared = shared.clone();
    tokio::spawn(async move {
        match command {
            ModelCommandKind::Pull { model } => pull(shared, command_id, model, tx).await,
            ModelCommandKind::Delete { model_id } => delete(shared, command_id, model_id, tx).await,
            ModelCommandKind::SelectRuntime { variant } => select_runtime(shared, command_id, variant, tx).await,
            ModelCommandKind::Configure { model_id, settings } => {
                configure(shared, command_id, model_id, settings, tx).await
            }
            ModelCommandKind::Cancel { target_command_id } => {
                if shared.pulls.cancel_command(&target_command_id) {
                    info!(target: "models", "cancelling {target_command_id}");
                }
                // Already finished (or never existed): nothing to stop, which
                // is what was asked for.
                let _ = tx.send(status(&command_id, CommandState::Done, None, None)).await;
            }
            ModelCommandKind::Unknown => {
                info!(target: "models", "command {command_id} is of a kind this client doesn't know");
                let error = "this client doesn't support that command; it may need updating".to_string();
                let _ = tx
                    .send(status(&command_id, CommandState::Unsupported, None, Some(error)))
                    .await;
            }
        }
    });
}

/// Whether this client's llama.cpp can run `spec`.
pub(crate) fn check_runtime_version(spec: &ModelSpec, have: &str) -> Result<(), String> {
    let Some(min) = &spec.min_runtime_version else {
        return Ok(());
    };
    let need = LlamaCppVersion::parse(min)
        .ok_or_else(|| format!("{min:?} isn't a llama.cpp version (min_runtime_version)"))?;
    match LlamaCppVersion::parse(have) {
        Some(have_v) if have_v >= need => Ok(()),
        _ => Err(format!(
            "this model needs llama.cpp {need} or newer, and this client has {have}; update the client to install it"
        )),
    }
}

async fn pull(shared: Arc<Shared>, command_id: String, spec: ModelSpec, tx: mpsc::Sender<CommandStatus>) {
    let checked = validate_spec(&spec, shared.store.allow_http())
        .and_then(|()| check_runtime_version(&spec, shared.runtime.version()));
    if let Err(why) = checked {
        warn!(target: "models", "refusing pull {command_id}: {why}");
        let _ = tx
            .send(status(&command_id, CommandState::Failed, None, Some(why)))
            .await;
        return;
    }
    let _ = tx.send(status(&command_id, CommandState::Accepted, None, None)).await;

    enum Start {
        New(Arc<AtomicBool>),
        Joined(Option<Progress>),
        Conflict,
    }
    let start = {
        let mut guard = shared.pulls.lock();
        let inner = &mut *guard;
        match inner.jobs.get_mut(&spec.id) {
            Some(job) if job.spec.source != spec.source => Start::Conflict,
            Some(job) => {
                job.subscribers.push(Subscriber {
                    command_id: command_id.clone(),
                    tx: tx.clone(),
                });
                inner.commands.insert(command_id.clone(), spec.id.clone());
                Start::Joined(job.progress)
            }
            None => {
                let cancel = Arc::new(AtomicBool::new(false));
                inner.failed.remove(&spec.id);
                inner.commands.insert(command_id.clone(), spec.id.clone());
                inner.jobs.insert(
                    spec.id.clone(),
                    Job {
                        spec: spec.clone(),
                        cancel: cancel.clone(),
                        subscribers: vec![Subscriber {
                            command_id: command_id.clone(),
                            tx: tx.clone(),
                        }],
                        progress: None,
                    },
                );
                Start::New(cancel)
            }
        }
    };
    match start {
        Start::Conflict => {
            let error = "a different version of this model is already being pulled".to_string();
            let _ = tx
                .send(status(&command_id, CommandState::Failed, None, Some(error)))
                .await;
        }
        Start::Joined(progress) => {
            info!(target: "models", "pull {command_id} joins the download of {} already running", spec.id);
            let _ = tx.try_send(status(&command_id, CommandState::InProgress, progress, None));
        }
        Start::New(cancel) => {
            shared.changed();
            run_pull(shared, spec, cancel).await;
        }
    }
}

/// Turns (done, total) callbacks into at most one report per interval,
/// plus the last.
struct Reporter {
    tx: watch::Sender<Option<Progress>>,
    interval: Duration,
    last: Option<Instant>,
}

impl Reporter {
    fn report(&mut self, phase: ProgressPhase, done: u64, total: u64) {
        let due = self.last.is_none_or(|t| t.elapsed() >= self.interval) || done == total;
        if due {
            self.last = Some(Instant::now());
            self.tx.send_replace(Some(Progress {
                done_bytes: done,
                total_bytes: total,
                phase,
            }));
        }
    }
}

async fn run_pull(shared: Arc<Shared>, spec: ModelSpec, cancel: Arc<AtomicBool>) {
    let settings = shared.settings();
    let (progress_tx, mut progress_rx) = watch::channel(None);
    let mut reporter = Reporter {
        tx: progress_tx,
        interval: Duration::from_millis(settings.progress_interval_ms),
        last: None,
    };
    let reserve = settings.disk_reserve_mb.saturating_mul(1024 * 1024);
    let work = {
        let (shared, spec, cancel) = (shared.clone(), spec.clone(), cancel.clone());
        tokio::task::spawn_blocking(move || -> Result<PullOutcome, PullError> {
            shared.runtime.resolve(
                shared.store.agent(),
                &cancel,
                reserve,
                &mut |done, total| reporter.report(ProgressPhase::Runtime, done, total),
                &|| shared.changed(),
            )?;
            shared.store.pull(&spec, reserve, &cancel, &mut |done, total| {
                reporter.report(ProgressPhase::Model, done, total)
            })
        })
    };
    tokio::pin!(work);
    let result = loop {
        tokio::select! {
            result = &mut work => break result,
            Ok(()) = progress_rx.changed() => {
                let progress = *progress_rx.borrow_and_update();
                let mut inner = shared.pulls.lock();
                if let Some(job) = inner.jobs.get_mut(&spec.id) {
                    job.progress = progress;
                    for s in &job.subscribers {
                        // Progress can be dropped if a session is slow; the
                        // final status can't.
                        let _ = s.tx.try_send(status(&s.command_id, CommandState::InProgress, progress, None));
                    }
                }
            }
        }
    };
    let (state, error) = match result {
        Ok(Ok(_)) => (CommandState::Done, None),
        Ok(Err(PullError::Cancelled)) => (CommandState::Cancelled, None),
        Ok(Err(PullError::Failed(why))) => (CommandState::Failed, Some(why)),
        Err(e) => (
            CommandState::Failed,
            Some(format!("the pull stopped unexpectedly: {e}")),
        ),
    };
    match &error {
        Some(why) => warn!(target: "models", "pull of {} failed: {why}", spec.id),
        None if state == CommandState::Cancelled => info!(target: "models", "pull of {} cancelled", spec.id),
        None => {}
    }
    let job = {
        let mut inner = shared.pulls.lock();
        let job = inner.jobs.remove(&spec.id);
        if let Some(job) = &job {
            for s in &job.subscribers {
                inner.commands.remove(&s.command_id);
            }
        }
        if let Some(why) = &error {
            inner.failed.insert(spec.id.clone(), (spec.clone(), why.clone()));
        }
        job
    };
    shared.changed();
    let last_progress = job.as_ref().and_then(|j| j.progress);
    for s in job.map(|j| j.subscribers).unwrap_or_default() {
        let _ =
            s.tx.send(status(&s.command_id, state, last_progress, error.clone()))
                .await;
    }
}

async fn delete(shared: Arc<Shared>, command_id: String, model_id: String, tx: mpsc::Sender<CommandStatus>) {
    if !valid_model_id(&model_id) {
        let error = format!("{model_id:?} isn't a valid model id");
        let _ = tx
            .send(status(&command_id, CommandState::Failed, None, Some(error)))
            .await;
        return;
    }
    let _ = tx.send(status(&command_id, CommandState::Accepted, None, None)).await;
    // A pull of it stops first, so nothing is writing into the directory.
    if shared.pulls.cancel_model(&model_id) {
        let deadline = Instant::now() + CANCEL_WAIT;
        while shared.pulls.is_pulling(&model_id) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    shared.pulls.lock().failed.remove(&model_id);
    llama_cpp::unload(&shared, &model_id).await;
    let result = {
        let (shared, id) = (shared.clone(), model_id.clone());
        tokio::task::spawn_blocking(move || shared.store.delete(&id)).await
    };
    shared.changed();
    let last = match result {
        Ok(Ok(_)) => status(&command_id, CommandState::Done, None, None),
        Ok(Err(e)) => status(
            &command_id,
            CommandState::Failed,
            None,
            Some(format!("couldn't delete it: {e}")),
        ),
        Err(e) => status(
            &command_id,
            CommandState::Failed,
            None,
            Some(format!("couldn't delete it: {e}")),
        ),
    };
    let _ = tx.send(last).await;
}

/// `select_runtime`: records the choice, unloads the model if the variant
/// changes, and installs and checks the new one (with progress), so the
/// next reply doesn't wait for it. If the chosen variant doesn't work here,
/// the command fails with why, and models keep loading with the fallback.
async fn select_runtime(shared: Arc<Shared>, command_id: String, variant: String, tx: mpsc::Sender<CommandStatus>) {
    let before = shared.runtime.selected();
    if let Err(why) = shared.runtime.select(&variant) {
        let _ = tx
            .send(status(&command_id, CommandState::Failed, None, Some(why)))
            .await;
        return;
    }
    info!(target: "models", "runtime selection is now {variant}");
    let _ = tx.send(status(&command_id, CommandState::Accepted, None, None)).await;
    shared.changed();

    let settings = shared.settings();
    let reserve = settings.disk_reserve_mb.saturating_mul(1024 * 1024);
    let (progress_tx, mut progress_rx) = watch::channel(None);
    let mut reporter = Reporter {
        tx: progress_tx,
        interval: Duration::from_millis(settings.progress_interval_ms),
        last: None,
    };
    let work = {
        let shared = shared.clone();
        tokio::task::spawn_blocking(move || {
            shared.runtime.resolve(
                shared.store.agent(),
                &AtomicBool::new(false),
                reserve,
                &mut |done, total| reporter.report(ProgressPhase::Runtime, done, total),
                &|| shared.changed(),
            )
        })
    };
    tokio::pin!(work);
    let result = loop {
        tokio::select! {
            result = &mut work => break result,
            Ok(()) = progress_rx.changed() => {
                let progress = *progress_rx.borrow_and_update();
                let _ = tx.try_send(status(&command_id, CommandState::InProgress, progress, None));
            }
        }
    };
    let resolved = match result {
        Ok(Ok(resolved)) => resolved,
        Ok(Err(e)) => {
            let _ = tx
                .send(status(&command_id, CommandState::Failed, None, Some(e.to_string())))
                .await;
            return;
        }
        Err(e) => {
            let _ = tx
                .send(status(&command_id, CommandState::Failed, None, Some(e.to_string())))
                .await;
            return;
        }
    };
    if before.as_deref() != Some(resolved.variant.as_str()) {
        info!(target: "models", "switched to llama.cpp {}; unloading the current model", resolved.variant);
        llama_cpp::stop(&shared).await;
    }
    shared.changed();
    let (state, error) = if variant == "auto" || resolved.variant == variant {
        (CommandState::Done, None)
    } else {
        let why = shared
            .runtime
            .info()
            .into_iter()
            .find(|r| r.variant == variant)
            .and_then(|r| r.error)
            .unwrap_or_else(|| "it didn't work here".into());
        (
            CommandState::Failed,
            Some(format!(
                "llama.cpp {variant} doesn't work on this client, so it uses {} instead: {why}",
                resolved.variant
            )),
        )
    };
    let _ = tx.send(status(&command_id, state, None, error)).await;
}

/// `configure`: stores the settings in the model's manifest and unloads the
/// model if it's loaded, so the next reply loads it with them.
async fn configure(
    shared: Arc<Shared>,
    command_id: String,
    model_id: String,
    settings: ModelSettings,
    tx: mpsc::Sender<CommandStatus>,
) {
    let fail = |why: String| status(&command_id, CommandState::Failed, None, Some(why));
    if !valid_model_id(&model_id) {
        let _ = tx.send(fail(format!("{model_id:?} isn't a valid model id"))).await;
        return;
    }
    if let Some(why) = settings.problem() {
        let _ = tx.send(fail(why)).await;
        return;
    }
    let result = {
        let (shared, id) = (shared.clone(), model_id.clone());
        tokio::task::spawn_blocking(move || shared.store.configure(&id, settings)).await
    };
    match result {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => {
            let _ = tx
                .send(fail(format!("this client doesn't have the model {model_id:?}")))
                .await;
            return;
        }
        Ok(Err(e)) => {
            let _ = tx.send(fail(format!("couldn't save the settings: {e}"))).await;
            return;
        }
        Err(e) => {
            let _ = tx.send(fail(format!("couldn't save the settings: {e}"))).await;
            return;
        }
    }
    info!(target: "models", "{model_id} settings: {settings:?}");
    llama_cpp::unload(&shared, &model_id).await;
    shared.changed();
    let _ = tx.send(status(&command_id, CommandState::Done, None, None)).await;
}
