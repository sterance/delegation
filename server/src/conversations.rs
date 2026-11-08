//! Conversations and their messages, as stored in SQLite.
//!
//! A conversation is a tree: every message points at its `parent_id`, and
//! siblings (same parent) are alternative branches. A message's history is
//! the chain from it up to the root. `active_leaf_id` records which branch
//! the user last looked at.
//!
//! Conversations are private: every function here that reads or changes one
//! takes the owner's user id and matches nothing for anyone else, so the
//! handlers answer 404 rather than 403.
//!
//! This module never changes a reply's status. Inserting a new reply as
//! `queued` is its creation; every move after that belongs to `replies.rs`.

use crate::db::now_ts;
use anyhow::Result;
use protocol::{ChatRole, ChatTurn};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A message's state. User and system messages are always `completed`. An
/// assistant reply goes `queued` (created, not yet sent) -> `assigned` (sent
/// to a client) -> `running` (first chunk received) -> one of the final
/// four. These are the job states a future job queue will use. Mirrored in
/// `frontend/src/types/chat.ts`; the database's CHECK constraint is built
/// from `ALL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageStatus {
    Queued,
    Assigned,
    Running,
    Completed,
    Failed,
    Cancelled,
    /// The answering client went away mid-reply (or the server restarted).
    /// The partial text is kept and the reply can be retried.
    Orphaned,
}

impl MessageStatus {
    pub const ALL: [MessageStatus; 7] = [
        MessageStatus::Queued,
        MessageStatus::Assigned,
        MessageStatus::Running,
        MessageStatus::Completed,
        MessageStatus::Failed,
        MessageStatus::Cancelled,
        MessageStatus::Orphaned,
    ];

    /// Still being generated: counts towards the limits, and has a hub in
    /// the reply runner.
    pub const ACTIVE: [MessageStatus; 3] = [MessageStatus::Queued, MessageStatus::Assigned, MessageStatus::Running];

    pub fn as_str(self) -> &'static str {
        match self {
            MessageStatus::Queued => "queued",
            MessageStatus::Assigned => "assigned",
            MessageStatus::Running => "running",
            MessageStatus::Completed => "completed",
            MessageStatus::Failed => "failed",
            MessageStatus::Cancelled => "cancelled",
            MessageStatus::Orphaned => "orphaned",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|status| status.as_str() == s)
    }

    pub fn is_active(self) -> bool {
        Self::ACTIVE.contains(&self)
    }
}

impl ToSql for MessageStatus {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for MessageStatus {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        Self::parse(s).ok_or_else(|| FromSqlError::Other(format!("unknown message status {s:?}").into()))
    }
}

/// Comma-separated quoted active statuses, for `status IN (...)`.
pub fn active_sql() -> String {
    MessageStatus::ACTIVE
        .iter()
        .map(|s| format!("'{}'", s.as_str()))
        .collect::<Vec<_>>()
        .join(",")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
}

impl MessageRole {
    pub const ALL: [MessageRole; 3] = [MessageRole::System, MessageRole::User, MessageRole::Assistant];

    pub fn as_str(self) -> &'static str {
        match self {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
        }
    }

    fn chat_role(self) -> ChatRole {
        match self {
            MessageRole::System => ChatRole::System,
            MessageRole::User => ChatRole::User,
            MessageRole::Assistant => ChatRole::Assistant,
        }
    }
}

impl ToSql for MessageRole {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for MessageRole {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        Self::ALL
            .into_iter()
            .find(|r| r.as_str() == s)
            .ok_or_else(|| FromSqlError::Other(format!("unknown message role {s:?}").into()))
    }
}

/// A message as the API returns it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    pub parent_id: Option<String>,
    pub role: MessageRole,
    pub content: String,
    pub status: MessageStatus,
    pub error: Option<String>,
    pub attempts: i64,
    /// Null once the client that answered has been deleted; `client_label`
    /// keeps its name.
    pub client_id: Option<String>,
    pub client_label: Option<String>,
    pub model: Option<String>,
    pub created_at: String,
    pub completed_at: Option<String>,
    /// Token counts, timings and so on, later.
    pub meta: Option<serde_json::Value>,
}

