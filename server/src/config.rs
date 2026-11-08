//! `config.env`: loading, parsing and validating every server setting.
//!
//! Lookup order for every key: real environment variable, then the config
//! file, then the compiled default. Docker Compose passes `config.env` in as
//! environment variables (`env_file:`), so inside the container the file
//! lookup simply finds nothing and the env wins; natively, the file is read
//! directly. Deliberately never calls `std::env::set_var`: it's racy once
//! the tokio runtime has spawned threads, and a map lookup does the same job.
//!
//! Everything is parsed once at startup. Any invalid value, including a
//! broken cross-variable rule, stops the server with a message naming the
//! variable, rather than quietly running on something else.

use protocol::models::{MIN_CONTEXT, MIN_MODEL_LOAD_TIMEOUT_MS, MIN_PROGRESS_INTERVAL_MS};
use protocol::telemetry::MIN_SAMPLER_INTERVAL_MS;
use protocol::{valid_model_id, Backend, ClientSettings, InferenceSettings, ModelMeta, ModelSource, ModelSpec};
use serde::Serialize;
use std::{collections::HashMap, path::PathBuf, time::Duration};

pub struct ConfigFile {
    file: HashMap<String, String>,
    source: Option<PathBuf>,
}

impl ConfigFile {
    /// Reads the first existing file among `$DELEGATION_CONFIG` and then
    /// `candidates`. A missing file is fine (everything falls back to
    /// defaults); an explicitly named `$DELEGATION_CONFIG` that can't be
    /// read is reported, since that's almost certainly a typo.
    pub fn load(candidates: &[PathBuf]) -> Self {
        let explicit = std::env::var_os("DELEGATION_CONFIG").map(PathBuf::from);
        if let Some(path) = &explicit {
            if !path.is_file() {
                eprintln!(
                    "[{}] [config] DELEGATION_CONFIG={} not found, ignoring",
                    protocol::ts(),
                    path.display()
                );
            }
        }
        for path in explicit.iter().chain(candidates) {
            if let Ok(text) = std::fs::read_to_string(path) {
                return ConfigFile {
                    file: parse_dotenv(&text),
                    source: Some(path.clone()),
                };
            }
        }
        ConfigFile {
            file: HashMap::new(),
            source: None,
        }
    }

    /// The file values were read from, if any, for the startup log.
    pub fn source(&self) -> Option<&PathBuf> {
        self.source.as_ref()
    }

    /// A blank value counts as unset, so `KEY=` falls back to the default.
    pub fn get(&self, key: &str) -> Option<String> {
        std::env::var(key)
            .ok()
            .or_else(|| self.file.get(key).cloned())
            .filter(|v| !v.trim().is_empty())
    }
}

/// Minimal dotenv: `KEY=value` lines, `#` comments, optional `export `
/// prefix and surrounding quotes. No interpolation or multi-line values.
fn parse_dotenv(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (key, value) = line.split_once('=')?;
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value);
            Some((key.trim().to_string(), value.to_string()))
        })
        .collect()
}

/// Every setting, validated.
#[derive(Debug, Clone)]
pub struct Settings {
    pub port: u16,
    /// `SERVER_LOG_LEVEL=debug`: per-tick and per-sample lines.
    pub debug_logging: bool,
    pub auth_timeout: Duration,
    pub db_path: String,
    /// Pre-migration backups to keep next to the database.
    pub db_migration_backups_keep: usize,
    pub frontend_dir: String,
    /// Pushed to every client in `welcome`, `INFERENCE_*` included.
    pub client: ClientSettings,
    /// `TEST_MODEL_*`: the catalog's one entry.
    pub test_model: ModelSpec,
    pub liveness_timeout: Duration,
    pub ws_ping_interval: Duration,
    pub last_seen_flush_interval: Duration,
    pub metrics: MetricsLimits,
    pub pairing_code_ttl_secs: u64,
    pub chat: ChatSettings,
    pub dashboard: DashboardConfig,
    pub http: HttpSettings,
    pub auth: AuthSettings,
}

/// How the server sits on the internet: origins, cookies, client IPs.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpSettings {
    /// Normalised (`scheme://host[:port]`, lowercase, no default port).
    pub public_origin: String,
    /// Also normalised. Accepted for `Origin` checks alongside `public_origin`.
    pub extra_allowed_origins: Vec<String>,
    pub cookie_secure: bool,
    pub trust_cf_connecting_ip: bool,
}

