//! SQLite persistence. `rusqlite`'s `bundled` feature compiles SQLite from
//! source as part of the Rust build, so there's no system SQLite dependency.
//!
//! There is one connection, behind a mutex. Every query runs through
//! [`Db::call`], which moves it onto tokio's blocking pool, so a slow write
//! never stalls the async worker threads serving the WebSockets and API.
//!
//! The schema is versioned with `PRAGMA user_version` and created in one go
//! on an empty database. There are no migrations: a database from an older
//! build is refused at startup with instructions to reset it (the project
//! is greenfield, so nothing in it needs to survive).

use crate::conversations::{MessageRole, MessageStatus};
use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Bump whenever the schema below changes. Older databases are refused.
pub const SCHEMA_VERSION: i64 = 1;

/// How long a query waits on a lock held by another connection (the admin
/// CLI, a backup) before giving up.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    /// Runs `f` with the connection on the blocking pool.
    pub async fn call<R, F>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut Connection) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            // A panic mid-query leaves the lock poisoned but the connection
            // fine (an open transaction rolls back when dropped).
            let mut conn = conn.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut conn)
        })
        .await
        .context("database task panicked")?
    }

    /// Runs `f` on the current thread. Only for code that isn't on an async
    /// worker: the CLI subcommands, shutdown, and tests.
    pub fn blocking<R>(&self, f: impl FnOnce(&mut Connection) -> Result<R>) -> Result<R> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut conn)
    }
}

/// Opens (creating if needed) the database at `path`, sets the pragmas and
/// checks the schema version. `:memory:` gives a private in-memory database.
pub fn open(path: &str) -> Result<Db> {
    let conn = Connection::open(path).with_context(|| format!("opening sqlite database {path}"))?;
    configure(&conn)?;
    ensure_schema(&conn, path)?;
    Ok(Db {
        conn: Arc::new(Mutex::new(conn)),
    })
}

fn configure(conn: &Connection) -> Result<()> {
    // WAL lets the backup command and the admin CLI read while the server
    // writes. journal_mode answers with the mode it ended up in (an
    // in-memory database stays "memory").
    let _mode: String = conn
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .context("setting journal_mode")?;
    // SQLite ships with foreign keys off; every cascade in the schema
    // depends on this.
    conn.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")
        .context("setting pragmas")?;
    conn.busy_timeout(BUSY_TIMEOUT).context("setting busy_timeout")?;
    let fk: bool = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
    if !fk {
        bail!("this SQLite build doesn't support foreign keys");
    }
    Ok(())
}

fn ensure_schema(conn: &Connection, path: &str) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let tables: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    )?;
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    if version == 0 && tables == 0 {
        conn.execute_batch(&format!(
            "BEGIN;\n{}\nPRAGMA user_version = {SCHEMA_VERSION};\nCOMMIT;",
            schema()
        ))
        .context("creating the schema")?;
        return Ok(());
    }
    let found = if version == 0 {
        "an unversioned schema from before auth and chat persistence".to_string()
    } else {
        format!("schema version {version}")
    };
    if version > SCHEMA_VERSION {
        bail!(
            "the database at {path} has {found}, newer than this build understands (version {SCHEMA_VERSION}). \
             Run a newer server, or restore a backup made by this one."
        );
    }
    bail!(
        "the database at {path} has {found}, but this build needs schema version {SCHEMA_VERSION}. \
         There are no migrations (the project is greenfield): stop the server, delete {path} \
         (and {path}-wal and {path}-shm if present), and start it again. Every client will then need \
         to re-enrol with a new pairing code."
    );
}

