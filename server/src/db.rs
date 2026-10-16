//! SQLite-backed persistence. Replaces the file-backed JSON registry from
//! the scaffold stage. `rusqlite`'s `bundled` feature compiles SQLite
//! from source as part of the Rust build, so there's no system SQLite
//! dependency and no musl/Alpine native-build story to worry about —
//! this was the whole reason the Rust rewrite made the earlier
//! node:sqlite-vs-better-sqlite3 question moot.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::sync::{Arc, Mutex};

pub type Db = Arc<Mutex<Connection>>;

pub fn open(path: &str) -> Result<Db> {
    let conn = Connection::open(path).context("opening sqlite database")?;
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS clients (
            client_id    TEXT PRIMARY KEY,
            public_key   TEXT NOT NULL,
            hostname     TEXT NOT NULL,
            enrolled_at  TEXT NOT NULL,
            last_seen_at TEXT
        );

        CREATE TABLE IF NOT EXISTS pairing_codes (
            code              TEXT PRIMARY KEY,
            expires_at        INTEGER NOT NULL,
            used_at           INTEGER,
            claimed_by_client TEXT
        );
        ",
    )
    .context("running schema migrations")?;
    add_missing_columns(
        &conn,
        "clients",
        &[
            ("os", "TEXT"),
            ("client_version", "TEXT"),
            ("specs_json", "TEXT"),
            ("info_updated_at", "TEXT"),
            // Set from the dashboard. NULL means "use the hostname", so an
            // unrenamed client keeps following hostname changes.
            ("friendly_name", "TEXT"),
        ],
    )
    .context("adding new client columns")?;
    Ok(Arc::new(Mutex::new(conn)))
}

/// SQLite has no `ADD COLUMN IF NOT EXISTS`, so check `table_info` first.
/// Safe to run against the live database on every start.
fn add_missing_columns(conn: &Connection, table: &str, columns: &[(&str, &str)]) -> Result<()> {
    let existing: Vec<String> = conn
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?;
    for (name, ty) in columns {
        if !existing.iter().any(|c| c == name) {
            conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {name} {ty}"), [])?;
        }
    }
    Ok(())
}

pub struct ClientRecord {
    pub client_id: String,
    pub public_key: String,
    pub hostname: String,
    #[allow(dead_code)]
    pub enrolled_at: String,
}

/// Create a new pairing code, valid for `ttl_secs`. Returns the code.
/// This is what the future "Add client" dashboard modal calls.
pub fn create_pairing_code(db: &Db, ttl_secs: i64, code: &str) -> Result<()> {
    let expires_at = now_epoch() + ttl_secs;
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO pairing_codes (code, expires_at) VALUES (?1, ?2)",
        (code, expires_at),
    )?;
    Ok(())
}

