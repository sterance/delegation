//! Shared `config.env` loading for the server and client.
//!
//! Lookup order for every key: real environment variable, then the
//! first config file found, then the caller's default. Docker Compose
//! passes `config.env` in as environment variables (`env_file:`), so
//! inside containers the file lookup simply finds nothing and the env
//! wins; natively, the file is read directly. Deliberately never calls
//! `std::env::set_var` — it's racy once the tokio runtime has spawned
//! threads, and a plain map lookup does the same job.

use std::{collections::HashMap, path::PathBuf, time::Duration};

pub struct Config {
    file: HashMap<String, String>,
    source: Option<PathBuf>,
}

impl Config {
    /// Reads the first existing file among `$DELEGATION_CONFIG` and then
    /// `candidates`. A missing file is fine (everything falls back to
    /// defaults); an explicitly named `$DELEGATION_CONFIG` that can't be
    /// read is reported, since that's almost certainly a typo.
    pub fn load(candidates: &[PathBuf]) -> Self {
        let explicit = std::env::var_os("DELEGATION_CONFIG").map(PathBuf::from);
        if let Some(path) = &explicit {
            if !path.is_file() {
                eprintln!("[{}] [config] DELEGATION_CONFIG={} not found, ignoring", crate::ts(), path.display());
            }
        }
        for path in explicit.iter().chain(candidates) {
            if let Ok(text) = std::fs::read_to_string(path) {
                return Config { file: parse(&text), source: Some(path.clone()) };
            }
        }
        Config { file: HashMap::new(), source: None }
    }

    /// The file values were read from, if any — for the startup log line.
    pub fn source(&self) -> Option<&PathBuf> {
        self.source.as_ref()
    }

    pub fn get(&self, key: &str) -> Option<String> {
        std::env::var(key)
            .ok()
            .or_else(|| self.file.get(key).cloned())
            .filter(|v| !v.is_empty())
    }

    pub fn string(&self, key: &str, default: &str) -> String {
        self.get(key).unwrap_or_else(|| default.to_string())
    }

    /// A positive integer, falling back to `default` (with a warning) if
    /// the value is present but unparseable or zero.
    pub fn u64(&self, key: &str, default: u64) -> u64 {
        match self.get(key) {
            None => default,
            Some(raw) => match raw.parse::<u64>() {
                Ok(n) if n > 0 => n,
                _ => {
                    eprintln!("[{}] [config] invalid {key}={raw:?}, using default {default}", crate::ts());
                    default
                }
            },
        }
    }

    /// A `*_MS` key as a Duration.
    pub fn ms(&self, key: &str, default_ms: u64) -> Duration {
        Duration::from_millis(self.u64(key, default_ms))
    }
}

/// Minimal dotenv: `KEY=value` lines, `#` comments, optional `export `
/// prefix and surrounding quotes. No interpolation or multi-line values.
fn parse(text: &str) -> HashMap<String, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dotenv_lines() {
        let map = parse("# comment\n\nA=1\nexport B = two \nC=\"quoted\"\nD='single'\nnot a pair\n");
        assert_eq!(map.get("A").map(String::as_str), Some("1"));
        assert_eq!(map.get("B").map(String::as_str), Some("two"));
        assert_eq!(map.get("C").map(String::as_str), Some("quoted"));
        assert_eq!(map.get("D").map(String::as_str), Some("single"));
        assert_eq!(map.len(), 4);
    }
}
