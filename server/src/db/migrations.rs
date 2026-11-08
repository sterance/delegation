//! Schema migrations: an ordered, forward-only list, applied on startup.
//!
//! `PRAGMA user_version` holds the version of the last migration applied.
//! Opening a database brings it up to [`SCHEMA_VERSION`], one migration per
//! transaction, after writing a backup next to it. See `server/DATABASE.md`
//! for the workflow and for restoring a backup.
//!
//! # Changing the schema
//!
//! Append a migration to [`MIGRATIONS`] with the next version number.
//!
//! - **Never edit a migration that has been deployed**, including its SQL.
//!   Databases that already ran it won't run it again, so an edit only
//!   reaches new databases and the two drift apart. Fix mistakes in a new
//!   migration.
//! - Set `min_compatible`: the oldest schema version whose code still runs
//!   correctly against the database after this migration. **Additive**
//!   changes (new tables, new nullable or defaulted columns, new indexes)
//!   copy the previous migration's value, so a rollback to the previous build
//!   keeps working. **Breaking** changes (dropping or renaming a column or
//!   table, tightening a constraint, changing what a value means) set it to
//!   their own version, so older builds refuse the database instead of
//!   misreading it. Prefer additive changes; split a breaking change across
//!   two releases where practical (add the new column and write both, then
//!   drop the old one in a later release).
//! - The `CHECK` lists for message statuses and roles are literals here, not
//!   generated from the Rust enums. Adding a variant to `MessageStatus`,
//!   `MessageRole` or `Role` needs a table-rebuild migration (below); a test
//!   fails until there is one.
//!
//! # Table rebuilds
//!
//! SQLite can't change a column's definition or a `CHECK` constraint in
//! place. Rebuild the table instead, with `rebuilds_tables: true` (the runner
//! then turns foreign keys off around the transaction and runs
//! `PRAGMA foreign_key_check` before committing):
//!
//! ```sql
//! CREATE TABLE messages_new ( ...the full new definition... );
//! INSERT INTO messages_new (col_a, col_b, ...) SELECT col_a, col_b, ... FROM messages;
//! DROP TABLE messages;
//! ALTER TABLE messages_new RENAME TO messages;
//! -- Dropping the table dropped its indexes (and triggers): recreate them.
//! CREATE INDEX messages_by_parent ON messages(conversation_id, parent_id, created_at);
//! ```
//!
//! Foreign keys in other tables (and the table's own, like
//! `messages.parent_id`) refer to it by name, so they point at the rebuilt
//! table once it has the old name. Copy every column, keep the primary keys,
//! and list columns explicitly in the `INSERT` so a reordered definition
//! can't shuffle data.

use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Copy)]
pub struct Migration {
    /// 1, 2, 3 … with no gaps.
    pub version: i64,
    /// Short description for logs and errors.
    pub name: &'static str,
    /// The oldest code schema version that can still run against a database
    /// at this version. Additive changes leave it alone; breaking ones raise
    /// it to their own version.
    pub min_compatible: i64,
    /// Table rebuilds need foreign keys off during the migration.
    pub rebuilds_tables: bool,
    pub apply: fn(&Transaction) -> Result<()>,
}

// NEVER EDIT A MIGRATION THAT HAS BEEN DEPLOYED. Fixes go in a new one.
// Append only; versions are consecutive from 1.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial schema (auth, clients, chat)",
        min_compatible: 1,
        rebuilds_tables: false,
        apply: |tx| sql(tx, V1_INITIAL),
    },
    Migration {
        version: 2,
        name: "add schema_meta",
        min_compatible: 1,
        rebuilds_tables: false,
        apply: |tx| {
            sql(
                tx,
                "CREATE TABLE schema_meta (
                   id                      INTEGER PRIMARY KEY CHECK (id = 1),
                   -- The oldest code schema version that can run against this
                   -- database. Read by builds older than the database (after
                   -- a rollback) to decide whether they can start.
                   min_compatible_version  INTEGER NOT NULL
                 );",
            )
        },
    },
];

/// The schema version this build creates and expects: the last migration's.
pub const SCHEMA_VERSION: i64 = MIGRATIONS[MIGRATIONS.len() - 1].version;