/// The whole schema. Statuses and roles come from their Rust enums, so the
/// CHECK constraints can't drift from the code.
fn schema() -> String {
    let statuses = sql_list(MessageStatus::ALL.iter().map(|s| s.as_str()));
    let roles = sql_list(MessageRole::ALL.iter().map(|r| r.as_str()));
    let active = sql_list(MessageStatus::ACTIVE.iter().map(|s| s.as_str()));
    format!(
        "
        CREATE TABLE users (
          id                   TEXT PRIMARY KEY,
          username             TEXT NOT NULL UNIQUE COLLATE NOCASE,
          password_hash        TEXT NOT NULL,
          role                 TEXT NOT NULL CHECK (role IN ('admin','user')),
          created_at           TEXT NOT NULL,
          password_changed_at  TEXT NOT NULL,
          disabled_at          TEXT
        );
        -- Exactly one admin per deployment, enforced by the database.
        CREATE UNIQUE INDEX one_admin ON users(role) WHERE role = 'admin';

        CREATE TABLE sessions (
          token_hash    TEXT PRIMARY KEY,
          user_id       TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
          created_at    TEXT NOT NULL,
          last_seen_at  TEXT NOT NULL,
          expires_at    TEXT NOT NULL,
          persistent    INTEGER NOT NULL,
          user_agent    TEXT,
          ip            TEXT
        );
        CREATE INDEX sessions_by_user ON sessions(user_id);

        CREATE TABLE password_resets (
          id          TEXT PRIMARY KEY,
          token_hash  TEXT NOT NULL UNIQUE,
          user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
          created_by  TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
          created_at  TEXT NOT NULL,
          expires_at  TEXT NOT NULL,
          used_at     TEXT
        );
        CREATE INDEX password_resets_by_user ON password_resets(user_id);

        CREATE TABLE clients (
          client_id        TEXT PRIMARY KEY,
          owner_user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
          public_key       TEXT NOT NULL,
          hostname         TEXT NOT NULL,
          -- Set from the dashboard. NULL means use the hostname, so an
          -- unrenamed client keeps following hostname changes.
          friendly_name    TEXT,
          enrolled_at      TEXT NOT NULL,
          last_seen_at     TEXT,
          os               TEXT,
          client_version   TEXT,
          specs_json       TEXT,
          info_updated_at  TEXT
        );
        CREATE INDEX clients_by_owner ON clients(owner_user_id);

        CREATE TABLE pairing_codes (
          code               TEXT PRIMARY KEY,
          created_by         TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
          expires_at         INTEGER NOT NULL,
          used_at            INTEGER,
          claimed_by_client  TEXT
        );

        CREATE TABLE conversations (
          id              TEXT PRIMARY KEY,
          owner_user_id   TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
          title           TEXT NOT NULL,
          active_leaf_id  TEXT,
          created_at      TEXT NOT NULL,
          updated_at      TEXT NOT NULL
        );
        CREATE INDEX conversations_by_owner ON conversations(owner_user_id, updated_at DESC);

        CREATE TABLE messages (
          id               TEXT PRIMARY KEY,
          conversation_id  TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
          parent_id        TEXT REFERENCES messages(id) ON DELETE CASCADE,
          role             TEXT NOT NULL CHECK (role IN ({roles})),
          content          TEXT NOT NULL,
          status           TEXT NOT NULL CHECK (status IN ({statuses})),
          error            TEXT,
          attempt_id       TEXT,
          attempts         INTEGER NOT NULL DEFAULT 0,
          client_id        TEXT REFERENCES clients(client_id) ON DELETE SET NULL,
          client_label     TEXT,
          model            TEXT,
          created_at       TEXT NOT NULL,
          completed_at     TEXT,
          meta_json        TEXT
        );
        CREATE INDEX messages_by_parent ON messages(conversation_id, parent_id, created_at);
        -- Small: only replies being generated. Backs the per-conversation
        -- and per-user limits.
        CREATE INDEX messages_active ON messages(conversation_id) WHERE status IN ({active});
        -- So deleting a client (ON DELETE SET NULL) doesn't scan every message.
        CREATE INDEX messages_by_client ON messages(client_id);
        "
    )
}

fn sql_list<'a>(items: impl Iterator<Item = &'a str>) -> String {
    items.map(|s| format!("'{s}'")).collect::<Vec<_>>().join(",")
}

/// `GET /api/health`: the database answers.
pub fn ping(conn: &Connection) -> Result<()> {
    conn.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))?;
    Ok(())
}

/// A consistent copy of the live database, safe while the server runs
/// (unlike copying the file, which can catch it half-written). `dest` must
/// not exist yet.
pub fn backup(conn: &Connection, dest: &str) -> Result<()> {
    if std::path::Path::new(dest).exists() {
        bail!("{dest} already exists; choose a new file name");
    }
    conn.execute("VACUUM INTO ?1", [dest])
        .with_context(|| format!("writing a backup to {dest}"))?;
    Ok(())
}

