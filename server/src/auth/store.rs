//! SQL for users, sessions and password reset links. Session and reset
//! tokens are only ever stored as SHA-256 hashes.

use super::{Role, User};
use crate::db::{now_ts, ts};
use anyhow::Result;
use rusqlite::{Connection, ErrorCode, OptionalExtension};
use serde::Serialize;
use std::time::SystemTime;

pub struct UserRow {
    pub user: User,
    pub password_hash: String,
    pub disabled: bool,
}

fn role_of(s: &str) -> rusqlite::Result<Role> {
    Role::parse(s)
        .ok_or_else(|| rusqlite::Error::InvalidColumnType(0, format!("role {s}"), rusqlite::types::Type::Text))
}

pub fn admin_exists(conn: &Connection) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM users WHERE role = 'admin'", [], |_| Ok(()))
        .optional()?
        .is_some())
}

pub fn admin_id(conn: &Connection) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT id FROM users WHERE role = 'admin'", [], |r| r.get(0))
        .optional()?)
}

/// Case-insensitive, like the column.
pub fn find_by_username(conn: &Connection, username: &str) -> Result<Option<UserRow>> {
    Ok(conn
        .query_row(
            "SELECT id, username, role, password_hash, disabled_at IS NOT NULL FROM users WHERE username = ?1",
            [username],
            |r| {
                let role: String = r.get(2)?;
                Ok(UserRow {
                    user: User {
                        id: r.get(0)?,
                        username: r.get(1)?,
                        role: role_of(&role)?,
                    },
                    password_hash: r.get(3)?,
                    disabled: r.get(4)?,
                })
            },
        )
        .optional()?)
}

pub fn find_by_id(conn: &Connection, id: &str) -> Result<Option<UserRow>> {
    Ok(conn
        .query_row(
            "SELECT id, username, role, password_hash, disabled_at IS NOT NULL FROM users WHERE id = ?1",
            [id],
            |r| {
                let role: String = r.get(2)?;
                Ok(UserRow {
                    user: User {
                        id: r.get(0)?,
                        username: r.get(1)?,
                        role: role_of(&role)?,
                    },
                    password_hash: r.get(3)?,
                    disabled: r.get(4)?,
                })
            },
        )
        .optional()?)
}

pub fn username_taken(conn: &Connection, username: &str) -> Result<bool> {
    Ok(conn
        .query_row("SELECT 1 FROM users WHERE username = ?1", [username], |_| Ok(()))
        .optional()?
        .is_some())
}

pub enum Inserted {
    Done(User),
    /// The username (in any case) exists, or, for an admin, an admin does.
    Conflict,
}

pub fn insert_user(conn: &Connection, username: &str, password_hash: &str, role: Role) -> Result<Inserted> {
    let user = User {
        id: uuid::Uuid::new_v4().to_string(),
        username: username.to_string(),
        role,
    };
    let now = now_ts();
    match conn.execute(
        "INSERT INTO users (id, username, password_hash, role, created_at, password_changed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
        (&user.id, username, password_hash, role.as_str(), &now),
    ) {
        Ok(_) => Ok(Inserted::Done(user)),
        Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == ErrorCode::ConstraintViolation => Ok(Inserted::Conflict),
        Err(e) => Err(e.into()),
    }
}

/// Rehashing after login: same password, so sessions stay.
pub fn replace_hash(conn: &Connection, user_id: &str, password_hash: &str) -> Result<()> {
    conn.execute(
        "UPDATE users SET password_hash = ?1 WHERE id = ?2",
        (password_hash, user_id),
    )?;
    Ok(())
}

/// A new password: signs the user out of every session except `keep`.
pub fn set_password(conn: &mut Connection, user_id: &str, password_hash: &str, keep: Option<&str>) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE users SET password_hash = ?1, password_changed_at = ?2 WHERE id = ?3",
        (password_hash, now_ts(), user_id),
    )?;
    tx.execute(
        "DELETE FROM sessions WHERE user_id = ?1 AND (?2 IS NULL OR token_hash != ?2)",
        (user_id, keep),
    )?;
    tx.commit()?;
    Ok(())
}