fn sql(tx: &Transaction, sql: &str) -> Result<()> {
    tx.execute_batch(sql)?;
    Ok(())
}

/// Version 1, frozen: exactly what `schema()` generated when migrations were
/// introduced, which is what the live version-1 databases were created
/// with. The whitespace is part of what SQLite stores in `sqlite_schema`.
const V1_INITIAL: &str = r#"
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
          role             TEXT NOT NULL CHECK (role IN ('system','user','assistant')),
          content          TEXT NOT NULL,
          status           TEXT NOT NULL CHECK (status IN ('queued','assigned','running','completed','failed','cancelled','orphaned')),
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
        CREATE INDEX messages_active ON messages(conversation_id) WHERE status IN ('queued','assigned','running');
        -- So deleting a client (ON DELETE SET NULL) doesn't scan every message.
        CREATE INDEX messages_by_client ON messages(client_id);
        "#;

/// Brings the database at `path` to the newest version in `migrations`, or
/// refuses (see the cases below). `backups_keep` is how many pre-migration
/// backups to keep.
pub fn ensure_schema(conn: &mut Connection, path: &str, migrations: &[Migration], backups_keep: usize) -> Result<()> {
    let latest = migrations.last().map_or(0, |m| m.version);
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let tables: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        [],
        |r| r.get(0),
    )?;

    if version == latest {
        return Ok(());
    }
    if version == 0 && tables > 0 {
        bail!(
            "the database at {path} has an unversioned schema from before auth and chat persistence, which \
             can't be upgraded. Stop the server, delete {path} (and {path}-wal and {path}-shm if present), and \
             start it again. Every client will then need to re-enrol with a new pairing code."
        );
    }
    if version > latest {
        return check_newer(conn, path, version, latest);
    }

    // Behind: back up (unless it's brand new), then apply what's pending.
    let backup = if version > 0 && is_file(path) {
        Some(write_backup(conn, path, latest, backups_keep)?)
    } else {
        None
    };
    for m in migrations.iter().filter(|m| m.version > version) {
        crate::log(format!("[db] applying migration {} ({})", m.version, m.name));
        if let Err(e) = apply(conn, m) {
            let at: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(-1);
            let restore = match &backup {
                Some(b) => format!(
                    "The backup taken before migrating is {}; to go back to it, see \"Restoring a backup\" in \
                     server/DATABASE.md.",
                    b.display()
                ),
                None => "There was no data to back up.".to_string(),
            };
            bail!(
                "migration {} ({}) failed: {e:#}. The database at {path} is still at schema version {at} \
                 (nothing from the failed migration was kept). {restore}",
                m.version,
                m.name
            );
        }
    }
    Ok(())
}

/// One migration and its `user_version` (and `schema_meta`) update, in one
/// transaction.
fn apply(conn: &mut Connection, m: &Migration) -> Result<()> {
    if !m.rebuilds_tables {
        return apply_in_tx(conn, m);
    }
    // Can't be changed inside a transaction, so around it.
    conn.execute_batch("PRAGMA foreign_keys = OFF")?;
    let result = apply_in_tx(conn, m);
    let restored = conn.execute_batch("PRAGMA foreign_keys = ON");
    result?;
    restored.context("turning foreign keys back on")?;
    Ok(())
}

fn apply_in_tx(conn: &mut Connection, m: &Migration) -> Result<()> {
    let tx = conn.transaction()?;
    (m.apply)(&tx)?;
    if m.rebuilds_tables {
        let broken: Vec<String> = {
            let mut stmt = tx.prepare("PRAGMA foreign_key_check")?;
            let rows = stmt.query_map([], |r| {
                Ok(format!(
                    "{} row {} -> {}",
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<i64>>(1)?.map_or("?".into(), |id| id.to_string()),
                    r.get::<_, String>(2)?
                ))
            })?;
            rows.take(5).collect::<rusqlite::Result<_>>()?
        };
        if !broken.is_empty() {
            bail!("it left broken foreign key references ({})", broken.join(", "));
        }
    }
    if has_schema_meta(&tx)? {
        tx.execute(
            "INSERT INTO schema_meta (id, min_compatible_version) VALUES (1, ?1)
             ON CONFLICT (id) DO UPDATE SET min_compatible_version = excluded.min_compatible_version",
            [m.min_compatible],
        )?;
    }
    tx.pragma_update(None, "user_version", m.version)?;
    tx.commit()?;
    Ok(())
}

