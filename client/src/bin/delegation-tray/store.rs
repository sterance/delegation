//! `servers.json`: the tray's saved servers, which one is active, and
//! whether the user last disconnected it by hand. Each server has its own
//! keypair, so servers can't correlate this machine by public key.

use anyhow::{bail, Context, Result};
use delegation_client::identity::Identity;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct Store {
    pub active: usize,
    pub manually_disconnected: bool,
    pub servers: Vec<SavedServer>,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct SavedServer {
    pub url: String,
    /// Both cleared when the server says it no longer knows this machine,
    /// so it can be re-paired without losing its place in the list.
    pub client_id: Option<String>,
    pub signing_key_b64: Option<String>,
}

impl SavedServer {
    pub fn identity(&self) -> Option<Identity> {
        let client_id = self.client_id.clone()?;
        Identity::from_b64(client_id, self.signing_key_b64.as_deref()?).ok()
    }
}

impl Store {
    pub fn default_path() -> PathBuf {
        delegation_client::paths::config_dir().join("servers.json")
    }

    /// A missing file is an empty store. An unreadable one is moved aside
    /// rather than silently overwritten, since it holds private keys.
    pub fn load(path: &Path) -> Store {
        let Ok(raw) = std::fs::read_to_string(path) else {
            return Store::default();
        };
        match serde_json::from_str(&raw) {
            Ok(store) => store,
            Err(e) => {
                let aside = path.with_extension(format!("json.corrupt-{}", std::process::id()));
                log::error!(target: "tray", "can't parse {}: {e}; moved it to {}", path.display(), aside.display());
                let _ = std::fs::rename(path, &aside);
                Store::default()
            }
        }
    }

    /// Reads without side effects (the form uses this; only the tray
    /// itself ever moves a broken file aside).
    pub fn peek(path: &Path) -> Option<Store> {
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    }

    /// Writes a temp file and renames it over the old one, so a crash
    /// mid-write can't leave a half-written store.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).context("creating config dir")?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?).context("writing servers.json.tmp")?;
        std::fs::rename(&tmp, path).context("replacing servers.json")?;
        Ok(())
    }

    pub fn find(&self, url: &str) -> Option<usize> {
        let url = normalize_url(url).ok()?;
        self.servers
            .iter()
            .position(|s| normalize_url(&s.url).ok().as_deref() == Some(url.as_str()))
    }

    pub fn active_server(&self) -> Option<&SavedServer> {
        self.servers.get(self.active)
    }

    /// Saves a freshly enrolled identity, replacing that server's old one
    /// if it's already in the list, and makes it the active server.
    pub fn upsert(&mut self, url: &str, identity: &Identity) -> usize {
        let entry = SavedServer {
            url: url.to_string(),
            client_id: Some(identity.client_id.clone()),
            signing_key_b64: Some(identity.signing_key_b64()),
        };
        let index = match self.find(url) {
            Some(i) => {
                self.servers[i] = entry;
                i
            }
            None => {
                self.servers.push(entry);
                self.servers.len() - 1
            }
        };
        self.active = index;
        self.manually_disconnected = false;
        index
    }

    pub fn forget_identity(&mut self, index: usize) {
        if let Some(server) = self.servers.get_mut(index) {
            server.client_id = None;
            server.signing_key_b64 = None;
        }
    }
}

/// "LLM.Example.com/" -> "https://llm.example.com". The normalized form is
/// what's stored and what "do we already know this server?" compares.
pub fn normalize_url(input: &str) -> Result<String> {
    let input = input.trim();
    let (scheme, rest) = match input.split_once("://") {
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
        None => ("https".to_string(), input),
    };
    if scheme != "http" && scheme != "https" {
        bail!("the URL must start with http:// or https://");
    }
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if host.is_empty() || host.contains(char::is_whitespace) || path.contains(char::is_whitespace) {
        bail!("that doesn't look like a server address");
    }
    Ok(format!(
        "{scheme}://{}{}",
        host.to_ascii_lowercase(),
        path.trim_end_matches('/')
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_urls() {
        assert_eq!(normalize_url("LLM.Smith-C.com/").unwrap(), "https://llm.smith-c.com");
        assert_eq!(
            normalize_url("  http://localhost:7070  ").unwrap(),
            "http://localhost:7070"
        );
        assert_eq!(
            normalize_url("HTTPS://Host/Sub/Path/").unwrap(),
            "https://host/Sub/Path"
        );
        assert!(normalize_url("ftp://host").is_err());
        assert!(normalize_url("").is_err());
        assert!(normalize_url("https://").is_err());
        assert!(normalize_url("has space.com").is_err());
    }

    #[test]
    fn upsert_find_and_forget() {
        // 32 zero bytes, base64.
        let identity = Identity::from_b64("id-1".into(), "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap();

        let mut store = Store {
            manually_disconnected: true,
            ..Store::default()
        };
        assert_eq!(store.upsert("https://a.example", &identity), 0);
        assert_eq!(store.upsert("https://b.example", &identity), 1);
        assert_eq!(store.active, 1);
        assert!(!store.manually_disconnected);

        assert_eq!(store.find("A.EXAMPLE/"), Some(0));
        assert_eq!(
            store.upsert("a.example", &identity),
            0,
            "same server must not be added twice"
        );
        assert_eq!(store.servers.len(), 2);

        store.forget_identity(0);
        assert!(store.servers[0].identity().is_none());
        assert_eq!(store.servers[1].identity().unwrap().client_id, "id-1");
    }

    #[test]
    fn save_and_load_round_trip_and_corrupt_file_is_kept() {
        let dir = std::env::temp_dir().join(format!("delegation-store-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("servers.json");
        assert_eq!(Store::load(&path), Store::default());

        let store = Store {
            active: 1,
            manually_disconnected: true,
            servers: vec![
                SavedServer {
                    url: "https://a".into(),
                    ..Default::default()
                };
                2
            ],
        };
        store.save(&path).unwrap();
        assert_eq!(Store::load(&path), store);

        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Store::load(&path), Store::default());
        let kept = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains("corrupt"));
        assert!(kept, "corrupt store should be moved aside, not deleted");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