impl HttpSettings {
    /// Whether a request's `Origin` header value is one this site accepts.
    pub fn origin_allowed(&self, origin: &str) -> bool {
        normalize_origin(origin).is_some_and(|o| o == self.public_origin || self.extra_allowed_origins.contains(&o))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Argon2Settings {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuthSettings {
    pub session_idle_timeout: Duration,
    /// Lifetime of a "Remember me" session.
    pub session_absolute_timeout: Duration,
    /// `SESSION_ABSOLUTE_TIMEOUT_DAYS`, for the login page's checkbox.
    pub remember_days: u64,
    /// Lifetime of a session without "Remember me".
    pub session_short_timeout: Duration,
    pub session_touch_interval: Duration,
    pub password_min_len: usize,
    pub password_max_len: usize,
    pub argon2: Argon2Settings,
    pub login_max_failures: usize,
    pub login_max_failures_per_ip: usize,
    pub login_window: Duration,
    pub enroll_max_attempts_per_ip: usize,
    pub signup_enabled: bool,
    pub signup_max_per_ip: usize,
    pub signup_window: Duration,
    pub password_reset_ttl: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChatSettings {
    /// The longest a reply may go without a chunk (or its first one) before it's failed.
    pub idle_timeout: Duration,
    /// How often a streaming reply's text is written to the database.
    pub flush_interval: Duration,
    /// 0 keeps conversations forever.
    pub retention_days: u64,
    pub max_message_chars: usize,
    pub max_reply_chars: usize,
    pub max_active_replies_per_user: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetricsLimits {
    /// Ring buffer length per client, and the graph width in samples.
    pub history_samples: usize,
    pub min_spacing: Duration,
    pub max_gpus: usize,
    pub max_string_len: usize,
}

/// What `GET /api/dashboard-config` returns. Field names are the
/// variable names, lowercased, minus the `DASHBOARD_` prefix; mirrored by
/// hand in `frontend/src/api.ts`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DashboardConfig {
    pub poll_interval_ms: u64,
    pub pause_when_hidden: bool,
    pub pairing_poll_interval_ms: u64,
    pub graph_width_px: u64,
    pub graph_height_px: u64,
    pub graph_grid_px: u64,
    pub graph_line_color: String,
    pub graph_grid_color: String,
    pub graph_bg_color: String,
    pub collapsed_max_gpus: u64,
    pub blink_period_ms: u64,
    pub metrics_history_samples: u64,
    pub tick_interval_ms: u64,
    pub pairing_code_ttl_secs: u64,
}

/// Keys that earlier versions read. They're ignored now; say so, rather
/// than let an old `config.env` look like it's still in effect.
const REPLACED_KEYS: &[(&str, &str)] = &[
    ("SERVER_HEARTBEAT_TIMEOUT_MS", "LIVENESS_TIMEOUT_MS"),
    ("SERVER_HEARTBEAT_SWEEP_INTERVAL_MS", "LIVENESS_TIMEOUT_MS"),
    ("SERVER_PAIRING_CODE_TTL_MS", "PAIRING_CODE_TTL_SECS"),
    ("CLIENT_ACK_TIMEOUT_MS", "SERVER_SILENCE_TIMEOUT_MS"),
    ("CLIENT_HEARTBEAT_INTERVAL_MS", "TICK_INTERVAL_MS"),
    ("CLIENT_RECONNECT_INITIAL_BACKOFF_MS", "RECONNECT_BACKOFF_INITIAL_MS"),
    ("CLIENT_RECONNECT_MAX_BACKOFF_MS", "RECONNECT_BACKOFF_MAX_MS"),
];

/// One line per old key that's still set, for a startup warning.
pub fn replaced_keys_in_use(lookup: impl Fn(&str) -> Option<String>) -> Vec<String> {
    REPLACED_KEYS
        .iter()
        .filter(|(old, _)| lookup(old).is_some())
        .map(|(old, new)| format!("{old} is no longer used (see {new}); remove it from config.env"))
        .collect()
}

impl Settings {
    /// Parses and validates everything, returning every problem found (not
    /// just the first), each naming its variable.
    pub fn parse(lookup: impl Fn(&str) -> Option<String>) -> Result<Settings, Vec<String>> {
        let mut p = Parser {
            lookup: &lookup,
            errors: Vec::new(),
        };
        let defaults = ClientSettings::default();

        let port = p.int("SERVER_PORT", 7070, 1);
        if port > u64::from(u16::MAX) {
            p.errors
                .push(format!("SERVER_PORT must be at most {}, got {port}", u16::MAX));
        }
        let debug_logging = match p.string("SERVER_LOG_LEVEL", "info").to_ascii_lowercase().as_str() {
            "info" => false,
            "debug" => true,
            other => {
                p.errors
                    .push(format!("SERVER_LOG_LEVEL must be \"info\" or \"debug\", got {other:?}"));
                false
            }
        };
        let auth_timeout_ms = p.int("SERVER_AUTH_TIMEOUT_MS", 5_000, 1);
        let db_path = p.string("DB_PATH", "data/delegation.db");
        let db_migration_backups_keep = p.int("DB_MIGRATION_BACKUPS_KEEP", 3, 1);
        let frontend_dir = p.string("FRONTEND_DIR", "frontend/dist");

        // ---- tick and liveness ----
        let tick = p.int("TICK_INTERVAL_MS", defaults.tick_interval_ms, 1);
        let sampler = p.int(
            "SAMPLER_INTERVAL_MS",
            defaults.sampler_interval_ms,
            MIN_SAMPLER_INTERVAL_MS,
        );
        let max_age = p.int("SAMPLE_MAX_AGE_MS", defaults.sample_max_age_ms, 1);
        let liveness = p.int("LIVENESS_TIMEOUT_MS", 4_000, 1);
        let ping = p.int("WS_PING_INTERVAL_MS", 2_000, 1);
        let silence = p.int("SERVER_SILENCE_TIMEOUT_MS", defaults.server_silence_timeout_ms, 1);
        let backoff_initial = p.int("RECONNECT_BACKOFF_INITIAL_MS", defaults.reconnect_backoff_initial_ms, 1);
        let backoff_max = p.int("RECONNECT_BACKOFF_MAX_MS", defaults.reconnect_backoff_max_ms, 1);
        let flush = p.int("LAST_SEEN_FLUSH_INTERVAL_MS", 30_000, 1);

        // ---- metrics ----
        // At least 2: the graph spaces samples width / (capacity - 1) apart.
        let history = p.int("METRICS_HISTORY_SAMPLES", 60, 2);
        let min_spacing = p.int("METRICS_MIN_SPACING_MS", 500, 0);
        let max_gpus = p.int("METRICS_MAX_GPUS", 16, 1);
        let max_string_len = p.int("METRICS_MAX_STRING_LEN", 128, 1);

        // ---- pairing ----
        let pairing_ttl = p.int("PAIRING_CODE_TTL_SECS", 600, 1);
        let pairing_poll = p.int("DASHBOARD_PAIRING_POLL_INTERVAL_MS", 1_500, 1);

        // ---- chat ----
        // Generous: a model's first token can take a while if it has to load.
        let chat_idle = p.int("CHAT_IDLE_TIMEOUT_MS", 120_000, 1);
        let chat_flush = p.int("CHAT_FLUSH_INTERVAL_MS", 3_000, 1);
        let chat_retention_days = p.int("CHAT_RETENTION_DAYS", 0, 0);
        let chat_max_message = p.int("CHAT_MAX_MESSAGE_CHARS", 100_000, 1);
        let chat_max_reply = p.int("CHAT_MAX_REPLY_CHARS", 400_000, 1);
        let chat_max_active = p.int("CHAT_MAX_ACTIVE_REPLIES_PER_USER", 3, 1);

        // ---- inference (pushed to clients) ----
        let inference_defaults = InferenceSettings::default();
        let inf_progress = p.int(
            "INFERENCE_PROGRESS_INTERVAL_MS",
            inference_defaults.progress_interval_ms,
            MIN_PROGRESS_INTERVAL_MS,
        );
        let inf_idle = p.int("INFERENCE_IDLE_UNLOAD_SECS", inference_defaults.idle_unload_secs, 0);
        let inf_load = p.int(
            "INFERENCE_MODEL_LOAD_TIMEOUT_MS",
            inference_defaults.model_load_timeout_ms,
            MIN_MODEL_LOAD_TIMEOUT_MS,
        );
        let inf_concurrent = p.int_in("INFERENCE_MAX_CONCURRENT", inference_defaults.max_concurrent, 1, 64);
        let inf_max_tokens = p.int_in("INFERENCE_MAX_TOKENS", inference_defaults.max_tokens, 1, 1 << 20);
        let inf_context = p.int_in(
            "INFERENCE_DEFAULT_CONTEXT",
            inference_defaults.default_context,
            MIN_CONTEXT,
            1 << 22,
        );
        let inf_threads = p.int_in("INFERENCE_THREADS", inference_defaults.threads, 0, 1_024);
        let inf_reserve = p.int("INFERENCE_DISK_RESERVE_MB", inference_defaults.disk_reserve_mb, 0);
        let inf_echo = p.bool("INFERENCE_ECHO_MODEL", inference_defaults.echo_model_enabled);

        // ---- the test model (the catalog's one entry) ----
        let test_id = p.string("TEST_MODEL_ID", DEFAULT_TEST_MODEL_ID);
        if !valid_model_id(&test_id) {
            p.errors.push(format!(
                "TEST_MODEL_ID must be lowercase letters, digits, '.', '_' and '-', at most {} characters, got {test_id:?}",
                protocol::models::MODEL_ID_MAX_LEN
            ));
        }
        let test_name = p.string("TEST_MODEL_NAME", DEFAULT_TEST_MODEL_NAME);
        if test_name.chars().count() > 200 {
            p.errors.push("TEST_MODEL_NAME must be at most 200 characters".into());
        }
        let test_url = p.string("TEST_MODEL_URL", DEFAULT_TEST_MODEL_URL);
        if !test_url.starts_with("https://")
            || test_url.len() <= "https://".len()
            || test_url.contains(char::is_whitespace)
        {
            p.errors
                .push(format!("TEST_MODEL_URL must be an https:// URL, got {test_url:?}"));
        }
        let test_sha = p
            .string("TEST_MODEL_SHA256", DEFAULT_TEST_MODEL_SHA256)
            .to_ascii_lowercase();
        if test_sha.len() != 64 || !test_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            p.errors
                .push(format!("TEST_MODEL_SHA256 must be 64 hex digits, got {test_sha:?}"));
        }
        let test_size = p.int("TEST_MODEL_SIZE_BYTES", DEFAULT_TEST_MODEL_SIZE_BYTES, 1);
        let test_context = p.int_in("TEST_MODEL_CONTEXT", 4_096, MIN_CONTEXT, 1 << 22);

        // ---- http: origins, cookies, client IPs ----
        let public_origin = p.origin("PUBLIC_ORIGIN", "https://llm.smith-c.com");
        let extra_origins = p.origin_list("EXTRA_ALLOWED_ORIGINS");
        let cookie_secure = p.bool("COOKIE_SECURE", true);
        let trust_cf_ip = p.bool("TRUST_CF_CONNECTING_IP", true);

        // ---- auth ----
        let idle_hours = p.int("SESSION_IDLE_TIMEOUT_HOURS", 168, 1);
        let absolute_days = p.int("SESSION_ABSOLUTE_TIMEOUT_DAYS", 30, 1);
        let short_hours = p.int("SESSION_SHORT_TIMEOUT_HOURS", 12, 1);
        let touch_secs = p.int("SESSION_TOUCH_INTERVAL_SECS", 300, 1);
        let password_min = p.int("AUTH_PASSWORD_MIN_LEN", 12, 1);
        let password_max = p.int("AUTH_PASSWORD_MAX_LEN", 256, 1);
        let argon2_memory = p.int("AUTH_ARGON2_MEMORY_KIB", 19_456, 8);
        let argon2_iterations = p.int("AUTH_ARGON2_ITERATIONS", 2, 1);
        let argon2_parallelism = p.int("AUTH_ARGON2_PARALLELISM", 1, 1);
        let login_max = p.int("AUTH_LOGIN_MAX_FAILURES", 5, 1);
        let login_max_ip = p.int("AUTH_LOGIN_MAX_FAILURES_PER_IP", 20, 1);
        let login_window = p.int("AUTH_LOGIN_WINDOW_SECS", 900, 1);
        let enroll_max_ip = p.int("ENROLL_MAX_ATTEMPTS_PER_IP", 10, 1);
        let signup_enabled = p.bool("AUTH_SIGNUP_ENABLED", true);
        let signup_max_ip = p.int("AUTH_SIGNUP_MAX_PER_IP", 5, 1);
        let signup_window = p.int("AUTH_SIGNUP_WINDOW_SECS", 3_600, 1);
        let reset_ttl_hours = p.int("PASSWORD_RESET_TTL_HOURS", 24, 1);

        // ---- dashboard ----
        let poll = p.int("DASHBOARD_POLL_INTERVAL_MS", 1_000, 1);
        let pause_when_hidden = p.bool("DASHBOARD_PAUSE_WHEN_HIDDEN", true);
        let graph_width = p.int("DASHBOARD_GRAPH_WIDTH_PX", 120, 1);
        let graph_height = p.int("DASHBOARD_GRAPH_HEIGHT_PX", 40, 1);
        let graph_grid = p.int("DASHBOARD_GRAPH_GRID_PX", 10, 1);
        let line_color = p.color("DASHBOARD_GRAPH_LINE_COLOR", "#00ff00");
        let grid_color = p.color("DASHBOARD_GRAPH_GRID_COLOR", "#008040");
        let bg_color = p.color("DASHBOARD_GRAPH_BG_COLOR", "#000000");
        let collapsed_max_gpus = p.int("DASHBOARD_COLLAPSED_MAX_GPUS", 1, 0);
        let blink_period = p.int("DASHBOARD_BLINK_PERIOD_MS", 1_000, 1);

        // ---- cross-variable rules ----
        p.at_least_twice("SAMPLE_MAX_AGE_MS", max_age, "SAMPLER_INTERVAL_MS", sampler);
        p.at_least_twice("LIVENESS_TIMEOUT_MS", liveness, "TICK_INTERVAL_MS", tick);
        p.at_least_twice("SERVER_SILENCE_TIMEOUT_MS", silence, "WS_PING_INTERVAL_MS", ping);
        if backoff_max < backoff_initial {
            p.errors.push(format!(
                "RECONNECT_BACKOFF_MAX_MS ({backoff_max}) must be at least RECONNECT_BACKOFF_INITIAL_MS ({backoff_initial})"
            ));
        }
        if min_spacing >= tick {
            p.errors.push(format!(
                "METRICS_MIN_SPACING_MS ({min_spacing}) must be less than TICK_INTERVAL_MS ({tick}), \
                 or every sample would be dropped"
            ));
        }
        for (key, context) in [
            ("INFERENCE_DEFAULT_CONTEXT", inf_context),
            ("TEST_MODEL_CONTEXT", test_context),
        ] {
            if inf_max_tokens >= context {
                p.errors.push(format!(
                    "INFERENCE_MAX_TOKENS ({inf_max_tokens}) must be less than {key} ({context}), \
                     or no prompt would fit next to the reply"
                ));
            }
        }
        if password_max < password_min {
            p.errors.push(format!(
                "AUTH_PASSWORD_MAX_LEN ({password_max}) must be at least AUTH_PASSWORD_MIN_LEN ({password_min})"
            ));
        }
        // Argon2's own limits: lanes up to 2^24 - 1, and at least 8 KiB of memory per lane.
        let argon2 = Argon2Settings {
            memory_kib: u32::try_from(argon2_memory).unwrap_or(u32::MAX),
            iterations: u32::try_from(argon2_iterations).unwrap_or(u32::MAX),
            parallelism: u32::try_from(argon2_parallelism).unwrap_or(u32::MAX),
        };
        if argon2_parallelism > 0xff_ffff {
            p.errors.push(format!(
                "AUTH_ARGON2_PARALLELISM must be at most 16777215, got {argon2_parallelism}"
            ));
        } else if argon2_memory < 8 * argon2_parallelism {
            p.errors.push(format!(
                "AUTH_ARGON2_MEMORY_KIB ({argon2_memory}) must be at least 8 x AUTH_ARGON2_PARALLELISM ({argon2_parallelism}), \
                 i.e. at least {}",
                8 * argon2_parallelism
            ));
        } else if argon2::Params::new(argon2.memory_kib, argon2.iterations, argon2.parallelism, None).is_err() {
            p.errors.push(format!(
                "AUTH_ARGON2_MEMORY_KIB ({argon2_memory}), AUTH_ARGON2_ITERATIONS ({argon2_iterations}) and \
                 AUTH_ARGON2_PARALLELISM ({argon2_parallelism}) aren't valid Argon2 parameters"
            ));
        }

        if !p.errors.is_empty() {
            return Err(p.errors);
        }
        Ok(Settings {
            port: port as u16,
            debug_logging,
            auth_timeout: Duration::from_millis(auth_timeout_ms),
            db_path,
            db_migration_backups_keep: db_migration_backups_keep as usize,
            frontend_dir,
            client: ClientSettings {
                tick_interval_ms: tick,
                sampler_interval_ms: sampler,
                sample_max_age_ms: max_age,
                server_silence_timeout_ms: silence,
                reconnect_backoff_initial_ms: backoff_initial,
                reconnect_backoff_max_ms: backoff_max,
                inference: InferenceSettings {
                    progress_interval_ms: inf_progress,
                    idle_unload_secs: inf_idle,
                    model_load_timeout_ms: inf_load,
                    max_concurrent: inf_concurrent,
                    max_tokens: inf_max_tokens,
                    default_context: inf_context,
                    threads: inf_threads,
                    disk_reserve_mb: inf_reserve,
                    echo_model_enabled: inf_echo,
                },
            },
            test_model: ModelSpec {
                id: test_id,
                display_name: test_name,
                backend: Backend::LlamaCpp,
                source: ModelSource {
                    url: test_url,
                    sha256: test_sha,
                    size_bytes: test_size,
                },
                context_length: Some(test_context),
                meta: ModelMeta::default(),
            },
            liveness_timeout: Duration::from_millis(liveness),
            ws_ping_interval: Duration::from_millis(ping),
            last_seen_flush_interval: Duration::from_millis(flush),
            metrics: MetricsLimits {
                history_samples: history as usize,
                min_spacing: Duration::from_millis(min_spacing),
                max_gpus: max_gpus as usize,
                max_string_len: max_string_len as usize,
            },
            pairing_code_ttl_secs: pairing_ttl,
            chat: ChatSettings {
                idle_timeout: Duration::from_millis(chat_idle),
                flush_interval: Duration::from_millis(chat_flush),
                retention_days: chat_retention_days,
                max_message_chars: chat_max_message as usize,
                max_reply_chars: chat_max_reply as usize,
                max_active_replies_per_user: chat_max_active as usize,
            },
            http: HttpSettings {
                public_origin,
                extra_allowed_origins: extra_origins,
                cookie_secure,
                trust_cf_connecting_ip: trust_cf_ip,
            },
            auth: AuthSettings {
                session_idle_timeout: hours(idle_hours),
                session_absolute_timeout: hours(absolute_days.saturating_mul(24)),
                remember_days: absolute_days,
                session_short_timeout: hours(short_hours),
                session_touch_interval: Duration::from_secs(touch_secs),
                password_min_len: password_min as usize,
                password_max_len: password_max as usize,
                argon2,
                login_max_failures: login_max as usize,
                login_max_failures_per_ip: login_max_ip as usize,
                login_window: Duration::from_secs(login_window),
                enroll_max_attempts_per_ip: enroll_max_ip as usize,
                signup_enabled,
                signup_max_per_ip: signup_max_ip as usize,
                signup_window: Duration::from_secs(signup_window),
                password_reset_ttl: hours(reset_ttl_hours),
            },
            dashboard: DashboardConfig {
                poll_interval_ms: poll,
                pause_when_hidden,
                pairing_poll_interval_ms: pairing_poll,
                graph_width_px: graph_width,
                graph_height_px: graph_height,
                graph_grid_px: graph_grid,
                graph_line_color: line_color,
                graph_grid_color: grid_color,
                graph_bg_color: bg_color,
                collapsed_max_gpus,
                blink_period_ms: blink_period,
                metrics_history_samples: history,
                tick_interval_ms: tick,
                pairing_code_ttl_secs: pairing_ttl,
            },
        })
    }

    /// The effective configuration, for the startup log.
    pub fn summary(&self) -> Vec<String> {
        let c = &self.client;
        let m = &self.metrics;
        let d = &self.dashboard;
        let a = &self.auth;
        vec![
            format!(
                "server: port {}, log level {}, auth timeout {} ms, db {} (keeping {} pre-migration backups), \
                 frontend {}",
                self.port,
                if self.debug_logging { "debug" } else { "info" },
                self.auth_timeout.as_millis(),
                self.db_path,
                self.db_migration_backups_keep,
                self.frontend_dir
            ),
            format!(
                "pushed to clients: tick {} ms, sampler {} ms, sample max age {} ms, server silence timeout {} ms, \
                 reconnect backoff {}..{} ms",
                c.tick_interval_ms,
                c.sampler_interval_ms,
                c.sample_max_age_ms,
                c.server_silence_timeout_ms,
                c.reconnect_backoff_initial_ms,
                c.reconnect_backoff_max_ms
            ),
            format!(
                "liveness: timeout {} ms, ws ping every {} ms, last_seen flush every {} ms",
                self.liveness_timeout.as_millis(),
                self.ws_ping_interval.as_millis(),
                self.last_seen_flush_interval.as_millis()
            ),
            format!(
                "metrics: {} samples per client, min spacing {} ms, max {} GPUs, max string length {}",
                m.history_samples,
                m.min_spacing.as_millis(),
                m.max_gpus,
                m.max_string_len
            ),
            format!(
                "pairing: code TTL {} s, dashboard polls its status every {} ms",
                self.pairing_code_ttl_secs, d.pairing_poll_interval_ms
            ),
            format!(
                "chat: idle timeout {} ms, flush every {} ms, retention {}, max message {} chars, \
                 max reply {} chars, {} active repl(ies) per user",
                self.chat.idle_timeout.as_millis(),
                self.chat.flush_interval.as_millis(),
                match self.chat.retention_days {
                    0 => "forever".to_string(),
                    days => format!("{days} day(s)"),
                },
                self.chat.max_message_chars,
                self.chat.max_reply_chars,
                self.chat.max_active_replies_per_user
            ),
            format!(
                "inference (pushed to clients): {} at once, max {} tokens, default context {}, threads {}, \
                 unload after {} s idle, load timeout {} ms, progress every {} ms, disk reserve {} MiB, echo model {}",
                c.inference.max_concurrent,
                c.inference.max_tokens,
                c.inference.default_context,
                match c.inference.threads {
                    0 => "auto".to_string(),
                    n => n.to_string(),
                },
                c.inference.idle_unload_secs,
                c.inference.model_load_timeout_ms,
                c.inference.progress_interval_ms,
                c.inference.disk_reserve_mb,
                if c.inference.echo_model_enabled { "on" } else { "off" }
            ),
            format!(
                "catalog: {} ({}, {} bytes, context {}) from {}",
                self.test_model.id,
                self.test_model.display_name,
                self.test_model.source.size_bytes,
                self.test_model.context_length.unwrap_or(0),
                self.test_model.source.url
            ),
            format!(
                "http: public origin {}, extra origins [{}], secure cookie {}, trust CF-Connecting-IP {}",
                self.http.public_origin,
                self.http.extra_allowed_origins.join(", "),
                self.http.cookie_secure,
                self.http.trust_cf_connecting_ip
            ),
            format!(
                "auth: sign-up {}, sessions idle {} h / remember-me {} d / otherwise {} h, touch every {} s, \
                 passwords {}..{} chars, argon2id m={} KiB t={} p={}, reset links valid {} h",
                if a.signup_enabled { "enabled" } else { "disabled" },
                a.session_idle_timeout.as_secs() / 3600,
                a.remember_days,
                a.session_short_timeout.as_secs() / 3600,
                a.session_touch_interval.as_secs(),
                a.password_min_len,
                a.password_max_len,
                a.argon2.memory_kib,
                a.argon2.iterations,
                a.argon2.parallelism,
                a.password_reset_ttl.as_secs() / 3600
            ),
            format!(
                "rate limits: {} failed logins per username+IP and {} per IP every {} s, {} enrolls per IP \
                 in the same window, {} sign-ups per IP every {} s",
                a.login_max_failures,
                a.login_max_failures_per_ip,
                a.login_window.as_secs(),
                a.enroll_max_attempts_per_ip,
                a.signup_max_per_ip,
                a.signup_window.as_secs()
            ),
            format!(
                "dashboard: poll every {} ms, pause when hidden {}, graph {}x{} px grid {} px, \
                 colours line {} grid {} bg {}, {} GPU(s) when collapsed, blink every {} ms",
                d.poll_interval_ms,
                d.pause_when_hidden,
                d.graph_width_px,
                d.graph_height_px,
                d.graph_grid_px,
                d.graph_line_color,
                d.graph_grid_color,
                d.graph_bg_color,
                d.collapsed_max_gpus,
                d.blink_period_ms
            ),
        ]
    }
}

impl Settings {
    /// Valid but probably unintended combinations, for startup warnings.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.http.cookie_secure && self.http.public_origin.starts_with("http://") {
            warnings.push(format!(
                "COOKIE_SECURE is true but PUBLIC_ORIGIN ({}) is plain http: browsers won't send the session \
                 cookie except on localhost. Set COOKIE_SECURE=false for plain-http deployments.",
                self.http.public_origin
            ));
        }
        warnings
    }
}

/// The test model's defaults: Qwen2.5 0.5B Instruct, Q4_K_M, from Qwen's own
/// Hugging Face repository. The size and SHA-256 (the file's LFS object id)
/// were checked against Hugging Face.
pub const DEFAULT_TEST_MODEL_ID: &str = "qwen2.5-0.5b-instruct-q4_k_m";
pub const DEFAULT_TEST_MODEL_NAME: &str = "Qwen2.5 0.5B Instruct (Q4_K_M)";
pub const DEFAULT_TEST_MODEL_URL: &str =
    "https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/main/qwen2.5-0.5b-instruct-q4_k_m.gguf";
pub const DEFAULT_TEST_MODEL_SHA256: &str = "74a4da8c9fdbcd15bd1f6d01d621410d31c6fc00986f5eb687824e7b93d7a9db";
pub const DEFAULT_TEST_MODEL_SIZE_BYTES: u64 = 491_400_032;

fn hours(n: u64) -> Duration {
    Duration::from_secs(n.saturating_mul(3600))
}

/// `scheme://host[:port]` as a browser sends it in `Origin`: lowercase, no
/// path, no default port. None if it isn't an http(s) origin.
pub fn normalize_origin(raw: &str) -> Option<String> {
    let value = raw.trim().trim_end_matches('/');
    let (scheme, authority) = value.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "https" => ":443",
        "http" => ":80",
        _ => return None,
    };
    let authority = authority.to_ascii_lowercase();
    if authority.is_empty() || authority.contains(|c: char| matches!(c, '/' | '?' | '#' | '@') || c.is_whitespace()) {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        // An IPv6 literal's colons sit inside brackets.
        Some((host, port)) if !port.contains(']') => (host, Some(port)),
        _ => (authority.as_str(), None),
    };
    if host.is_empty() || port.is_some_and(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    let authority = authority.strip_suffix(default_port).unwrap_or(&authority);
    Some(format!("{scheme}://{authority}"))
}

struct Parser<'a> {
    lookup: &'a dyn Fn(&str) -> Option<String>,
    errors: Vec<String>,
}