fn has_schema_meta(conn: &Connection) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'schema_meta'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// A database newer than this build, usually after a rollback: start if the
/// migrations it has are ones this build can live with.
fn check_newer(conn: &Connection, path: &str, version: i64, latest: i64) -> Result<()> {
    let min_compatible: Option<i64> = if has_schema_meta(conn)? {
        conn.query_row("SELECT min_compatible_version FROM schema_meta WHERE id = 1", [], |r| {
            r.get(0)
        })
        .optional()?
    } else {
        None
    };
    match min_compatible {
        Some(min) if latest >= min => {
            crate::log(format!(
                "[db] warning: the database is at schema version {version}, newer than this build (version \
                 {latest}). Its changes since are compatible with this build (it needs version {min} or later), \
                 so starting anyway."
            ));
            Ok(())
        }
        _ => {
            let needs = min_compatible.map_or("a newer build".to_string(), |min| format!("version {min} or later"));
            let backups = list_backups(path);
            let backups = if backups.is_empty() {
                "There are no pre-migration backups next to it.".to_string()
            } else {
                let names: Vec<String> = backups.iter().map(|b| b.display().to_string()).collect();
                format!(
                    "Pre-migration backups, oldest first: {}. To go back to one, see \"Restoring a backup\" in \
                     server/DATABASE.md.",
                    names.join(", ")
                )
            };
            bail!(
                "the database at {path} is at schema version {version}, which needs {needs}, but this build is \
                 schema version {latest}. Deploy a newer build, or restore a backup made before the upgrade. \
                 {backups}"
            )
        }
    }
}

// ---------------- pre-migration backups ----------------

fn is_file(path: &str) -> bool {
    path != ":memory:" && !path.is_empty() && Path::new(path).is_file()
}

/// `<db file name>.pre-v`, the start of every backup's file name.
fn backup_prefix(path: &str) -> (PathBuf, String) {
    let p = Path::new(path);
    let dir = p
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let file = p
        .file_name()
        .map_or("delegation.db".into(), |f| f.to_string_lossy().into_owned());
    (dir, format!("{file}.pre-v"))
}

/// `delegation.db.pre-v<target>-<UTC timestamp>.bak` next to the database,
/// then the oldest ones beyond `keep` are deleted.
fn write_backup(conn: &Connection, path: &str, target: i64, keep: usize) -> Result<PathBuf> {
    let (dir, prefix) = backup_prefix(path);
    // Fixed width and sortable: 20261108T101500.123Z.
    let stamp = protocol::utc_timestamp(SystemTime::now(), true).replace(['-', ':'], "");
    let dest = dir.join(format!("{prefix}{target}-{stamp}.bak"));
    super::backup(conn, &dest.to_string_lossy())
        .context("couldn't back up the database before migrating, so it wasn't migrated")?;
    crate::log(format!(
        "[db] backed up the database to {} before migrating",
        dest.display()
    ));
    for old in list_backups(path).iter().rev().skip(keep.max(1)) {
        match std::fs::remove_file(old) {
            Ok(()) => crate::log(format!("[db] deleted old backup {}", old.display())),
            Err(e) => crate::log(format!(
                "[db] warning: couldn't delete old backup {}: {e}",
                old.display()
            )),
        }
    }
    Ok(dest)
}

