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
    Ok(Arc::new(Mutex::new(conn)))
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
pub fn try_enroll(
    db: &Db,
    code: &str,
    client_id: &str,
    public_key: &str,
    hostname: &str,
) -> Result<EnrollOutcome> {
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

pub fn touch_last_seen(db: &Db, client_id: &str) -> Result<()> {
    let conn = db.lock().unwrap();
    conn.execute(
        "UPDATE clients SET last_seen_at = ?1 WHERE client_id = ?2",
        (protocol::ts(), client_id),
    )?;
    Ok(())
}

pub struct ClientListRow {
    pub client_id: String,
    pub hostname: String,
    pub enrolled_at: String,
    pub last_seen_at: Option<String>,
}

/// Backs the future dashboard's client list. Connected/disconnected is
/// deliberately NOT part of this query — that's live state owned by the
/// in-memory connection map in main.rs, not something persisted to disk.
pub fn list_clients(db: &Db) -> Result<Vec<ClientListRow>> {
    let conn = db.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT client_id, hostname, enrolled_at, last_seen_at FROM clients ORDER BY enrolled_at DESC",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ClientListRow {
                client_id: r.get(0)?,
                hostname: r.get(1)?,
                enrolled_at: r.get(2)?,
                last_seen_at: r.get(3)?,
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