/// Validates a pairing code and, if valid, enrolls the client in one
/// transaction. Returns the new client_id on success.
///
/// Mirrors the scaffold's consume-regardless-of-outcome semantics: a
/// code is marked used on first use whether or not that use succeeds,
/// so it can't be brute-forced by retrying nearby guesses. Unlike the
/// scaffold, the row isn't deleted — `claimed_by_client` lets the
/// dashboard modal poll "has this code been used yet, and by whom"
/// without a race against a diff of the full client list.
pub fn try_enroll(db: &Db, code: &str, client_id: &str, public_key: &str, hostname: &str) -> Result<EnrollOutcome> {
    let mut conn = db.lock().unwrap();
    let tx = conn.transaction()?;

    let row: Option<(i64, Option<i64>)> = tx
        .query_row(
            "SELECT expires_at, used_at FROM pairing_codes WHERE code = ?1",
            [code],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();

    let Some((expires_at, used_at)) = row else {
        return Ok(EnrollOutcome::InvalidCode);
    };
    if used_at.is_some() {
        return Ok(EnrollOutcome::InvalidCode);
    }

    // Mark used regardless of whether expiry also fails it — a code gets
    // exactly one shot.
    tx.execute(
        "UPDATE pairing_codes SET used_at = ?1 WHERE code = ?2",
        (now_epoch(), code),
    )?;

    if expires_at < now_epoch() {
        tx.commit()?;
        return Ok(EnrollOutcome::InvalidCode);
    }

    let enrolled_at = protocol::ts();
    tx.execute(
        "INSERT INTO clients (client_id, public_key, hostname, enrolled_at) VALUES (?1, ?2, ?3, ?4)",
        (client_id, public_key, hostname, &enrolled_at),
    )?;
    tx.execute(
        "UPDATE pairing_codes SET claimed_by_client = ?1 WHERE code = ?2",
        (client_id, code),
    )?;

    tx.commit()?;
    Ok(EnrollOutcome::Enrolled)
}

pub enum EnrollOutcome {
    Enrolled,
    InvalidCode,
}

pub fn get_client(db: &Db, client_id: &str) -> Result<Option<ClientRecord>> {
    let conn = db.lock().unwrap();
    let result = conn
        .query_row(
            "SELECT client_id, public_key, hostname, enrolled_at FROM clients WHERE client_id = ?1",
            [client_id],
            |r| {
                Ok(ClientRecord {
                    client_id: r.get(0)?,
                    public_key: r.get(1)?,
                    hostname: r.get(2)?,
                    enrolled_at: r.get(3)?,
                })
            },
        )
        .ok();
    Ok(result)
}

/// Returns false if the client no longer exists (deleted from the
/// dashboard while connected).
pub fn touch_last_seen(db: &Db, client_id: &str) -> Result<bool> {
    let conn = db.lock().unwrap();
    let updated = conn.execute(
        "UPDATE clients SET last_seen_at = ?1 WHERE client_id = ?2",
        (protocol::ts(), client_id),
    )?;
    Ok(updated > 0)
}

/// `None` clears the friendly name so the dashboard falls back to the
/// hostname. Returns false if there's no such client.
pub fn set_friendly_name(db: &Db, client_id: &str, friendly_name: Option<&str>) -> Result<bool> {
    let conn = db.lock().unwrap();
    let updated = conn.execute(
        "UPDATE clients SET friendly_name = ?1 WHERE client_id = ?2",
        (friendly_name, client_id),
    )?;
    Ok(updated > 0)
}

/// Returns false if there's no such client. The pairing code that enrolled
/// it keeps its `claimed_by_client`, which just stops resolving.
pub fn delete_client(db: &Db, client_id: &str) -> Result<bool> {
    let conn = db.lock().unwrap();
    let deleted = conn.execute("DELETE FROM clients WHERE client_id = ?1", [client_id])?;
    Ok(deleted > 0)
}

/// Stores what a client reported in its `info` message. The hostname
/// overwrites the one from enrollment, so renamed machines update.
pub fn update_client_info(
    db: &Db,
    client_id: &str,
    hostname: &str,
    os: &str,
    client_version: &str,
    specs_json: &str,
) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute(
        "UPDATE clients SET hostname = ?1, os = ?2, client_version = ?3, specs_json = ?4, info_updated_at = ?5
         WHERE client_id = ?6",
        (hostname, os, client_version, specs_json, protocol::ts(), client_id),
    )?;
    Ok(())
}

pub struct ClientListRow {
    pub client_id: String,
    pub hostname: String,
    /// The hostname when no friendly name has been set.
    pub friendly_name: String,
    pub enrolled_at: String,
    pub last_seen_at: Option<String>,
    pub os: Option<String>,
    pub client_version: Option<String>,
    pub specs_json: Option<String>,
    pub info_updated_at: Option<String>,
}