// ---------------- sessions ----------------

pub struct NewSession<'a> {
    pub token_hash: &'a str,
    pub user_id: &'a str,
    pub expires_at: SystemTime,
    pub persistent: bool,
    pub user_agent: Option<&'a str>,
    pub ip: &'a str,
}

pub fn insert_session(conn: &Connection, s: NewSession<'_>) -> Result<()> {
    let now = now_ts();
    conn.execute(
        "INSERT INTO sessions (token_hash, user_id, created_at, last_seen_at, expires_at, persistent, user_agent, ip)
         VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7)",
        (
            s.token_hash,
            s.user_id,
            &now,
            ts(s.expires_at),
            s.persistent,
            s.user_agent,
            s.ip,
        ),
    )?;
    Ok(())
}

pub struct SessionRow {
    pub user: User,
    pub disabled: bool,
    pub last_seen_at: String,
    pub expires_at: String,
}

pub fn load_session(conn: &Connection, token_hash: &str) -> Result<Option<SessionRow>> {
    Ok(conn
        .query_row(
            "SELECT u.id, u.username, u.role, u.disabled_at IS NOT NULL, s.last_seen_at, s.expires_at
             FROM sessions s JOIN users u ON u.id = s.user_id WHERE s.token_hash = ?1",
            [token_hash],
            |r| {
                let role: String = r.get(2)?;
                Ok(SessionRow {
                    user: User {
                        id: r.get(0)?,
                        username: r.get(1)?,
                        role: role_of(&role)?,
                    },
                    disabled: r.get(3)?,
                    last_seen_at: r.get(4)?,
                    expires_at: r.get(5)?,
                })
            },
        )
        .optional()?)
}

pub fn touch_session(conn: &Connection, token_hash: &str, at: SystemTime) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET last_seen_at = ?1 WHERE token_hash = ?2",
        (ts(at), token_hash),
    )?;
    Ok(())
}

pub fn delete_session(conn: &Connection, token_hash: &str) -> Result<()> {
    conn.execute("DELETE FROM sessions WHERE token_hash = ?1", [token_hash])?;
    Ok(())
}

/// The cleanup task: sessions past their expiry or idle too long, and reset
/// links that were used or have expired.
pub fn delete_expired(conn: &Connection, now: SystemTime, idle_cutoff: SystemTime) -> Result<(usize, usize)> {
    let sessions = conn.execute(
        "DELETE FROM sessions WHERE expires_at <= ?1 OR last_seen_at <= ?2",
        (ts(now), ts(idle_cutoff)),
    )?;
    let resets = conn.execute(
        "DELETE FROM password_resets WHERE used_at IS NOT NULL OR expires_at <= ?1",
        [ts(now)],
    )?;
    Ok((sessions, resets))
}

// ---------------- password reset links ----------------

/// Invalidates the user's older unused links, in the same transaction.
pub fn create_reset(
    conn: &mut Connection,
    token_hash: &str,
    user_id: &str,
    created_by: &str,
    expires_at: SystemTime,
) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "DELETE FROM password_resets WHERE user_id = ?1 AND used_at IS NULL",
        [user_id],
    )?;
    tx.execute(
        "INSERT INTO password_resets (id, token_hash, user_id, created_by, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        (
            uuid::Uuid::new_v4().to_string(),
            token_hash,
            user_id,
            created_by,
            now_ts(),
            ts(expires_at),
        ),
    )?;
    tx.commit()?;
    Ok(())
}

pub struct ResetLink {
    pub id: String,
    pub username: String,
    pub expires_at: String,
}

/// An unused, unexpired link.
pub fn find_reset(conn: &Connection, token_hash: &str) -> Result<Option<ResetLink>> {
    Ok(conn
        .query_row(
            "SELECT r.id, u.username, r.expires_at
             FROM password_resets r JOIN users u ON u.id = r.user_id
             WHERE r.token_hash = ?1 AND r.used_at IS NULL AND r.expires_at > ?2",
            (token_hash, now_ts()),
            |r| {
                Ok(ResetLink {
                    id: r.get(0)?,
                    username: r.get(1)?,
                    expires_at: r.get(2)?,
                })
            },
        )
        .optional()?)
}