impl Parser<'_> {
    fn string(&mut self, key: &str, default: &str) -> String {
        (self.lookup)(key)
            .map(|v| v.trim().to_string())
            .unwrap_or_else(|| default.to_string())
    }

    /// A whole number of at least `min`. On error, records it and returns
    /// the default so later cross-checks still have something to compare.
    fn int(&mut self, key: &str, default: u64, min: u64) -> u64 {
        let Some(raw) = (self.lookup)(key) else {
            return default;
        };
        match raw.trim().parse::<u64>() {
            Ok(n) if n >= min => n,
            Ok(n) => {
                self.errors.push(format!("{key} must be at least {min}, got {n}"));
                default
            }
            Err(_) => {
                self.errors
                    .push(format!("{key} must be a whole number (at least {min}), got {raw:?}"));
                default
            }
        }
    }

    /// A whole number from `min` to `max`, for the u32 settings.
    fn int_in(&mut self, key: &str, default: u32, min: u32, max: u32) -> u32 {
        let n = self.int(key, u64::from(default), u64::from(min));
        if n > u64::from(max) {
            self.errors.push(format!("{key} must be at most {max}, got {n}"));
            return default;
        }
        n as u32
    }

    fn bool(&mut self, key: &str, default: bool) -> bool {
        let Some(raw) = (self.lookup)(key) else {
            return default;
        };
        match raw.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => true,
            "false" | "0" | "no" | "off" => false,
            _ => {
                self.errors.push(format!("{key} must be true or false, got {raw:?}"));
                default
            }
        }
    }

    /// `#rgb` or `#rrggbb`, normalised to lowercase.
    fn color(&mut self, key: &str, default: &str) -> String {
        let Some(raw) = (self.lookup)(key) else {
            return default.to_string();
        };
        let value = raw.trim();
        let hex = value.strip_prefix('#').unwrap_or("");
        if (hex.len() == 3 || hex.len() == 6) && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            value.to_ascii_lowercase()
        } else {
            self.errors
                .push(format!("{key} must be a hex colour like #0f0 or #00ff00, got {raw:?}"));
            default.to_string()
        }
    }

    fn origin(&mut self, key: &str, default: &str) -> String {
        let raw = self.string(key, default);
        normalize_origin(&raw).unwrap_or_else(|| {
            self.errors.push(format!(
                "{key} must be an origin like https://example.com or http://localhost:5173 (no path), got {raw:?}"
            ));
            default.to_string()
        })
    }

    /// Comma-separated origins; blank entries are skipped.
    fn origin_list(&mut self, key: &str) -> Vec<String> {
        let raw = self.string(key, "");
        let mut origins = Vec::new();
        for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            match normalize_origin(entry) {
                Some(origin) => origins.push(origin),
                None => self.errors.push(format!(
                    "{key} must be comma-separated origins like http://localhost:5173 (no paths), got {entry:?}"
                )),
            }
        }
        origins
    }

    fn at_least_twice(&mut self, key: &str, value: u64, other_key: &str, other: u64) {
        if value < other.saturating_mul(2) {
            self.errors.push(format!(
                "{key} ({value}) must be at least 2 x {other_key} ({other}), i.e. at least {}",
                other.saturating_mul(2)
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(pairs: &[(&str, &str)]) -> Result<Settings, Vec<String>> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        Settings::parse(|key| map.get(key).cloned())
    }

    /// Asserts the config is rejected and the only error names `key`.
    fn rejects(pairs: &[(&str, &str)], key: &str) {
        let errors = parse(pairs).expect_err("expected the config to be rejected");
        assert_eq!(errors.len(), 1, "expected exactly one error, got {errors:?}");
        assert!(errors[0].starts_with(key), "error should name {key}: {:?}", errors[0]);
    }

    #[test]
    fn parses_dotenv_lines() {
        let map = parse_dotenv("# comment\n\nA=1\nexport B = two \nC=\"quoted\"\nD='single'\nnot a pair\n");
        assert_eq!(map.get("A").map(String::as_str), Some("1"));
        assert_eq!(map.get("B").map(String::as_str), Some("two"));
        assert_eq!(map.get("C").map(String::as_str), Some("quoted"));
        assert_eq!(map.get("D").map(String::as_str), Some("single"));
        assert_eq!(map.len(), 4);
    }

    #[test]
    fn empty_config_gives_the_documented_defaults() {
        let s = parse(&[]).unwrap();
        assert_eq!(s.port, 7070);
        assert!(!s.debug_logging);
        assert_eq!(s.client, ClientSettings::default());
        assert_eq!(s.client.inference, InferenceSettings::default());
        assert_eq!(s.test_model.id, "qwen2.5-0.5b-instruct-q4_k_m");
        assert_eq!(s.test_model.source.size_bytes, 491_400_032);
        assert_eq!(s.test_model.context_length, Some(4_096));
        assert!(s.test_model.source.url.starts_with("https://huggingface.co/Qwen/"));
        assert_eq!(s.liveness_timeout, Duration::from_millis(4_000));
        assert_eq!(s.ws_ping_interval, Duration::from_millis(2_000));
        assert_eq!(s.last_seen_flush_interval, Duration::from_millis(30_000));
        assert_eq!(
            s.metrics,
            MetricsLimits {
                history_samples: 60,
                min_spacing: Duration::from_millis(500),
                max_gpus: 16,
                max_string_len: 128,
            }
        );
        assert_eq!(s.pairing_code_ttl_secs, 600);
        assert_eq!(
            s.chat,
            ChatSettings {
                idle_timeout: Duration::from_millis(120_000),
                flush_interval: Duration::from_millis(3_000),
                retention_days: 0,
                max_message_chars: 100_000,
                max_reply_chars: 400_000,
                max_active_replies_per_user: 3,
            }
        );
        assert_eq!(
            s.http,
            HttpSettings {
                public_origin: "https://llm.smith-c.com".into(),
                extra_allowed_origins: vec![],
                cookie_secure: true,
                trust_cf_connecting_ip: true,
            }
        );
        assert_eq!(
            s.auth,
            AuthSettings {
                session_idle_timeout: Duration::from_secs(168 * 3600),
                session_absolute_timeout: Duration::from_secs(30 * 24 * 3600),
                remember_days: 30,
                session_short_timeout: Duration::from_secs(12 * 3600),
                session_touch_interval: Duration::from_secs(300),
                password_min_len: 12,
                password_max_len: 256,
                argon2: Argon2Settings {
                    memory_kib: 19_456,
                    iterations: 2,
                    parallelism: 1,
                },
                login_max_failures: 5,
                login_max_failures_per_ip: 20,
                login_window: Duration::from_secs(900),
                enroll_max_attempts_per_ip: 10,
                signup_enabled: true,
                signup_max_per_ip: 5,
                signup_window: Duration::from_secs(3600),
                password_reset_ttl: Duration::from_secs(24 * 3600),
            }
        );
        assert!(s.warnings().is_empty());
        assert_eq!(
            s.dashboard,
            DashboardConfig {
                poll_interval_ms: 1_000,
                pause_when_hidden: true,
                pairing_poll_interval_ms: 1_500,
                graph_width_px: 120,
                graph_height_px: 40,
                graph_grid_px: 10,
                graph_line_color: "#00ff00".into(),
                graph_grid_color: "#008040".into(),
                graph_bg_color: "#000000".into(),
                collapsed_max_gpus: 1,
                blink_period_ms: 1_000,
                metrics_history_samples: 60,
                tick_interval_ms: 1_000,
                pairing_code_ttl_secs: 600,
            }
        );
    }

    #[test]
    fn values_override_defaults_and_reach_the_dashboard_config() {
        let s = parse(&[
            ("TICK_INTERVAL_MS", "2000"),
            ("METRICS_HISTORY_SAMPLES", "120"),
            ("DASHBOARD_PAUSE_WHEN_HIDDEN", "false"),
            ("DASHBOARD_GRAPH_LINE_COLOR", "#0F0"),
            ("PAIRING_CODE_TTL_SECS", "300"),
            ("SERVER_LOG_LEVEL", "DEBUG"),
        ])
        .unwrap();
        assert_eq!(s.client.tick_interval_ms, 2_000);
        assert_eq!(s.dashboard.tick_interval_ms, 2_000);
        assert_eq!(s.metrics.history_samples, 120);
        assert_eq!(s.dashboard.metrics_history_samples, 120);
        assert!(!s.dashboard.pause_when_hidden);
        assert_eq!(s.dashboard.graph_line_color, "#0f0");
        assert_eq!(s.dashboard.pairing_code_ttl_secs, 300);
        assert!(s.debug_logging);
    }

    #[test]
    fn rejects_malformed_single_values() {
        rejects(&[("TICK_INTERVAL_MS", "fast")], "TICK_INTERVAL_MS");
        rejects(&[("TICK_INTERVAL_MS", "-5")], "TICK_INTERVAL_MS");
        rejects(&[("WS_PING_INTERVAL_MS", "0")], "WS_PING_INTERVAL_MS");
        rejects(&[("SAMPLER_INTERVAL_MS", "199")], "SAMPLER_INTERVAL_MS");
        rejects(&[("METRICS_HISTORY_SAMPLES", "1")], "METRICS_HISTORY_SAMPLES");
        rejects(&[("DASHBOARD_GRAPH_WIDTH_PX", "0")], "DASHBOARD_GRAPH_WIDTH_PX");
        rejects(&[("DASHBOARD_GRAPH_HEIGHT_PX", "4.5")], "DASHBOARD_GRAPH_HEIGHT_PX");
        rejects(&[("DASHBOARD_GRAPH_GRID_COLOR", "green")], "DASHBOARD_GRAPH_GRID_COLOR");
        rejects(&[("DASHBOARD_GRAPH_BG_COLOR", "#00000")], "DASHBOARD_GRAPH_BG_COLOR");
        rejects(&[("DASHBOARD_GRAPH_LINE_COLOR", "#ggg")], "DASHBOARD_GRAPH_LINE_COLOR");
        rejects(
            &[("DASHBOARD_PAUSE_WHEN_HIDDEN", "maybe")],
            "DASHBOARD_PAUSE_WHEN_HIDDEN",
        );
        rejects(&[("SERVER_PORT", "70000")], "SERVER_PORT");
        rejects(&[("SERVER_LOG_LEVEL", "trace")], "SERVER_LOG_LEVEL");
        // Zero is allowed where it means "none" or "off".
        assert!(parse(&[("DASHBOARD_COLLAPSED_MAX_GPUS", "0"), ("METRICS_MIN_SPACING_MS", "0")]).is_ok());
    }

    #[test]
    fn sample_max_age_must_cover_two_sampler_intervals() {
        rejects(&[("SAMPLER_INTERVAL_MS", "2000")], "SAMPLE_MAX_AGE_MS");
        rejects(&[("SAMPLE_MAX_AGE_MS", "999")], "SAMPLE_MAX_AGE_MS");
        assert!(parse(&[("SAMPLE_MAX_AGE_MS", "1000")]).is_ok());
    }

    #[test]
    fn liveness_timeout_must_cover_two_ticks() {
        rejects(&[("LIVENESS_TIMEOUT_MS", "1999")], "LIVENESS_TIMEOUT_MS");
        // Raising the tick alone breaks the default liveness timeout.
        rejects(&[("TICK_INTERVAL_MS", "2500")], "LIVENESS_TIMEOUT_MS");
        assert!(parse(&[("LIVENESS_TIMEOUT_MS", "2000")]).is_ok());
    }

    #[test]
    fn server_silence_timeout_must_cover_two_pings() {
        rejects(&[("WS_PING_INTERVAL_MS", "3001")], "SERVER_SILENCE_TIMEOUT_MS");
        rejects(&[("SERVER_SILENCE_TIMEOUT_MS", "3999")], "SERVER_SILENCE_TIMEOUT_MS");
        assert!(parse(&[("SERVER_SILENCE_TIMEOUT_MS", "4000")]).is_ok());
    }

    #[test]
    fn backoff_max_must_not_be_below_initial() {
        rejects(&[("RECONNECT_BACKOFF_INITIAL_MS", "60000")], "RECONNECT_BACKOFF_MAX_MS");
        assert!(parse(&[("RECONNECT_BACKOFF_INITIAL_MS", "30000")]).is_ok());
    }

    #[test]
    fn min_spacing_must_be_below_the_tick() {
        rejects(&[("METRICS_MIN_SPACING_MS", "1000")], "METRICS_MIN_SPACING_MS");
        rejects(
            &[("TICK_INTERVAL_MS", "500"), ("LIVENESS_TIMEOUT_MS", "4000")],
            "METRICS_MIN_SPACING_MS",
        );
        assert!(parse(&[("METRICS_MIN_SPACING_MS", "999")]).is_ok());
    }

    #[test]
    fn reports_every_problem_at_once() {
        let errors = parse(&[("TICK_INTERVAL_MS", "x"), ("DASHBOARD_GRAPH_BG_COLOR", "black")]).unwrap_err();
        assert_eq!(errors.len(), 2, "{errors:?}");
    }

    #[test]
    fn origins_are_normalised_and_validated() {
        let s = parse(&[
            ("PUBLIC_ORIGIN", "HTTPS://Example.com:443/"),
            ("EXTRA_ALLOWED_ORIGINS", "http://localhost:5173, ,http://127.0.0.1:80"),
        ])
        .unwrap();
        assert_eq!(s.http.public_origin, "https://example.com");
        assert_eq!(
            s.http.extra_allowed_origins,
            vec!["http://localhost:5173".to_string(), "http://127.0.0.1".to_string()]
        );
        assert!(s.http.origin_allowed("https://example.com"));
        assert!(s.http.origin_allowed("http://localhost:5173"));
        assert!(!s.http.origin_allowed("https://evil.example"));
        assert!(!s.http.origin_allowed("null"));
        assert!(!s.http.origin_allowed("http://localhost:5174"));

        rejects(&[("PUBLIC_ORIGIN", "llm.smith-c.com")], "PUBLIC_ORIGIN");
        rejects(&[("PUBLIC_ORIGIN", "https://llm.smith-c.com/app")], "PUBLIC_ORIGIN");
        rejects(&[("PUBLIC_ORIGIN", "ftp://llm.smith-c.com")], "PUBLIC_ORIGIN");
        rejects(
            &[("EXTRA_ALLOWED_ORIGINS", "http://ok.test,http://bad.test:x")],
            "EXTRA_ALLOWED_ORIGINS",
        );
    }

    #[test]
    fn auth_rules_between_values() {
        rejects(
            &[("AUTH_PASSWORD_MIN_LEN", "20"), ("AUTH_PASSWORD_MAX_LEN", "19")],
            "AUTH_PASSWORD_MAX_LEN",
        );
        rejects(
            &[("AUTH_ARGON2_PARALLELISM", "4"), ("AUTH_ARGON2_MEMORY_KIB", "31")],
            "AUTH_ARGON2_MEMORY_KIB",
        );
        rejects(&[("AUTH_ARGON2_ITERATIONS", "0")], "AUTH_ARGON2_ITERATIONS");
        rejects(&[("AUTH_SIGNUP_ENABLED", "sometimes")], "AUTH_SIGNUP_ENABLED");
        let s = parse(&[("COOKIE_SECURE", "false"), ("SESSION_ABSOLUTE_TIMEOUT_DAYS", "7")]).unwrap();
        assert!(!s.http.cookie_secure);
        assert_eq!(s.auth.remember_days, 7);
        assert_eq!(s.auth.session_absolute_timeout, Duration::from_secs(7 * 86400));
        // Allowed but warned about.
        let s = parse(&[("PUBLIC_ORIGIN", "http://192.168.1.5:7070")]).unwrap();
        assert_eq!(s.warnings().len(), 1);
    }

    #[test]
    fn the_example_config_parses_to_the_compiled_defaults() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../config.env.example");
        let file = parse_dotenv(&std::fs::read_to_string(path).unwrap());
        let s = Settings::parse(|key| file.get(key).cloned()).expect("config.env.example is valid");
        let defaults = parse(&[]).unwrap();
        assert_eq!(s.client, defaults.client, "the pushed values, inference included");
        assert_eq!(s.test_model, defaults.test_model);
        assert_eq!(s.chat, defaults.chat);
    }

    #[test]
    fn inference_and_test_model_settings_are_validated() {
        let s = parse(&[
            ("INFERENCE_MAX_CONCURRENT", "2"),
            ("INFERENCE_THREADS", "8"),
            ("INFERENCE_IDLE_UNLOAD_SECS", "0"),
            ("INFERENCE_ECHO_MODEL", "true"),
            ("TEST_MODEL_SHA256", &"AB".repeat(32)),
        ])
        .unwrap();
        assert_eq!(s.client.inference.max_concurrent, 2);
        assert_eq!(s.client.inference.threads, 8);
        assert_eq!(s.client.inference.idle_unload_secs, 0);
        assert!(s.client.inference.echo_model_enabled);
        assert!(
            !parse(&[]).unwrap().client.inference.echo_model_enabled,
            "off by default"
        );
        assert_eq!(s.test_model.source.sha256, "ab".repeat(32), "normalised to lowercase");

        rejects(&[("INFERENCE_MAX_CONCURRENT", "0")], "INFERENCE_MAX_CONCURRENT");
        rejects(&[("INFERENCE_MAX_CONCURRENT", "65")], "INFERENCE_MAX_CONCURRENT");
        rejects(&[("INFERENCE_DEFAULT_CONTEXT", "100")], "INFERENCE_DEFAULT_CONTEXT");
        rejects(
            &[("INFERENCE_PROGRESS_INTERVAL_MS", "10")],
            "INFERENCE_PROGRESS_INTERVAL_MS",
        );
        rejects(&[("INFERENCE_ECHO_MODEL", "maybe")], "INFERENCE_ECHO_MODEL");
        rejects(&[("TEST_MODEL_ID", "../escape")], "TEST_MODEL_ID");
        rejects(&[("TEST_MODEL_ID", "Qwen")], "TEST_MODEL_ID");
        rejects(&[("TEST_MODEL_URL", "http://example.com/m.gguf")], "TEST_MODEL_URL");
        rejects(&[("TEST_MODEL_SHA256", "abc")], "TEST_MODEL_SHA256");
        rejects(&[("TEST_MODEL_SIZE_BYTES", "0")], "TEST_MODEL_SIZE_BYTES");
        // The reply has to leave room for a prompt.
        rejects(&[("TEST_MODEL_CONTEXT", "2048")], "INFERENCE_MAX_TOKENS");
        assert_eq!(parse(&[("INFERENCE_MAX_TOKENS", "4096")]).unwrap_err().len(), 2);
    }

    #[test]
    fn warns_about_replaced_keys() {
        let set = ["SERVER_HEARTBEAT_TIMEOUT_MS", "CLIENT_HEARTBEAT_INTERVAL_MS"];
        let lines = replaced_keys_in_use(|k| set.contains(&k).then(|| "1".to_string()));
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("SERVER_HEARTBEAT_TIMEOUT_MS"));
    }
}