/// Backs the future dashboard's client list. Connected/disconnected is
/// deliberately NOT part of this query — that's live state owned by the
/// in-memory connection map in main.rs, not something persisted to disk.
pub fn list_clients(db: &Db) -> Result<Vec<ClientListRow>> {
    let conn = db.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT client_id, hostname, COALESCE(friendly_name, hostname), enrolled_at, last_seen_at, os,
                client_version, specs_json, info_updated_at
         FROM clients ORDER BY enrolled_at DESC",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ClientListRow {
                client_id: r.get(0)?,
                hostname: r.get(1)?,
                friendly_name: r.get(2)?,
                enrolled_at: r.get(3)?,
                last_seen_at: r.get(4)?,
                os: r.get(5)?,
                client_version: r.get(6)?,
                specs_json: r.get(7)?,
                info_updated_at: r.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Polled by the "Add client" modal to find out whether its code has
/// been claimed yet, and by whom.
pub fn pairing_code_status(db: &Db, code: &str) -> Result<Option<(bool, Option<String>)>> {
    let conn = db.lock().unwrap();
    let result = conn
        .query_row(
            "SELECT used_at IS NOT NULL, claimed_by_client FROM pairing_codes WHERE code = ?1",
            [code],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    Ok(result)
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_adds_columns_to_an_old_database_and_is_repeatable() {
        let dir = std::env::temp_dir().join(format!("delegation-db-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        let _ = std::fs::remove_file(&path);
        {
            // The schema as it was before info/specs existed.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE clients (client_id TEXT PRIMARY KEY, public_key TEXT NOT NULL,
                 hostname TEXT NOT NULL, enrolled_at TEXT NOT NULL, last_seen_at TEXT);
                 INSERT INTO clients VALUES ('c1', 'pk', 'old-name', '2026-01-01T00:00:00Z', NULL);",
            )
            .unwrap();
        }
        let db = open(path.to_str().unwrap()).unwrap();
        drop(db);
        let db = open(path.to_str().unwrap()).unwrap();

        let rows = list_clients(&db).unwrap();
        assert_eq!(rows[0].hostname, "old-name");
        assert!(rows[0].specs_json.is_none());

        update_client_info(&db, "c1", "new-name", "Debian 13", "0.1.0", "{}").unwrap();
        let rows = list_clients(&db).unwrap();
        assert_eq!(rows[0].hostname, "new-name");
        assert_eq!(rows[0].os.as_deref(), Some("Debian 13"));
        assert!(rows[0].info_updated_at.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_db(name: &str) -> (Db, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("delegation-db-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.db");
        let _ = std::fs::remove_file(&path);
        let db = open(path.to_str().unwrap()).unwrap();
        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO clients (client_id, public_key, hostname, enrolled_at) VALUES ('c1', 'pk', 'host', 'now')",
                [],
            )
            .unwrap();
        (db, dir)
    }

    #[test]
    fn friendly_name_defaults_to_and_falls_back_to_hostname() {
        let (db, dir) = test_db("friendly");
        assert_eq!(list_clients(&db).unwrap()[0].friendly_name, "host");

        assert!(set_friendly_name(&db, "c1", Some("Gaming PC")).unwrap());
        update_client_info(&db, "c1", "renamed-host", "os", "0.1.0", "{}").unwrap();
        let row = &list_clients(&db).unwrap()[0];
        assert_eq!(row.friendly_name, "Gaming PC");
        assert_eq!(row.hostname, "renamed-host");

        assert!(set_friendly_name(&db, "c1", None).unwrap());
        assert_eq!(list_clients(&db).unwrap()[0].friendly_name, "renamed-host");
        assert!(!set_friendly_name(&db, "nope", Some("x")).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_client_removes_it_and_stops_heartbeats_matching() {
        let (db, dir) = test_db("delete");
        assert!(touch_last_seen(&db, "c1").unwrap());
        assert!(delete_client(&db, "c1").unwrap());
        assert!(list_clients(&db).unwrap().is_empty());
        assert!(!touch_last_seen(&db, "c1").unwrap());
        assert!(!delete_client(&db, "c1").unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
