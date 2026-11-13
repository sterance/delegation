//! The catalog: the models this server is willing to install on clients.
//!
//! For now it's the entry built from the `TEST_MODEL_*` settings, plus any
//! in the JSON file at `MODEL_CATALOG_PATH` (for testing bigger models). The
//! suggestion engine will replace it with a database table it fills (model
//! files and their hashes); either way, browsers only ever name catalog
//! entries by id. They never send URLs or hashes, so whatever a client
//! downloads is something the server chose.

use crate::config::Settings;
use protocol::{Backend, ModelMeta, ModelSpec};
use serde::Serialize;

pub struct Catalog {
    entries: Vec<ModelSpec>,
}

/// What the dashboard sees of an entry: no URL or hash, it doesn't need them.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct CatalogEntry {
    pub id: String,
    pub display_name: String,
    pub backend: Backend,
    pub size_bytes: u64,
    pub context_length: Option<u32>,
    pub meta: ModelMeta,
    /// Clients with an older llama.cpp can't install it.
    pub min_runtime_version: Option<String>,
}

impl Catalog {
    pub fn from_settings(settings: &Settings) -> Catalog {
        Catalog {
            entries: std::iter::once(settings.test_model.clone())
                .chain(settings.catalog_extra.iter().cloned())
                .collect(),
        }
    }

    pub fn get(&self, id: &str) -> Option<&ModelSpec> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn list(&self) -> Vec<CatalogEntry> {
        self.entries
            .iter()
            .map(|e| CatalogEntry {
                id: e.id.clone(),
                display_name: e.display_name.clone(),
                backend: e.backend,
                size_bytes: e.source.size_bytes,
                context_length: e.context_length,
                meta: e.meta.clone(),
                min_runtime_version: e.min_runtime_version.clone(),
            })
            .collect()
    }
}
