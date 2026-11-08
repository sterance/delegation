//! Models on clients: what a client has (`model_inventory`), the commands
//! the server sends to change that (`model_command`), and how they went
//! (`model_command_status`).
//!
//! The server decides which models a client should have; the client only
//! executes commands and reports. The command set is meant to grow (load
//! and unload with per-model settings, choosing among pinned runtime
//! variants), so a kind this build doesn't know parses as
//! [`ModelCommandKind::Unknown`] and is answered `unsupported` rather than
//! breaking the connection.
//!
//! Model ids come from the server and name directories on the client, so
//! both sides check them with [`valid_model_id`].

use serde::{Deserialize, Serialize};

/// The longest model id either side accepts.
pub const MODEL_ID_MAX_LEN: usize = 100;

/// Lowercase ASCII letters, digits, `.`, `_` and `-`, 1 to
/// [`MODEL_ID_MAX_LEN`] characters, and not made only of dots (so never
/// `.` or `..`). The client builds a directory name from it and from
/// nothing else the server sends.
pub fn valid_model_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MODEL_ID_MAX_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
        && !id.bytes().all(|b| b == b'.')
}

/// What runs a model.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// A GGUF file run by a managed `llama-server`.
    LlamaCpp,
    /// The built-in test model: replies with the user's last message.
    Echo,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelState {
    /// Being pulled.
    Downloading,
    /// On disk and verified; loads on the first request.
    Available,
    /// Being loaded into memory.
    Loading,
    /// In memory, ready to answer.
    Loaded,
    /// The last pull failed; `LocalModel::error` says why.
    Failed,
}

/// Descriptive details the suggestion engine and the dashboard use. All
/// optional: a model without them still works.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ModelMeta {
    /// e.g. "qwen2.5".
    pub family: Option<String>,
    /// e.g. "0.5B".
    pub parameters: Option<String>,
    /// e.g. "Q4_K_M".
    pub quantization: Option<String>,
}

/// One model a client has (or is getting).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct LocalModel {
    pub id: String,
    pub display_name: String,
    pub backend: Backend,
    pub state: ModelState,
    pub size_bytes: u64,
    /// The context it runs with, in tokens. None: the client's default.
    #[serde(default)]
    pub context_length: Option<u32>,
    #[serde(default)]
    pub meta: ModelMeta,
    /// Why it's `failed`.
    #[serde(default)]
    pub error: Option<String>,
}

/// The data volume models live on.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Storage {
    /// Everything under the models directory, partial downloads included.
    pub models_bytes: u64,
    /// Free space on the volume. 0 if it couldn't be read.
    pub free_bytes: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeState {
    /// Not downloaded yet; it will be on the first pull or load.
    Absent,
    Downloading,
    Ready,
}

/// A runtime (inference engine build) this client can use. Only pinned
/// builds exist: executables never come from the server.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RuntimeInfo {
    pub backend: Backend,
    /// The pinned release, e.g. "b12110".
    pub version: String,
    /// e.g. "cpu". GPU variants come later.
    pub variant: String,
    pub state: RuntimeState,
}

/// Everything about models on one client: `model_inventory`. Sent after
/// `welcome`, and again whenever it changes (debounced). Replaces the
/// previous one.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct Inventory {
    pub models: Vec<LocalModel>,
    pub storage: Storage,
    pub runtimes: Vec<RuntimeInfo>,
}

/// Where a model file comes from, and what it must be.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ModelSource {
    /// HTTPS only.
    pub url: String,
    /// Lowercase hex SHA-256 of the whole file.
    pub sha256: String,
    pub size_bytes: u64,
}

/// A model the server wants on a client.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    pub id: String,
    pub display_name: String,
    pub backend: Backend,
    pub source: ModelSource,
    #[serde(default)]
    pub context_length: Option<u32>,
    #[serde(default)]
    pub meta: ModelMeta,
}

/// What a `model_command` asks for.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelCommandKind {
    /// Download and verify a model (and the runtime it needs, if that isn't
    /// there yet). Already present and verified: done at once.
    Pull { model: ModelSpec },
    /// Unload it if it's loaded, then remove it. Missing: done at once.
    Delete { model_id: String },
    /// Stop another command (a pull). A cancelled pull keeps its partial
    /// file, so pulling again resumes.
    Cancel { target_command_id: String },
    /// Any kind this client doesn't know (a newer server). Answered
    /// `unsupported`.
    #[serde(other)]
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandState {
    Accepted,
    InProgress,
    Done,
    Failed,
    Cancelled,
    /// The client doesn't know this kind of command.
    Unsupported,
}