const MESSAGE_COLUMNS: &str = "id, conversation_id, parent_id, role, content, status, error, attempts, client_id,
    client_label, model, created_at, completed_at, meta_json";

impl Message {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let meta: Option<String> = r.get(13)?;
        Ok(Message {
            id: r.get(0)?,
            conversation_id: r.get(1)?,
            parent_id: r.get(2)?,
            role: r.get(3)?,
            content: r.get(4)?,
            status: r.get(5)?,
            error: r.get(6)?,
            attempts: r.get(7)?,
            client_id: r.get(8)?,
            client_label: r.get(9)?,
            model: r.get(10)?,
            created_at: r.get(11)?,
            completed_at: r.get(12)?,
            meta: meta.and_then(|json| serde_json::from_str(&json).ok()),
        })
    }
}

/// A conversation for the sidebar.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub active_leaf_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

const CONVERSATION_COLUMNS: &str = "id, title, active_leaf_id, created_at, updated_at";

impl ConversationSummary {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(ConversationSummary {
            id: r.get(0)?,
            title: r.get(1)?,
            active_leaf_id: r.get(2)?,
            created_at: r.get(3)?,
            updated_at: r.get(4)?,
        })
    }
}

/// A conversation with every message, in creation order. The dashboard
/// works out the active branch and sibling counts itself.
#[derive(Debug, Clone, Serialize)]
pub struct Conversation {
    #[serde(flatten)]
    pub summary: ConversationSummary,
    pub messages: Vec<Message>,
}

pub const TITLE_MAX_CHARS: usize = 60;
/// For renames from the dashboard.
pub const RENAME_MAX_CHARS: usize = 200;

/// The first line of the first message, shortened.
pub fn title_from(content: &str) -> String {
    let line = content.trim().lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return "New chat".into();
    }
    if line.chars().count() > TITLE_MAX_CHARS {
        format!("{}…", line.chars().take(TITLE_MAX_CHARS).collect::<String>())
    } else {
        line.to_string()
    }
}

/// Newest first. `before` is the `updated_at` of the last conversation on
/// the previous page.
pub fn list_conversations(
    conn: &Connection,
    owner: &str,
    before: Option<&str>,
    limit: usize,
) -> Result<Vec<ConversationSummary>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CONVERSATION_COLUMNS} FROM conversations
         WHERE owner_user_id = ?1 AND (?2 IS NULL OR updated_at < ?2)
         ORDER BY updated_at DESC, id DESC LIMIT ?3"
    ))?;
    let rows = stmt
        .query_map((owner, before, limit as i64), ConversationSummary::from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

pub fn get_conversation(conn: &Connection, owner: &str, id: &str) -> Result<Option<Conversation>> {
    let summary = conn
        .query_row(
            &format!("SELECT {CONVERSATION_COLUMNS} FROM conversations WHERE id = ?1 AND owner_user_id = ?2"),
            (id, owner),
            ConversationSummary::from_row,
        )
        .optional()?;
    let Some(summary) = summary else { return Ok(None) };
    let mut stmt = conn.prepare(&format!(
        "SELECT {MESSAGE_COLUMNS} FROM messages WHERE conversation_id = ?1 ORDER BY created_at, rowid"
    ))?;
    let messages = stmt
        .query_map([id], Message::from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(Some(Conversation { summary, messages }))
}

/// One message, if `owner` owns its conversation.
pub fn get_message(conn: &Connection, owner: &str, message_id: &str) -> Result<Option<Message>> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {} FROM messages m JOIN conversations c ON c.id = m.conversation_id
                 WHERE m.id = ?1 AND c.owner_user_id = ?2",
                MESSAGE_COLUMNS
                    .split(',')
                    .map(|c| format!("m.{}", c.trim()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            (message_id, owner),
            Message::from_row,
        )
        .optional()?)
}

/// Returns false if there's no such conversation for `owner`.
pub fn rename_conversation(conn: &Connection, owner: &str, id: &str, title: &str) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE conversations SET title = ?1 WHERE id = ?2 AND owner_user_id = ?3",
        (title, id, owner),
    )? > 0)
}

pub enum SetLeaf {
    Done,
    NoConversation,
    /// The message isn't in this conversation.
    BadLeaf,
}