// ---------------- timestamps ----------------
//
// Every timestamp the auth and chat tables store is RFC 3339 UTC with
// milliseconds ("2026-11-08T10:15:00.123Z"): fixed width, so SQL can compare
// and sort them as text, and `parse_ts` reads them back.

pub fn ts(time: SystemTime) -> String {
    protocol::utc_timestamp(time, true)
}

pub fn now_ts() -> String {
    ts(SystemTime::now())
}

/// Inverse of [`ts`] (also accepts the no-millisecond form).
pub fn parse_ts(s: &str) -> Option<SystemTime> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut date = date.splitn(3, '-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: u32 = date.next()?.parse().ok()?;
    let day: u32 = date.next()?.parse().ok()?;
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut hms = hms.splitn(3, ':');
    let hh: u64 = hms.next()?.parse().ok()?;
    let mm: u64 = hms.next()?.parse().ok()?;
    let ss: u64 = hms.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let millis: u64 = match frac.len() {
        0 => 0,
        1..=3 => format!("{frac:0<3}").parse().ok()?,
        _ => frac[..3].parse().ok()?,
    };
    let days = days_from_civil(year, month, day);
    let secs = u64::try_from(days).ok()? * 86_400 + hh * 3600 + mm * 60 + ss;
    Some(UNIX_EPOCH + Duration::from_secs(secs) + Duration::from_millis(millis))
}

/// Days since 1970-01-01 (Howard Hinnant's days_from_civil, the inverse of
/// the civil_from_days in `protocol::utc_timestamp`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = i64::from((month + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Whether a stored timestamp is in the past. Unparseable counts as past,
/// so a corrupt expiry fails closed.
pub fn is_past(stored: &str, now: SystemTime) -> bool {
    parse_ts(stored).is_none_or(|t| t <= now)
}

// ---------------- clients and pairing codes ----------------

pub struct ClientRecord {
    pub client_id: String,
    pub owner_user_id: String,
    pub public_key: String,
    pub hostname: String,
}

/// A pairing code created by `created_by`, valid for `ttl_secs`. Whoever
/// enrolls with it becomes a client owned by `created_by`.
pub fn create_pairing_code(conn: &Connection, code: &str, created_by: &str, ttl_secs: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO pairing_codes (code, created_by, expires_at) VALUES (?1, ?2, ?3)",
        (code, created_by, now_epoch() + ttl_secs),
    )?;
    Ok(())
}

pub enum EnrollOutcome {
    Enrolled { owner_user_id: String },
    InvalidCode,
}