impl CommandState {
    /// No more statuses follow.
    pub fn is_final(self) -> bool {
        !matches!(self, CommandState::Accepted | CommandState::InProgress)
    }
}

/// Which download a pull's progress is about: the runtime it needed first,
/// or the model itself.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProgressPhase {
    Runtime,
    Model,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub phase: ProgressPhase,
}

/// Where a reply is before its first text: `chat_progress`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// The model is being loaded into memory (can take a while).
    LoadingModel,
    /// The model is processing the prompt or producing text.
    Generating,
}

/// The inference values from the server's `config.env`, pushed in
/// `welcome` as part of `ClientSettings`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct InferenceSettings {
    /// Pull progress is reported at most this often.
    pub progress_interval_ms: u64,
    /// Stop `llama-server` after this long with no requests. 0 never does.
    pub idle_unload_secs: u64,
    /// How long a model may take to load.
    pub model_load_timeout_ms: u64,
    /// Requests generated at once; more fail with "client busy".
    pub max_concurrent: u32,
    /// The longest reply, in tokens.
    pub max_tokens: u32,
    /// Context size for a model that doesn't specify one.
    pub default_context: u32,
    /// CPU threads for llama.cpp; 0 lets it decide.
    pub threads: u32,
    /// Free space a pull must leave on the data volume.
    pub disk_reserve_mb: u64,
    /// Offer the built-in `echo` test model.
    pub echo_model_enabled: bool,
}

/// Progress reports can't come faster than this.
pub const MIN_PROGRESS_INTERVAL_MS: u64 = 100;
/// A model gets at least this long to load.
pub const MIN_MODEL_LOAD_TIMEOUT_MS: u64 = 1_000;
/// The smallest context the client will run a model with.
pub const MIN_CONTEXT: u32 = 256;

impl Default for InferenceSettings {
    fn default() -> Self {
        InferenceSettings {
            progress_interval_ms: 1_000,
            idle_unload_secs: 600,
            model_load_timeout_ms: 120_000,
            max_concurrent: 1,
            max_tokens: 2_048,
            default_context: 4_096,
            threads: 0,
            disk_reserve_mb: 2_048,
            echo_model_enabled: true,
        }
    }
}

impl InferenceSettings {
    /// The server validates these, but the client never trusts them blindly.
    pub fn sanitized(self) -> Self {
        InferenceSettings {
            progress_interval_ms: self.progress_interval_ms.max(MIN_PROGRESS_INTERVAL_MS),
            model_load_timeout_ms: self.model_load_timeout_ms.max(MIN_MODEL_LOAD_TIMEOUT_MS),
            max_concurrent: self.max_concurrent.clamp(1, 64),
            max_tokens: self.max_tokens.max(1),
            default_context: self.default_context.max(MIN_CONTEXT),
            threads: self.threads.min(1_024),
            ..self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ids_are_checked() {
        for ok in ["echo", "qwen2.5-0.5b-instruct-q4_k_m", "a", "x.y_z-1", &"a".repeat(100)] {
            assert!(valid_model_id(ok), "{ok:?} should be valid");
        }
        for bad in [
            "",
            ".",
            "..",
            "...",
            "../etc",
            "a/b",
            "a\\b",
            "Qwen",
            "white space",
            "nul\0",
            "ünï",
            &"a".repeat(101),
        ] {
            assert!(!valid_model_id(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn sanitized_inference_settings_are_usable() {
        let s = InferenceSettings {
            progress_interval_ms: 0,
            model_load_timeout_ms: 0,
            max_concurrent: 0,
            max_tokens: 0,
            default_context: 1,
            threads: 100_000,
            ..InferenceSettings::default()
        }
        .sanitized();
        assert_eq!(s.progress_interval_ms, MIN_PROGRESS_INTERVAL_MS);
        assert_eq!(s.model_load_timeout_ms, MIN_MODEL_LOAD_TIMEOUT_MS);
        assert_eq!((s.max_concurrent, s.max_tokens), (1, 1));
        assert_eq!(s.default_context, MIN_CONTEXT);
        assert_eq!(s.threads, 1_024);
        assert_eq!(InferenceSettings::default().sanitized(), InferenceSettings::default());
    }
}