/// Switching branch. Deliberately leaves `updated_at` alone: looking at an
/// older branch doesn't move the conversation up the sidebar.
pub fn set_active_leaf(conn: &mut Connection, owner: &str, id: &str, leaf_id: &str) -> Result<SetLeaf> {
    let tx = conn.transaction()?;
    if !owns(&tx, owner, id)? {
        return Ok(SetLeaf::NoConversation);
    }
    let in_conversation = tx
        .query_row(
            "SELECT 1 FROM messages WHERE id = ?1 AND conversation_id = ?2",
            (leaf_id, id),
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !in_conversation {
        return Ok(SetLeaf::BadLeaf);
    }
    tx.execute(
        "UPDATE conversations SET active_leaf_id = ?1 WHERE id = ?2",
        (leaf_id, id),
    )?;
    tx.commit()?;
    Ok(SetLeaf::Done)
}

pub fn delete_conversation(conn: &Connection, owner: &str, id: &str) -> Result<bool> {
    Ok(conn.execute(
        "DELETE FROM conversations WHERE id = ?1 AND owner_user_id = ?2",
        (id, owner),
    )? > 0)
}

fn owns(conn: &Connection, owner: &str, id: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM conversations WHERE id = ?1 AND owner_user_id = ?2",
            (id, owner),
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Why a new reply can't start right now.
#[derive(Debug, PartialEq, Eq)]
pub enum Busy {
    /// This conversation already has a reply being generated.
    Conversation,
    /// The user has `CHAT_MAX_ACTIVE_REPLIES_PER_USER` replies being generated.
    User,
}

/// Checked inside the transaction that would add a reply, so two sends at
/// once can't both pass.
pub fn check_limits(
    tx: &Transaction<'_>,
    owner: &str,
    conversation_id: &str,
    max_per_user: usize,
) -> Result<Option<Busy>> {
    let active = active_sql();
    let in_conversation: i64 = tx.query_row(
        &format!("SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND status IN ({active})"),
        [conversation_id],
        |r| r.get(0),
    )?;
    if in_conversation > 0 {
        return Ok(Some(Busy::Conversation));
    }
    let for_user: i64 = tx.query_row(
        &format!(
            "SELECT COUNT(*) FROM messages m JOIN conversations c ON c.id = m.conversation_id
             WHERE c.owner_user_id = ?1 AND m.status IN ({active})"
        ),
        [owner],
        |r| r.get(0),
    )?;
    if for_user as usize >= max_per_user {
        return Ok(Some(Busy::User));
    }
    Ok(None)
}

pub struct NewTurn<'a> {
    pub owner: &'a str,
    pub conversation_id: &'a str,
    /// None regenerates: a new reply to `parent_id`, which must be a user
    /// message, with no new user message.
    pub content: Option<&'a str>,
    /// What the new message answers: normally the current leaf, or an
    /// earlier message when editing. None starts a new root.
    pub parent_id: Option<&'a str>,
    pub max_active_per_user: usize,
}

pub struct CreatedTurn {
    /// Set when this send created the conversation.
    pub new_conversation: Option<ConversationSummary>,
    pub user_message: Option<Message>,
    pub reply: Message,
}

pub enum TurnOutcome {
    Created(Box<CreatedTurn>),
    /// The conversation exists but isn't this user's.
    NotFound,
    BadParent(&'static str),
    Busy(Busy),
}

/// The send transaction: creates the conversation if it doesn't exist yet
/// (titled from the message), inserts the user message (unless
/// regenerating) and an empty `queued` reply as its child, and points
/// `active_leaf_id` at the reply.
pub fn create_turn(conn: &mut Connection, turn: NewTurn<'_>) -> Result<TurnOutcome> {
    let tx = conn.transaction()?;
    let now = now_ts();
    let owner_of: Option<String> = tx
        .query_row(
            "SELECT owner_user_id FROM conversations WHERE id = ?1",
            [turn.conversation_id],
            |r| r.get(0),
        )
        .optional()?;
    let new_conversation = match owner_of {
        Some(owner) if owner != turn.owner => return Ok(TurnOutcome::NotFound),
        Some(_) => None,
        None => {
            let Some(content) = turn.content else {
                return Ok(TurnOutcome::NotFound);
            };
            let summary = ConversationSummary {
                id: turn.conversation_id.to_string(),
                title: title_from(content),
                active_leaf_id: None,
                created_at: now.clone(),
                updated_at: now.clone(),
            };
            tx.execute(
                "INSERT INTO conversations (id, owner_user_id, title, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)",
                (&summary.id, turn.owner, &summary.title, &now),
            )?;
            Some(summary)
        }
    };

    if let Some(parent_id) = turn.parent_id {
        let parent_role: Option<MessageRole> = tx
            .query_row(
                "SELECT role FROM messages WHERE id = ?1 AND conversation_id = ?2",
                (parent_id, turn.conversation_id),
                |r| r.get(0),
            )
            .optional()?;
        match (parent_role, turn.content.is_some()) {
            (None, _) => return Ok(TurnOutcome::BadParent("parent_id isn't a message in this conversation")),
            (Some(MessageRole::User), true) => {
                return Ok(TurnOutcome::BadParent(
                    "a new message must follow a reply, not another message",
                ))
            }
            (Some(role), false) if role != MessageRole::User => {
                return Ok(TurnOutcome::BadParent(
                    "regenerating needs parent_id to be a user message",
                ))
            }
            _ => {}
        }
    } else if turn.content.is_none() {
        return Ok(TurnOutcome::BadParent("regenerating needs parent_id"));
    }

    if let Some(busy) = check_limits(&tx, turn.owner, turn.conversation_id, turn.max_active_per_user)? {
        return Ok(TurnOutcome::Busy(busy));
    }

    let message = |role: MessageRole, parent_id: Option<String>, content: &str, status: MessageStatus| Message {
        id: Uuid::new_v4().to_string(),
        conversation_id: turn.conversation_id.to_string(),
        parent_id,
        role,
        content: content.to_string(),
        status,
        error: None,
        attempts: 0,
        client_id: None,
        client_label: None,
        model: None,
        created_at: now.clone(),
        completed_at: None,
        meta: None,
    };
    let user_message = turn.content.map(|content| {
        let mut m = message(
            MessageRole::User,
            turn.parent_id.map(str::to_string),
            content,
            MessageStatus::Completed,
        );
        m.completed_at = Some(now.clone());
        m
    });
    let reply_parent = match &user_message {
        Some(m) => Some(m.id.clone()),
        None => turn.parent_id.map(str::to_string),
    };
    let reply = message(MessageRole::Assistant, reply_parent, "", MessageStatus::Queued);
    for m in user_message.iter().chain([&reply]) {
        tx.execute(
            "INSERT INTO messages (id, conversation_id, parent_id, role, content, status, created_at, completed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                &m.id,
                &m.conversation_id,
                &m.parent_id,
                m.role,
                &m.content,
                m.status,
                &m.created_at,
                &m.completed_at,
            ),
        )?;
    }
    tx.execute(
        "UPDATE conversations SET active_leaf_id = ?1, updated_at = ?2 WHERE id = ?3",
        (&reply.id, &now, turn.conversation_id),
    )?;
    tx.commit()?;
    let new_conversation = new_conversation.map(|c| ConversationSummary {
        active_leaf_id: Some(reply.id.clone()),
        ..c
    });
    Ok(TurnOutcome::Created(Box::new(CreatedTurn {
        new_conversation,
        user_message,
        reply,
    })))
}

/// What a client is sent for `reply_id`: the chain from its parent up to
/// the root, oldest first, `completed` messages only (so failed and
/// cancelled replies aren't fed back as context).
pub fn history_for(conn: &Connection, reply_id: &str) -> Result<Vec<ChatTurn>> {
    let mut stmt = conn.prepare(
        "WITH RECURSIVE chain(id, parent_id, role, content, status, depth) AS (
           SELECT p.id, p.parent_id, p.role, p.content, p.status, 0
           FROM messages r JOIN messages p ON p.id = r.parent_id WHERE r.id = ?1
           UNION ALL
           SELECT m.id, m.parent_id, m.role, m.content, m.status, chain.depth + 1
           FROM messages m JOIN chain ON m.id = chain.parent_id
         )
         SELECT role, content FROM chain WHERE status = 'completed' ORDER BY depth DESC",
    )?;
    let turns = stmt
        .query_map([reply_id], |r| {
            let role: MessageRole = r.get(0)?;
            Ok(ChatTurn {
                role: role.chat_role(),
                content: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    // Trimmed to the model's context by the reply runner (`trim_history`),
    // which knows which model will answer.
    Ok(turns)
}

/// A rough token count: characters / 4, plus a few for each turn's role
/// markers in the chat template. Good enough to stay clear of the limit.
pub fn estimate_tokens(turn: &ChatTurn) -> u64 {
    const PER_TURN: u64 = 4;
    (turn.content.chars().count() as u64).div_ceil(4) + PER_TURN
}

/// Drops the oldest turns until the estimated prompt fits in
/// `budget_tokens` (the model's context minus the room kept for the
/// reply). The newest turn, the user's message, always stays, even alone
/// over budget, and the history never starts with an assistant turn.
/// Returns the turns kept and how many were dropped.
pub fn trim_history(turns: Vec<ChatTurn>, budget_tokens: u64) -> (Vec<ChatTurn>, usize) {
    let total = turns.len();
    let mut used = 0;
    let mut keep = 0;
    for (i, turn) in turns.iter().enumerate().rev() {
        let cost = estimate_tokens(turn);
        if i + 1 < total && used + cost > budget_tokens {
            break;
        }
        used += cost;
        keep += 1;
    }
    let mut kept: Vec<ChatTurn> = turns.into_iter().skip(total - keep).collect();
    while kept.len() > 1 && kept[0].role == ChatRole::Assistant {
        kept.remove(0);
    }
    let dropped = total - kept.len();
    (kept, dropped)
}

/// `CHAT_RETENTION_DAYS`: deletes conversations not updated since `cutoff`.
pub fn delete_older_than(conn: &Connection, cutoff: &str) -> Result<usize> {
    Ok(conn.execute("DELETE FROM conversations WHERE updated_at < ?1", [cutoff])?)
}

/// A message's status and content, for tests.
#[cfg(test)]
pub fn message_state(conn: &Connection, message_id: &str) -> Result<Option<(MessageStatus, String)>> {
    Ok(conn
        .query_row(
            "SELECT status, content FROM messages WHERE id = ?1",
            [message_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::{add_user, memory_db};

    fn turn<'a>(owner: &'a str, conv: &'a str, content: Option<&'a str>, parent: Option<&'a str>) -> NewTurn<'a> {
        NewTurn {
            owner,
            conversation_id: conv,
            content,
            parent_id: parent,
            max_active_per_user: 10,
        }
    }

    fn created(outcome: TurnOutcome) -> (Option<Message>, Message) {
        match outcome {
            TurnOutcome::Created(turn) => (turn.user_message, turn.reply),
            _ => panic!("expected the turn to be created"),
        }
    }

    /// Marks a reply completed with `text`, as the runner would.
    fn complete(c: &Connection, id: &str, text: &str) {
        c.execute(
            "UPDATE messages SET status = 'completed', content = ?1 WHERE id = ?2",
            (text, id),
        )
        .unwrap();
    }

    #[test]
    fn statuses_parse_and_render() {
        for s in MessageStatus::ALL {
            assert_eq!(MessageStatus::parse(s.as_str()), Some(s));
            assert_eq!(serde_json::to_value(s).unwrap(), serde_json::json!(s.as_str()));
        }
        assert_eq!(MessageStatus::parse("done"), None);
        assert!(MessageStatus::Running.is_active() && !MessageStatus::Orphaned.is_active());
    }

    #[test]
    fn titles_come_from_the_first_line() {
        assert_eq!(title_from("  hello\nworld"), "hello");
        assert_eq!(title_from("\n\n"), "New chat");
        let long = "x".repeat(70);
        assert_eq!(title_from(&long).chars().count(), TITLE_MAX_CHARS + 1);
    }

    #[test]
    fn sends_build_a_tree_and_history_follows_one_branch() {
        let db = memory_db();
        let alice = add_user(&db, "alice", "user");
        db.blocking(|c| {
            let (u1, r1) = created(create_turn(c, turn(&alice, "conv", Some("first"), None))?);
            let u1 = u1.unwrap();
            assert_eq!(r1.parent_id.as_deref(), Some(u1.id.as_str()));
            assert_eq!(r1.status, MessageStatus::Queued);
            complete(c, &r1.id, "reply one");

            let (u2, r2) = created(create_turn(c, turn(&alice, "conv", Some("second"), Some(&r1.id)))?);
            complete(c, &r2.id, "reply two");
            // Edit the second message: a sibling of u2.
            let (u2b, r2b) = created(create_turn(
                c,
                turn(&alice, "conv", Some("second, edited"), Some(&r1.id)),
            )?);
            assert_eq!(u2b.as_ref().unwrap().parent_id, u2.as_ref().unwrap().parent_id);

            let history = history_for(c, &r2b.id)?;
            let texts: Vec<&str> = history.iter().map(|t| t.content.as_str()).collect();
            assert_eq!(texts, vec!["first", "reply one", "second, edited"]);
            complete(c, &r2b.id, "reply two b");

            // Regenerate r2: a sibling reply to u2, no new user message.
            let (none, r2c) = created(create_turn(c, turn(&alice, "conv", None, Some(&u2.unwrap().id)))?);
            assert!(none.is_none());
            assert_eq!(r2c.parent_id, r2.parent_id);
            let texts: Vec<String> = history_for(c, &r2c.id)?.into_iter().map(|t| t.content).collect();
            assert_eq!(texts, vec!["first", "reply one", "second"]);

            let conv = get_conversation(c, &alice, "conv")?.unwrap();
            assert_eq!(conv.messages.len(), 7);
            assert_eq!(conv.summary.active_leaf_id.as_deref(), Some(r2c.id.as_str()));
            assert_eq!(conv.summary.title, "first");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn history_skips_replies_that_did_not_complete() {
        let db = memory_db();
        let alice = add_user(&db, "alice", "user");
        db.blocking(|c| {
            let (_, r1) = created(create_turn(c, turn(&alice, "conv", Some("hi"), None))?);
            c.execute(
                "UPDATE messages SET status = 'failed', content = 'partial' WHERE id = ?1",
                [&r1.id],
            )?;
            let (_, r2) = created(create_turn(c, turn(&alice, "conv", Some("again"), Some(&r1.id)))?);
            let texts: Vec<String> = history_for(c, &r2.id)?.into_iter().map(|t| t.content).collect();
            assert_eq!(texts, vec!["hi", "again"]);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn conversations_are_private_and_parents_are_checked() {
        let db = memory_db();
        let alice = add_user(&db, "alice", "user");
        let bob = add_user(&db, "bob", "user");
        db.blocking(|c| {
            let (u1, r1) = created(create_turn(c, turn(&alice, "conv", Some("hi"), None))?);
            complete(c, &r1.id, "hello");
            assert!(matches!(
                create_turn(c, turn(&bob, "conv", Some("mine now"), Some(&r1.id)))?,
                TurnOutcome::NotFound
            ));
            assert!(get_conversation(c, &bob, "conv")?.is_none());
            assert!(get_message(c, &bob, &r1.id)?.is_none());
            assert!(get_message(c, &alice, &r1.id)?.is_some());
            assert!(!rename_conversation(c, &bob, "conv", "x")?);
            assert!(!delete_conversation(c, &bob, "conv")?);
            assert!(matches!(
                set_active_leaf(c, &bob, "conv", &r1.id)?,
                SetLeaf::NoConversation
            ));
            assert!(list_conversations(c, &bob, None, 50)?.is_empty());

            assert!(matches!(
                create_turn(c, turn(&alice, "conv", Some("x"), Some("nope")))?,
                TurnOutcome::BadParent(_)
            ));
            let u1 = u1.unwrap();
            assert!(matches!(
                create_turn(c, turn(&alice, "conv", Some("x"), Some(&u1.id)))?,
                TurnOutcome::BadParent(_)
            ));
            assert!(matches!(
                create_turn(c, turn(&alice, "conv", None, Some(&r1.id)))?,
                TurnOutcome::BadParent(_)
            ));
            assert!(matches!(set_active_leaf(c, &alice, "conv", "nope")?, SetLeaf::BadLeaf));
            assert!(matches!(set_active_leaf(c, &alice, "conv", &u1.id)?, SetLeaf::Done));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn limits_one_active_reply_per_conversation_and_n_per_user() {
        let db = memory_db();
        let alice = add_user(&db, "alice", "user");
        db.blocking(|c| {
            let mut t = turn(&alice, "a", Some("hi"), None);
            t.max_active_per_user = 2;
            let (_, ra) = created(create_turn(c, t)?);
            let mut t = turn(&alice, "a", Some("again"), Some(&ra.id));
            t.max_active_per_user = 2;
            assert!(matches!(create_turn(c, t)?, TurnOutcome::Busy(Busy::Conversation)));
            let mut t = turn(&alice, "b", Some("hi"), None);
            t.max_active_per_user = 2;
            created(create_turn(c, t)?);
            let mut t = turn(&alice, "c", Some("hi"), None);
            t.max_active_per_user = 2;
            assert!(matches!(create_turn(c, t)?, TurnOutcome::Busy(Busy::User)));
            // A busy send created nothing.
            assert!(get_conversation(c, &alice, "c")?.is_none());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn trimming_drops_the_oldest_turns_and_keeps_the_newest_user_message() {
        let t = |role, n: usize| ChatTurn {
            role,
            content: "x".repeat(n),
        };
        // Each 40-char turn costs 10 + 4 = 14 tokens.
        let history = vec![
            t(ChatRole::User, 40),
            t(ChatRole::Assistant, 40),
            t(ChatRole::User, 40),
            t(ChatRole::Assistant, 40),
            t(ChatRole::User, 40),
        ];
        let (kept, dropped) = trim_history(history.clone(), 1_000);
        assert_eq!((kept.len(), dropped), (5, 0), "it all fits");

        // Room for two turns, but the older would be an assistant turn
        // opening the history, so it goes too.
        let (kept, dropped) = trim_history(history.clone(), 28);
        assert_eq!((kept.len(), dropped), (1, 4));
        // Room for three: user, assistant, user.
        let (kept, dropped) = trim_history(history.clone(), 42);
        assert_eq!((kept.len(), dropped), (3, 2));
        // Room for four: the leading assistant turn is dropped again.
        let (kept, dropped) = trim_history(history.clone(), 56);
        assert_eq!((kept.len(), dropped), (3, 2));
        assert_eq!(kept[0].role, ChatRole::User);
        assert_eq!(kept.last(), history.last());

        // The newest message alone over budget still goes.
        let (kept, dropped) = trim_history(vec![t(ChatRole::User, 10), t(ChatRole::User, 4_000)], 10);
        assert_eq!((kept.len(), dropped), (1, 1));
        assert_eq!(kept[0].content.len(), 4_000);
        assert_eq!(trim_history(Vec::new(), 0), (Vec::new(), 0));
    }

    #[test]
    fn listing_pages_newest_first_and_retention_deletes() {
        let db = memory_db();
        let alice = add_user(&db, "alice", "user");
        db.blocking(|c| {
            for (id, at) in [("old", "2026-01-01T00:00:00.000Z"), ("mid", "2026-06-01T00:00:00.000Z"), ("new", "2026-11-01T00:00:00.000Z")] {
                c.execute(
                    "INSERT INTO conversations (id, owner_user_id, title, created_at, updated_at) VALUES (?1, ?2, ?1, ?3, ?3)",
                    (id, &alice, at),
                )?;
            }
            let page: Vec<String> = list_conversations(c, &alice, None, 2)?.into_iter().map(|s| s.id).collect();
            assert_eq!(page, vec!["new", "mid"]);
            let next: Vec<String> = list_conversations(c, &alice, Some("2026-06-01T00:00:00.000Z"), 2)?
                .into_iter()
                .map(|s| s.id)
                .collect();
            assert_eq!(next, vec!["old"]);
            assert_eq!(delete_older_than(c, "2026-07-01T00:00:00.000Z")?, 2);
            assert_eq!(list_conversations(c, &alice, None, 10)?.len(), 1);
            Ok(())
        })
        .unwrap();
    }
}