/// Validates a pairing code and, if valid, enrolls the client in one
/// transaction, owned by the code's creator.
///
/// A code is marked used on first use whether or not that use succeeds, so
/// it can't be brute-forced by retrying nearby guesses. The row isn't
/// deleted: `claimed_by_client` lets the dashboard's "Add client" dialog
/// poll whether its code has been used yet, and by whom.
pub fn try_enroll(
    conn: &mut Connection,
    code: &str,
    client_id: &str,
    public_key: &str,
    hostname: &str,
) -> Result<EnrollOutcome> {
    let tx = conn.transaction()?;
    let row: Option<(i64, Option<i64>, String)> = tx
        .query_row(
            "SELECT expires_at, used_at, created_by FROM pairing_codes WHERE code = ?1",
            [code],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((expires_at, used_at, created_by)) = row else {
        return Ok(EnrollOutcome::InvalidCode);
    };
    if used_at.is_some() {
        return Ok(EnrollOutcome::InvalidCode);
    }
    // Exactly one shot, even if expiry also fails it.
    tx.execute(
        "UPDATE pairing_codes SET used_at = ?1 WHERE code = ?2",
        (now_epoch(), code),
    )?;
    if expires_at < now_epoch() {
        tx.commit()?;
        return Ok(EnrollOutcome::InvalidCode);
    }
    tx.execute(
        "INSERT INTO clients (client_id, owner_user_id, public_key, hostname, enrolled_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        (client_id, &created_by, public_key, hostname, protocol::ts()),
    )?;
    tx.execute(
        "UPDATE pairing_codes SET claimed_by_client = ?1 WHERE code = ?2",
        (client_id, code),
    )?;
    tx.commit()?;
    Ok(EnrollOutcome::Enrolled {
        owner_user_id: created_by,
    })
}

pub fn get_client(conn: &Connection, client_id: &str) -> Result<Option<ClientRecord>> {
    Ok(conn
        .query_row(
            "SELECT client_id, owner_user_id, public_key, hostname FROM clients WHERE client_id = ?1",
            [client_id],
            |r| {
                Ok(ClientRecord {
                    client_id: r.get(0)?,
                    owner_user_id: r.get(1)?,
                    public_key: r.get(2)?,
                    hostname: r.get(3)?,
                })
            },
        )
        .optional()?)
}

/// Writes in-memory `last_seen_at` values, given as (client_id, timestamp),
/// in one transaction. Returns the ids that matched no row: clients deleted
/// since they last connected.
pub fn write_last_seen(conn: &mut Connection, rows: &[(String, String)]) -> Result<Vec<String>> {
    let tx = conn.transaction()?;
    let mut missing = Vec::new();
    {
        let mut stmt = tx.prepare_cached("UPDATE clients SET last_seen_at = ?1 WHERE client_id = ?2")?;
        for (client_id, at) in rows {
            if stmt.execute((at, client_id))? == 0 {
                missing.push(client_id.clone());
            }
        }
    }
    tx.commit()?;
    Ok(missing)
}

/// `None` clears the friendly name so the dashboard falls back to the
/// hostname. Returns false if there's no such client.
pub fn set_friendly_name(conn: &Connection, client_id: &str, friendly_name: Option<&str>) -> Result<bool> {
    let updated = conn.execute(
        "UPDATE clients SET friendly_name = ?1 WHERE client_id = ?2",
        (friendly_name, client_id),
    )?;
    Ok(updated > 0)
}

/// Returns false if there's no such client. Replies it gave keep their
/// `client_label`; their `client_id` becomes NULL.
pub fn delete_client(conn: &Connection, client_id: &str) -> Result<bool> {
    Ok(conn.execute("DELETE FROM clients WHERE client_id = ?1", [client_id])? > 0)
}

/// Stores what a client reported in its `info` message. The hostname
/// overwrites the one from enrollment, so renamed machines update.
pub fn update_client_info(
    conn: &Connection,
    client_id: &str,
    hostname: &str,
    os: &str,
    client_version: &str,
    specs_json: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE clients SET hostname = ?1, os = ?2, client_version = ?3, specs_json = ?4, info_updated_at = ?5
         WHERE client_id = ?6",
        (hostname, os, client_version, specs_json, protocol::ts(), client_id),
    )?;
    Ok(())
}

/// One client as the dashboard lists it. Which rows a user gets is decided
/// in `access.rs`, never here.
#[derive(Debug, Clone)]
pub struct ClientListRow {
    pub client_id: String,
    pub owner_user_id: String,
    pub owner_username: String,
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

/// The columns `ClientListRow::from_row` reads, in order, from `clients c
/// JOIN users u ON u.id = c.owner_user_id`.
pub const CLIENT_LIST_COLUMNS: &str = "c.client_id, c.owner_user_id, u.username, c.hostname,
    COALESCE(c.friendly_name, c.hostname), c.enrolled_at, c.last_seen_at, c.os, c.client_version,
    c.specs_json, c.info_updated_at";

impl ClientListRow {
    pub fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(ClientListRow {
            client_id: r.get(0)?,
            owner_user_id: r.get(1)?,
            owner_username: r.get(2)?,
            hostname: r.get(3)?,
            friendly_name: r.get(4)?,
            enrolled_at: r.get(5)?,
            last_seen_at: r.get(6)?,
            os: r.get(7)?,
            client_version: r.get(8)?,
            specs_json: r.get(9)?,
            info_updated_at: r.get(10)?,
        })
    }
}

pub struct PairingCodeStatus {
    pub created_by: String,
    pub used: bool,
    pub claimed_by: Option<String>,
}

/// Polled by the "Add client" dialog to find out whether its code has been
/// claimed yet, and by whom.
pub fn pairing_code_status(conn: &Connection, code: &str) -> Result<Option<PairingCodeStatus>> {
    Ok(conn
        .query_row(
            "SELECT created_by, used_at IS NOT NULL, claimed_by_client FROM pairing_codes WHERE code = ?1",
            [code],
            |r| {
                Ok(PairingCodeStatus {
                    created_by: r.get(0)?,
                    used: r.get(1)?,
                    claimed_by: r.get(2)?,
                })
            },
        )
        .optional()?)
}