/// This database's pre-migration backups, oldest first.
pub fn list_backups(path: &str) -> Vec<PathBuf> {
    let (dir, prefix) = backup_prefix(path);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut found: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let rest = name.strip_prefix(&prefix)?.strip_suffix(".bak")?;
            // <target>-<timestamp>: sort on the timestamp, not the version.
            let (target, stamp) = rest.split_once('-')?;
            target.parse::<i64>().ok()?;
            Some((stamp.to_string(), e.path()))
        })
        .collect();
    found.sort();
    found.into_iter().map(|(_, p)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{configure, open};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("delegation-migrations-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A configured connection brought to the end of `migrations`.
    fn open_with(path: &str, migrations: &[Migration], keep: usize) -> Result<Connection> {
        let mut conn = Connection::open(path)?;
        configure(&conn)?;
        ensure_schema(&mut conn, path, migrations, keep)?;
        Ok(conn)
    }

    fn version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    fn schema_rows(conn: &Connection) -> String {
        let mut stmt = conn
            .prepare("SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name")
            .unwrap();
        let rows: Vec<String> = stmt
            .query_map([], |r| {
                Ok(format!(
                    "{}|{}|{}|{}",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?.unwrap_or_default()
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        rows.join("\n----\n")
    }

    /// One row in every version-1 table, with real references between them.
    fn fill_v1(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO users VALUES ('u1', 'alice', 'hash', 'admin', 't', 't', NULL);
             INSERT INTO users VALUES ('u2', 'bob', 'hash', 'user', 't', 't', NULL);
             INSERT INTO sessions VALUES ('tok', 'u1', 't', 't', 't', 1, 'ua', '1.2.3.4');
             INSERT INTO password_resets VALUES ('r1', 'rtok', 'u2', 'u1', 't', 't', NULL);
             INSERT INTO clients (client_id, owner_user_id, public_key, hostname, enrolled_at)
               VALUES ('c1', 'u1', 'pk', 'host', 't');
             INSERT INTO pairing_codes VALUES ('CODE', 'u1', 0, 0, 'c1');
             INSERT INTO conversations VALUES ('conv', 'u1', 'Hello', 'm2', 't', 't');
             INSERT INTO messages (id, conversation_id, parent_id, role, content, status, created_at)
               VALUES ('m1', 'conv', NULL, 'user', 'hi', 'completed', 't');
             INSERT INTO messages (id, conversation_id, parent_id, role, content, status, client_id, created_at)
               VALUES ('m2', 'conv', 'm1', 'assistant', 'hello', 'completed', 'c1', 't');",
        )
        .unwrap();
    }

    const V1_TABLES: [&str; 7] = [
        "users",
        "sessions",
        "password_resets",
        "clients",
        "pairing_codes",
        "conversations",
        "messages",
    ];

    fn sql_migration(version: i64, min_compatible: i64, apply: fn(&Transaction) -> Result<()>) -> Migration {
        Migration {
            version,
            name: "test migration",
            min_compatible,
            rebuilds_tables: false,
            apply,
        }
    }

    #[test]
    fn the_list_is_consecutive_and_min_compatible_only_rises() {
        let mut prev_min = 1;
        for (i, m) in MIGRATIONS.iter().enumerate() {
            assert_eq!(m.version, i as i64 + 1, "versions are 1, 2, 3 … with no gaps");
            assert!(
                (prev_min..=m.version).contains(&m.min_compatible),
                "migration {}: min_compatible must be between the previous one's ({prev_min}) and its own version",
                m.version
            );
            prev_min = m.min_compatible;
        }
        assert_eq!(SCHEMA_VERSION, MIGRATIONS.len() as i64);
    }

    #[test]
    fn migration_1_matches_the_schema_the_live_databases_were_created_with() {
        // schema_v1.snapshot is sqlite_schema from a database created by the
        // generated schema() that migration 1 replaced.
        let conn = open_with(":memory:", &MIGRATIONS[..1], 3).unwrap();
        assert_eq!(schema_rows(&conn), include_str!("schema_v1.snapshot"));
        assert_eq!(version(&conn), 1);
    }

    #[test]
    fn an_empty_database_gets_every_migration_and_schema_meta() {
        let dir = temp_dir("fresh");
        let path = dir.join("delegation.db").to_string_lossy().into_owned();
        let db = open(&path, 3).unwrap();
        db.blocking(|c| {
            assert_eq!(version(c), SCHEMA_VERSION);
            let min: i64 = c.query_row("SELECT min_compatible_version FROM schema_meta", [], |r| r.get(0))?;
            assert_eq!(min, MIGRATIONS.last().unwrap().min_compatible);
            Ok(())
        })
        .unwrap();
        assert!(list_backups(&path).is_empty(), "nothing to back up on a new database");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_schema_accepts_every_status_and_role_the_code_has() {
        use crate::conversations::{MessageRole, MessageStatus};
        let conn = open_with(":memory:", MIGRATIONS, 3).unwrap();
        fill_v1(&conn);
        let hint = "add a table-rebuild migration that extends the CHECK list";
        for status in MessageStatus::ALL {
            conn.execute(
                "INSERT INTO messages (id, conversation_id, role, content, status, created_at)
                 VALUES (?1, 'conv', 'assistant', '', ?2, 't')",
                (format!("s-{}", status.as_str()), status.as_str()),
            )
            .unwrap_or_else(|e| panic!("status {} is refused ({e}): {hint}", status.as_str()));
        }
        for role in MessageRole::ALL {
            conn.execute(
                "INSERT INTO messages (id, conversation_id, role, content, status, created_at)
                 VALUES (?1, 'conv', ?2, '', 'completed', 't')",
                (format!("r-{}", role.as_str()), role.as_str()),
            )
            .unwrap_or_else(|e| panic!("message role {} is refused ({e}): {hint}", role.as_str()));
        }
        use crate::auth::Role;
        // Exhaustive, so a new Role variant stops this compiling until it's
        // added to the list.
        let roles = [Role::Admin, Role::User].map(|r| match r {
            Role::Admin | Role::User => r,
        });
        for role in roles {
            let id = format!("x-{}", role.as_str());
            // A second admin trips the one-admin index, which is fine; only
            // the CHECK matters here.
            if let Err(e) = conn.execute(
                "INSERT INTO users VALUES (?1, ?1, 'h', ?2, 't', 't', NULL)",
                (&id, role.as_str()),
            ) {
                assert!(
                    !e.to_string().contains("CHECK"),
                    "user role {} is refused ({e}): {hint}",
                    role.as_str()
                );
            }
        }
        assert!(conn
            .execute("UPDATE messages SET status = 'bogus' WHERE id = 'm1'", [])
            .is_err());
    }

    #[test]
    fn a_version_1_database_is_backed_up_and_upgraded_with_its_rows() {
        let dir = temp_dir("upgrade");
        let path = dir.join("delegation.db").to_string_lossy().into_owned();
        {
            let conn = open_with(&path, &MIGRATIONS[..1], 3).unwrap();
            fill_v1(&conn);
        }
        let conn = open_with(&path, MIGRATIONS, 3).unwrap();
        assert_eq!(version(&conn), SCHEMA_VERSION);
        let counts: Vec<i64> = V1_TABLES.iter().map(|t| count(&conn, t)).collect();
        assert_eq!(counts, [2, 1, 1, 1, 1, 1, 2]);
        let parent: String = conn
            .query_row("SELECT parent_id FROM messages WHERE id = 'm2'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(parent, "m1");

        let backups = list_backups(&path);
        assert_eq!(backups.len(), 1);
        let name = backups[0].file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with(&format!("delegation.db.pre-v{SCHEMA_VERSION}-")),
            "{name}"
        );
        // The backup is the database as it was: version 1, same rows.
        let copy = Connection::open(&backups[0]).unwrap();
        assert_eq!(version(&copy), 1);
        assert_eq!(count(&copy, "messages"), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_migration_keeps_the_previous_version_and_names_the_backup() {
        let dir = temp_dir("failing");
        let path = dir.join("delegation.db").to_string_lossy().into_owned();
        {
            let conn = open_with(&path, MIGRATIONS, 3).unwrap();
            fill_v1(&conn);
        }
        let mut list = MIGRATIONS.to_vec();
        let failing = SCHEMA_VERSION + 1;
        list.push(Migration {
            name: "breaks halfway",
            ..sql_migration(failing, 1, |tx| {
                sql(tx, "CREATE TABLE half_done (x); INSERT INTO no_such_table VALUES (1);")
            })
        });
        let err = format!("{:#}", open_with(&path, &list, 3).expect_err("refused"));
        let backup = list_backups(&path).pop().expect("backed up first");
        assert!(
            err.contains(&format!("migration {failing} (breaks halfway) failed")),
            "{err}"
        );
        assert!(err.contains("no_such_table"), "says why: {err}");
        assert!(err.contains(&backup.display().to_string()), "names the backup: {err}");
        assert!(
            err.contains(&format!("still at schema version {SCHEMA_VERSION}")),
            "{err}"
        );

        let conn = Connection::open(&path).unwrap();
        assert_eq!(version(&conn), SCHEMA_VERSION);
        assert!(!has_table(&conn, "half_done"), "the failed migration rolled back");
        assert_eq!(count(&conn, "users"), 2);
        // And the server's own open() refuses through the same path.
        drop(conn);
        assert!(open_with(&path, &list, 3).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn has_table(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            [name],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            > 0
    }

    #[test]
    fn only_the_newest_backups_are_kept() {
        let dir = temp_dir("keep");
        let path = dir.join("delegation.db").to_string_lossy().into_owned();
        {
            open_with(&path, &MIGRATIONS[..1], 2).unwrap();
        }
        // Older backups from earlier upgrades, one of another database, and
        // an unrelated file.
        for name in [
            "delegation.db.pre-v9-20250101T000000.000Z.bak",
            "delegation.db.pre-v10-20250102T000000.000Z.bak",
            "delegation.db.pre-v2-20250103T000000.000Z.bak",
            "other.db.pre-v2-20200101T000000.000Z.bak",
            "notes.txt",
        ] {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        let mut list = MIGRATIONS.to_vec();
        for v in SCHEMA_VERSION + 1..=SCHEMA_VERSION + 3 {
            list.push(sql_migration(v, 1, |_| Ok(())));
            // One upgrade per open, like a deploy per release.
            open_with(&path, &list, 2).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".pre-v") || n == "notes.txt")
            .collect();
        left.sort();
        let ours: Vec<&String> = left.iter().filter(|n| n.starts_with("delegation.db.pre-v")).collect();
        assert_eq!(ours.len(), 2, "{left:?}");
        assert!(
            ours.iter().all(|n| !n.contains("2025")),
            "the old ones went first: {left:?}"
        );
        assert!(left.contains(&"other.db.pre-v2-20200101T000000.000Z.bak".to_string()));
        assert!(left.contains(&"notes.txt".to_string()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_backup_stops_the_migration() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("nobackup");
        let path = dir.join("delegation.db").to_string_lossy().into_owned();
        let mut conn = open_with(&path, &MIGRATIONS[..1], 3).unwrap();
        // Open (so the WAL files exist), then make the data directory
        // read-only so the backup can't be written.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let writable = std::fs::write(dir.join("probe"), "").is_ok();
        let result = ensure_schema(&mut conn, &path, MIGRATIONS, 3);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        if writable {
            // Running as root: directory permissions don't apply.
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let err = format!("{:#}", result.expect_err("refused"));
        assert!(err.contains("wasn't migrated"), "{err}");
        assert_eq!(version(&conn), 1);
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_older_build_starts_after_an_additive_migration_but_not_a_breaking_one() {
        for (breaking, name) in [(false, "additive"), (true, "breaking")] {
            let dir = temp_dir(name);
            let path = dir.join("delegation.db").to_string_lossy().into_owned();
            {
                open_with(&path, &MIGRATIONS[..1], 3).unwrap();
            }
            // The new build: one migration past this one.
            let next = SCHEMA_VERSION + 1;
            let min = if breaking {
                next
            } else {
                MIGRATIONS.last().unwrap().min_compatible
            };
            let mut newer = MIGRATIONS.to_vec();
            newer.push(sql_migration(next, min, |tx| {
                sql(tx, "ALTER TABLE users ADD COLUMN nickname TEXT")
            }));
            open_with(&path, &newer, 3).unwrap();

            // Rolled back to this build.
            let result = open_with(&path, MIGRATIONS, 3);
            if breaking {
                let err = format!("{:#}", result.expect_err("refused"));
                assert!(err.contains(&format!("schema version {next}")), "{err}");
                assert!(
                    err.contains(&format!("this build is schema version {SCHEMA_VERSION}")),
                    "{err}"
                );
                let backup = list_backups(&path).pop().unwrap();
                assert!(err.contains(&backup.display().to_string()), "lists the backups: {err}");
            } else {
                let conn = result.unwrap();
                assert_eq!(version(&conn), next, "left as it is");
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Adds 'paused' to the messages status CHECK: the rebuild the module
    /// docs describe, on the table with the most references.
    fn rebuild_messages(tx: &Transaction, conversation_id: &str) -> Result<()> {
        let original: String = tx.query_row(
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'messages'",
            [],
            |r| r.get(0),
        )?;
        let new = original
            .replacen("CREATE TABLE messages", "CREATE TABLE messages_new", 1)
            .replacen("'orphaned')", "'orphaned','paused')", 1);
        assert_ne!(new, original);
        tx.execute_batch(&new)?;
        tx.execute_batch(&format!(
            "INSERT INTO messages_new (id, conversation_id, parent_id, role, content, status, error, attempt_id,
               attempts, client_id, client_label, model, created_at, completed_at, meta_json)
             SELECT id, {conversation_id}, parent_id, role, content, status, error, attempt_id,
               attempts, client_id, client_label, model, created_at, completed_at, meta_json FROM messages;
             DROP TABLE messages;
             ALTER TABLE messages_new RENAME TO messages;
             CREATE INDEX messages_by_parent ON messages(conversation_id, parent_id, created_at);
             CREATE INDEX messages_active ON messages(conversation_id)
               WHERE status IN ('queued','assigned','running');
             CREATE INDEX messages_by_client ON messages(client_id);"
        ))?;
        Ok(())
    }

    #[test]
    fn a_table_rebuild_keeps_rows_and_references() {
        let dir = temp_dir("rebuild");
        let path = dir.join("delegation.db").to_string_lossy().into_owned();
        {
            let conn = open_with(&path, MIGRATIONS, 3).unwrap();
            fill_v1(&conn);
        }
        let mut list = MIGRATIONS.to_vec();
        list.push(Migration {
            rebuilds_tables: true,
            ..sql_migration(SCHEMA_VERSION + 1, 1, |tx| rebuild_messages(tx, "conversation_id"))
        });
        let conn = open_with(&path, &list, 3).unwrap();
        assert_eq!(version(&conn), SCHEMA_VERSION + 1);
        let fk: bool = conn.query_row("PRAGMA foreign_keys", [], |r| r.get(0)).unwrap();
        assert!(fk, "foreign keys are back on");
        assert_eq!(count(&conn, "messages"), 2);
        conn.execute("UPDATE messages SET status = 'paused' WHERE id = 'm2'", [])
            .unwrap();
        let indexes: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE type = 'index' AND tbl_name = 'messages'
                 AND name NOT LIKE 'sqlite_%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(indexes, 3);
        // The references still work, in both directions: parent_id within
        // the table, conversation and client into it.
        assert!(conn
            .execute(
                "INSERT INTO messages (id, conversation_id, parent_id, role, content, status, created_at)
                 VALUES ('m3', 'conv', 'nope', 'user', '', 'completed', 't')",
                []
            )
            .is_err());
        conn.execute("DELETE FROM clients", []).unwrap();
        let client: Option<String> = conn
            .query_row("SELECT client_id FROM messages WHERE id = 'm2'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(client, None, "deleting the client cleared it on the reply");
        conn.execute("DELETE FROM messages WHERE id = 'm1'", []).unwrap();
        assert_eq!(count(&conn, "messages"), 0, "deleting the parent cascaded to its reply");
        conn.execute(
            "INSERT INTO messages (id, conversation_id, role, content, status, created_at)
                      VALUES ('m4', 'conv', 'user', '', 'completed', 't')",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM conversations", []).unwrap();
        assert_eq!(count(&conn, "messages"), 0, "deleting the conversation cascaded");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_table_rebuild_that_breaks_a_reference_fails() {
        let dir = temp_dir("rebuild-broken");
        let path = dir.join("delegation.db").to_string_lossy().into_owned();
        {
            let conn = open_with(&path, MIGRATIONS, 3).unwrap();
            fill_v1(&conn);
        }
        let mut list = MIGRATIONS.to_vec();
        list.push(Migration {
            name: "rebuild messages badly",
            rebuilds_tables: true,
            // Points every message at a conversation that doesn't exist.
            ..sql_migration(SCHEMA_VERSION + 1, 1, |tx| rebuild_messages(tx, "'missing'"))
        });
        let err = format!("{:#}", open_with(&path, &list, 3).expect_err("refused"));
        assert!(err.contains("broken foreign key references"), "{err}");
        assert!(err.contains("messages"), "{err}");
        let conn = Connection::open(&path).unwrap();
        assert_eq!(version(&conn), SCHEMA_VERSION);
        let conversation: String = conn
            .query_row("SELECT conversation_id FROM messages WHERE id = 'm2'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(conversation, "conv", "rolled back");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
