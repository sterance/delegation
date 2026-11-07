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

use protocol::telemetry::MIN_SAMPLER_INTERVAL_MS;
use protocol::ClientSettings;
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
    pub frontend_dir: String,
    /// Pushed to every client in `welcome`.
    pub client: ClientSettings,
    pub liveness_timeout: Duration,
    pub ws_ping_interval: Duration,
    pub last_seen_flush_interval: Duration,
    pub metrics: MetricsLimits,
    pub pairing_code_ttl_secs: u64,
    pub dashboard: DashboardConfig,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetricsLimits {
    /// Ring buffer length per client, and the graph width in samples.
    pub history_samples: usize,
    pub min_spacing: Duration,
    pub max_gpus: usize,
    pub max_string_len: usize,
}

/// What `GET /admin/dashboard-config` returns. Field names are the
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

        if !p.errors.is_empty() {
            return Err(p.errors);
        }
        Ok(Settings {
            port: port as u16,
            debug_logging,
            auth_timeout: Duration::from_millis(auth_timeout_ms),
            db_path,
            frontend_dir,
            client: ClientSettings {
                tick_interval_ms: tick,
                sampler_interval_ms: sampler,
                sample_max_age_ms: max_age,
                server_silence_timeout_ms: silence,
                reconnect_backoff_initial_ms: backoff_initial,
                reconnect_backoff_max_ms: backoff_max,
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
        vec![
            format!(
                "server: port {}, log level {}, auth timeout {} ms, db {}, frontend {}",
                self.port,
                if self.debug_logging { "debug" } else { "info" },
                self.auth_timeout.as_millis(),
                self.db_path,
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
                "dashboard: poll every {} ms, pause when hidden {}, graph {}x{} px grid {} px, \
                 colours line {} grid {} bg {}, {} GPU(s) when collapsed",
                d.poll_interval_ms,
                d.pause_when_hidden,
                d.graph_width_px,
                d.graph_height_px,
                d.graph_grid_px,
                d.graph_line_color,
                d.graph_grid_color,
                d.graph_bg_color,
                d.collapsed_max_gpus
            ),
        ]
    }
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
    fn warns_about_replaced_keys() {
        let set = ["SERVER_HEARTBEAT_TIMEOUT_MS", "CLIENT_HEARTBEAT_INTERVAL_MS"];
        let lines = replaced_keys_in_use(|k| set.contains(&k).then(|| "1".to_string()));
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("SERVER_HEARTBEAT_TIMEOUT_MS"));
    }
}