/// Uses the link: sets the password and signs the user out everywhere.
/// Returns the user id, or None if the link was used (or expired) first.
pub fn use_reset(conn: &mut Connection, token_hash: &str, password_hash: &str) -> Result<Option<String>> {
    let tx = conn.transaction()?;
    let now = now_ts();
    let user_id: Option<String> = tx
        .query_row(
            "UPDATE password_resets SET used_at = ?1
             WHERE token_hash = ?2 AND used_at IS NULL AND expires_at > ?1 RETURNING user_id",
            (&now, token_hash),
            |r| r.get(0),
        )
        .optional()?;
    let Some(user_id) = user_id else { return Ok(None) };
    tx.execute(
        "UPDATE users SET password_hash = ?1, password_changed_at = ?2 WHERE id = ?3",
        (password_hash, &now, &user_id),
    )?;
    tx.execute("DELETE FROM sessions WHERE user_id = ?1", [&user_id])?;
    tx.commit()?;
    Ok(Some(user_id))
}

// ---------------- the admin's Users page ----------------

#[derive(Debug, Serialize)]
pub struct UserSummary {
    pub id: String,
    pub username: String,
    pub role: Role,
    pub created_at: String,
    pub disabled_at: Option<String>,
    pub client_count: i64,
    pub conversation_count: i64,
    /// The latest `last_seen_at` of any of their sessions.
    pub last_active_at: Option<String>,
}

/// Newest first, since anyone can sign up.
pub fn list_users(conn: &Connection) -> Result<Vec<UserSummary>> {
    let mut stmt = conn.prepare(
        "SELECT u.id, u.username, u.role, u.created_at, u.disabled_at,
                (SELECT COUNT(*) FROM clients c WHERE c.owner_user_id = u.id),
                (SELECT COUNT(*) FROM conversations v WHERE v.owner_user_id = u.id),
                (SELECT MAX(s.last_seen_at) FROM sessions s WHERE s.user_id = u.id)
         FROM users u ORDER BY u.created_at DESC, u.username",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let role: String = r.get(2)?;
            Ok(UserSummary {
                id: r.get(0)?,
                username: r.get(1)?,
                role: role_of(&role)?,
                created_at: r.get(3)?,
                disabled_at: r.get(4)?,
                client_count: r.get(5)?,
                conversation_count: r.get(6)?,
                last_active_at: r.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Disabling also ends every session. Returns false if there's no such user.
pub fn set_disabled(conn: &mut Connection, user_id: &str, disabled: bool) -> Result<bool> {
    let tx = conn.transaction()?;
    let updated = tx.execute(
        "UPDATE users SET disabled_at = CASE WHEN ?1 THEN COALESCE(disabled_at, ?2) END WHERE id = ?3",
        (disabled, now_ts(), user_id),
    )?;
    if disabled {
        tx.execute("DELETE FROM sessions WHERE user_id = ?1", [user_id])?;
    }
    tx.commit()?;
    Ok(updated > 0)
}

/// Deletes the user and, by cascade, their sessions, reset links,
/// conversations, pairing codes and clients. Returns the deleted clients'
/// ids (to disconnect them), or None if there's no such user.
pub fn delete_user(conn: &mut Connection, user_id: &str) -> Result<Option<Vec<String>>> {
    let tx = conn.transaction()?;
    let clients: Vec<String> = {
        let mut stmt = tx.prepare("SELECT client_id FROM clients WHERE owner_user_id = ?1")?;
        let ids = stmt
            .query_map([user_id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        ids
    };
    if tx.execute("DELETE FROM users WHERE id = ?1", [user_id])? == 0 {
        return Ok(None);
    }
    tx.commit()?;
    Ok(Some(clients))
}