/// Pairing codes are only useful for `PAIRING_CODE_TTL_SECS`; keep a day of
/// them for the dialog's status polling, then let them go.
pub fn delete_old_pairing_codes(conn: &Connection) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM pairing_codes WHERE expires_at < ?1",
        [now_epoch() - 86_400],
    )?)
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A fresh in-memory database with the full schema.
    pub fn memory_db() -> Db {
        open(":memory:").unwrap()
    }

    /// Inserts a user directly (no password hashing), returning its id.
    pub fn add_user(db: &Db, username: &str, role: &str) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        db.blocking(|c| {
            c.execute(
                "INSERT INTO users (id, username, password_hash, role, created_at, password_changed_at)
                 VALUES (?1, ?2, 'x', ?3, ?4, ?4)",
                (&id, username, role, now_ts()),
            )?;
            Ok(())
        })
        .unwrap();
        id
    }

    pub fn add_client(db: &Db, client_id: &str, owner: &str) {
        db.blocking(|c| {
            c.execute(
                "INSERT INTO clients (client_id, owner_user_id, public_key, hostname, enrolled_at)
                 VALUES (?1, ?2, 'pk', 'host', 'now')",
                (client_id, owner),
            )?;
            Ok(())
        })
        .unwrap();
    }

    fn temp_path(name: &str) -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("delegation-db-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.db").to_str().unwrap().to_string();
        (dir, path)
    }

    #[test]
    fn pragmas_are_set_and_the_schema_is_versioned() {
        let (dir, path) = temp_path("pragmas");
        let db = open(&path).unwrap();
        db.blocking(|c| {
            let mode: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
            assert_eq!(mode, "wal");
            let fk: bool = c.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
            assert!(fk);
            let sync: i64 = c.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
            assert_eq!(sync, 1, "NORMAL");
            let version: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            assert_eq!(version, SCHEMA_VERSION);
            Ok(())
        })
        .unwrap();
        drop(db);
        // Reopening a current database is fine.
        open(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_or_unversioned_database_is_refused() {
        let (dir, path) = temp_path("old");
        {
            // The schema before versioning.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE clients (client_id TEXT PRIMARY KEY, public_key TEXT NOT NULL,
                 hostname TEXT NOT NULL, enrolled_at TEXT NOT NULL, last_seen_at TEXT);",
            )
            .unwrap();
        }
        let err = open(&path).err().expect("refused").to_string();
        assert!(err.contains("delete"), "explains the reset: {err}");

        let (dir2, path2) = temp_path("older-version");
        {
            let conn = Connection::open(&path2).unwrap();
            conn.execute_batch("CREATE TABLE t (x); PRAGMA user_version = 0;")
                .unwrap();
            conn.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
                .unwrap();
        }
        assert!(open(&path2).err().unwrap().to_string().contains("newer"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn foreign_keys_cascade() {
        let db = memory_db();
        let alice = add_user(&db, "alice", "user");
        add_client(&db, "c1", &alice);
        db.blocking(|c| {
            create_pairing_code(c, "CODE", &alice, 60)?;
            c.execute("DELETE FROM users WHERE id = ?1", [&alice])?;
            let clients: i64 = c.query_row("SELECT COUNT(*) FROM clients", [], |r| r.get(0))?;
            let codes: i64 = c.query_row("SELECT COUNT(*) FROM pairing_codes", [], |r| r.get(0))?;
            assert_eq!((clients, codes), (0, 0));
            // A client must have an owner that exists.
            assert!(c
                .execute(
                    "INSERT INTO clients (client_id, owner_user_id, public_key, hostname, enrolled_at)
                     VALUES ('c2', 'nobody', 'pk', 'h', 'now')",
                    [],
                )
                .is_err());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn enrolling_makes_the_code_creator_the_owner_and_burns_the_code() {
        let db = memory_db();
        let alice = add_user(&db, "alice", "user");
        db.blocking(|c| {
            create_pairing_code(c, "GOOD", &alice, 60)?;
            create_pairing_code(c, "OLD", &alice, -1)?;
            match try_enroll(c, "GOOD", "c1", "pk", "host")? {
                EnrollOutcome::Enrolled { owner_user_id } => assert_eq!(owner_user_id, alice),
                EnrollOutcome::InvalidCode => panic!("should enroll"),
            }
            assert_eq!(get_client(c, "c1")?.unwrap().owner_user_id, alice);
            assert!(matches!(
                try_enroll(c, "GOOD", "c2", "pk", "h")?,
                EnrollOutcome::InvalidCode
            ));
            assert!(matches!(
                try_enroll(c, "OLD", "c3", "pk", "h")?,
                EnrollOutcome::InvalidCode
            ));
            assert!(matches!(
                try_enroll(c, "NOPE", "c4", "pk", "h")?,
                EnrollOutcome::InvalidCode
            ));
            let status = pairing_code_status(c, "GOOD")?.unwrap();
            assert_eq!(
                (status.created_by.as_str(), status.used, status.claimed_by.as_deref()),
                (alice.as_str(), true, Some("c1"))
            );
            let status = pairing_code_status(c, "OLD")?.unwrap();
            assert!(
                status.used && status.claimed_by.is_none(),
                "an expired code is burnt too"
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn friendly_name_and_info_updates() {
        let db = memory_db();
        let alice = add_user(&db, "alice", "user");
        add_client(&db, "c1", &alice);
        db.blocking(|c| {
            let name = |c: &Connection| -> String {
                c.query_row(
                    "SELECT COALESCE(friendly_name, hostname) FROM clients WHERE client_id = 'c1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
            };
            assert_eq!(name(c), "host");
            assert!(set_friendly_name(c, "c1", Some("Gaming PC"))?);
            update_client_info(c, "c1", "renamed-host", "os", "0.1.0", "{}")?;
            assert_eq!(name(c), "Gaming PC");
            assert!(set_friendly_name(c, "c1", None)?);
            assert_eq!(name(c), "renamed-host");
            assert!(!set_friendly_name(c, "nope", Some("x"))?);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn last_seen_flush_writes_rows_and_reports_deleted_clients() {
        let db = memory_db();
        let alice = add_user(&db, "alice", "user");
        add_client(&db, "c1", &alice);
        let rows = vec![
            ("c1".to_string(), "2026-11-06T12:00:00Z".to_string()),
            ("gone".to_string(), "2026-11-06T12:00:00Z".to_string()),
        ];
        db.blocking(|c| {
            assert_eq!(write_last_seen(c, &rows)?, vec!["gone".to_string()]);
            assert!(delete_client(c, "c1")?);
            assert_eq!(write_last_seen(c, &rows[..1])?, vec!["c1".to_string()]);
            assert!(!delete_client(c, "c1")?);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn timestamps_round_trip_and_compare_as_text() {
        let t = UNIX_EPOCH + Duration::from_millis(1_793_966_468_412);
        assert_eq!(ts(t), "2026-11-06T12:01:08.412Z");
        assert_eq!(parse_ts("2026-11-06T12:01:08.412Z"), Some(t));
        assert_eq!(
            parse_ts("2026-11-06T12:01:08Z"),
            Some(UNIX_EPOCH + Duration::from_secs(1_793_966_468))
        );
        for millis in [0u64, 951_782_400_000, 1_709_164_800_123, 4_102_444_799_999] {
            let t = UNIX_EPOCH + Duration::from_millis(millis);
            assert_eq!(parse_ts(&ts(t)), Some(t), "{}", ts(t));
        }
        assert!(ts(t) < ts(t + Duration::from_millis(1)));
        assert_eq!(parse_ts("garbage"), None);
        assert_eq!(parse_ts("2026-13-01T00:00:00Z"), None);
        assert!(is_past("garbage", SystemTime::now()));
        assert!(!is_past(
            &ts(SystemTime::now() + Duration::from_secs(60)),
            SystemTime::now()
        ));
    }

    #[test]
    fn backup_produces_a_database_with_the_rows() {
        let (dir, path) = temp_path("backup");
        let db = open(&path).unwrap();
        let alice = add_user(&db, "alice", "user");
        add_client(&db, "c1", &alice);
        let dest = dir.join("backup.db").to_str().unwrap().to_string();
        db.blocking(|c| backup(c, &dest)).unwrap();
        assert!(db.blocking(|c| backup(c, &dest)).is_err(), "won't overwrite");

        let copy = open(&dest).unwrap();
        copy.blocking(|c| {
            let users: i64 = c.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
            let clients: i64 = c.query_row("SELECT COUNT(*) FROM clients", [], |r| r.get(0))?;
            assert_eq!((users, clients), (1, 1));
            Ok(())
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
