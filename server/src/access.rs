//! Every authorization decision about clients, in one place. Handlers ask
//! here; they never compare owners or roles themselves.
//!
//! Lending a client to another user (not built yet) slots in as:
//! - a `Borrower(GrantId)` variant of [`ClientAccess`] that allows `View`,
//!   `Use` and `Pool` but not `Manage`;
//! - one join against a `client_grants` table in [`VISIBLE_CLIENTS`];
//! - nothing in the dashboard beyond the "Lent by …" wording, since it
//!   decides what to show from `can_use` / `can_manage`.

use crate::auth::User;
use crate::db::{ClientListRow, CLIENT_LIST_COLUMNS};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

/// Why a user may see a client at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ClientAccess {
    /// The site admin, for a client someone else owns.
    Admin,
    Owner,
    // Borrower(GrantId) — future: a client lent to this user.
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAction {
    View,
    /// Chat with it.
    Use,
    /// Have it take the user's pooled jobs ("any of my clients"). Narrower
    /// than `Use`: the admin may Use anyone's client directly, but a pooled
    /// job must never land on a volunteer's machine just because the
    /// admin's own clients are busy.
    Pool,
    /// Rename or delete it.
    Manage,
}

/// None means the user may not see the client at all (answer 404).
pub fn client_access(user: &User, client: &ClientListRow) -> Option<ClientAccess> {
    if client.owner_user_id == user.id {
        Some(ClientAccess::Owner)
    } else if user.is_admin() {
        Some(ClientAccess::Admin)
    } else {
        None
    }
}

pub fn allows(access: ClientAccess, action: ClientAction) -> bool {
    match (access, action) {
        (ClientAccess::Owner, _) => true,
        // The admin's pool is the admin's own clients.
        (ClientAccess::Admin, ClientAction::Pool) => false,
        (ClientAccess::Admin, _) => true,
        // (ClientAccess::Borrower(_), ClientAction::View | ClientAction::Use | ClientAction::Pool) => true,
        // (ClientAccess::Borrower(_), ClientAction::Manage) => false,
    }
}

/// A client a user's jobs might be placed on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementClient {
    pub client_id: String,
    /// What the dashboard calls it.
    pub label: String,
    /// A job pinned to it may run there.
    pub can_use: bool,
    /// It's in the user's pool.
    pub can_pool: bool,
}

/// Every client `user_id`'s jobs may be placed on, for the scheduler, which
/// asks again at each placement so a change in access (a deleted client,
/// later a revoked loan) takes effect on the next one. None if the user no
/// longer exists.
pub fn placement_clients(conn: &Connection, user_id: &str) -> Result<Option<Vec<PlacementClient>>> {
    let Some(row) = crate::auth::store::find_by_id(conn, user_id)? else {
        return Ok(None);
    };
    let user = row.user;
    let clients = visible_clients(conn, &user)?
        .into_iter()
        .filter_map(|client| {
            let access = client_access(&user, &client)?;
            Some(PlacementClient {
                can_use: allows(access, ClientAction::Use),
                can_pool: allows(access, ClientAction::Pool),
                client_id: client.client_id,
                label: client.friendly_name,
            })
        })
        .collect();
    Ok(Some(clients))
}

/// "Clients this user can see": the admin sees all, everyone else their
/// own. The single place lending extends with a join.
const VISIBLE_CLIENTS: &str =
    "FROM clients c JOIN users u ON u.id = c.owner_user_id WHERE (?1 OR c.owner_user_id = ?2)";

/// Every client `user` can see, newest first.
pub fn visible_clients(conn: &Connection, user: &User) -> Result<Vec<ClientListRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CLIENT_LIST_COLUMNS} {VISIBLE_CLIENTS} ORDER BY c.enrolled_at DESC, c.client_id"
    ))?;
    let rows = stmt
        .query_map((user.is_admin(), &user.id), ClientListRow::from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// One client, if `user` can see it.
pub fn visible_client(conn: &Connection, user: &User, client_id: &str) -> Result<Option<ClientListRow>> {
    Ok(conn
        .query_row(
            &format!("SELECT {CLIENT_LIST_COLUMNS} {VISIBLE_CLIENTS} AND c.client_id = ?3"),
            (user.is_admin(), &user.id, client_id),
            ClientListRow::from_row,
        )
        .optional()?)
}

/// The outcome of checking one action on one client.
pub enum Check {
    Allowed(Box<ClientListRow>),
    /// Visible, but this action isn't allowed (403).
    Denied,
    /// Not visible (404).
    Hidden,
}

pub fn check(conn: &Connection, user: &User, client_id: &str, action: ClientAction) -> Result<Check> {
    let Some(row) = visible_client(conn, user, client_id)? else {
        return Ok(Check::Hidden);
    };
    Ok(match client_access(user, &row) {
        None => Check::Hidden,
        Some(access) if allows(access, action) => Check::Allowed(Box::new(row)),
        Some(_) => Check::Denied,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Role;
    use crate::db::tests::{add_client, add_user, memory_db};

    fn user(id: &str, role: Role) -> User {
        User {
            id: id.into(),
            username: id.into(),
            role,
        }
    }

    #[test]
    fn the_admin_sees_everything_and_users_only_their_own() {
        let db = memory_db();
        let admin_id = add_user(&db, "admin", "admin");
        let alice_id = add_user(&db, "alice", "user");
        let bob_id = add_user(&db, "bob", "user");
        add_client(&db, "a1", &alice_id);
        add_client(&db, "b1", &bob_id);
        add_client(&db, "z1", &admin_id);
        let admin = user(&admin_id, Role::Admin);
        let alice = user(&alice_id, Role::User);

        db.blocking(|c| {
            let ids = |rows: Vec<ClientListRow>| {
                let mut ids: Vec<String> = rows.into_iter().map(|r| r.client_id).collect();
                ids.sort();
                ids
            };
            assert_eq!(ids(visible_clients(c, &admin)?), vec!["a1", "b1", "z1"]);
            assert_eq!(ids(visible_clients(c, &alice)?), vec!["a1"]);
            assert!(visible_client(c, &alice, "b1")?.is_none());

            let a1 = visible_client(c, &admin, "a1")?.unwrap();
            assert_eq!(a1.owner_username, "alice");
            assert_eq!(client_access(&admin, &a1), Some(ClientAccess::Admin));
            assert_eq!(client_access(&alice, &a1), Some(ClientAccess::Owner));
            let b1 = visible_client(c, &admin, "b1")?.unwrap();
            assert_eq!(client_access(&alice, &b1), None);
            let z1 = visible_client(c, &admin, "z1")?.unwrap();
            assert_eq!(client_access(&admin, &z1), Some(ClientAccess::Owner));

            assert!(matches!(check(c, &alice, "b1", ClientAction::View)?, Check::Hidden));
            assert!(matches!(
                check(c, &alice, "a1", ClientAction::Manage)?,
                Check::Allowed(_)
            ));
            assert!(matches!(check(c, &admin, "b1", ClientAction::Use)?, Check::Allowed(_)));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn admins_and_owners_may_do_everything_but_the_admin_pools_only_their_own() {
        for access in [ClientAccess::Admin, ClientAccess::Owner] {
            for action in [ClientAction::View, ClientAction::Use, ClientAction::Manage] {
                assert!(allows(access, action));
            }
        }
        assert!(allows(ClientAccess::Owner, ClientAction::Pool));
        assert!(!allows(ClientAccess::Admin, ClientAction::Pool));
    }

    #[test]
    fn a_pool_is_the_users_own_clients_even_for_the_admin() {
        let db = memory_db();
        let admin_id = add_user(&db, "admin", "admin");
        let alice_id = add_user(&db, "alice", "user");
        add_client(&db, "a1", &alice_id);
        add_client(&db, "z1", &admin_id);
        db.blocking(|c| {
            let pool = |clients: Vec<PlacementClient>, pooled: bool| {
                let mut ids: Vec<String> = clients
                    .into_iter()
                    .filter(|c| if pooled { c.can_pool } else { c.can_use })
                    .map(|c| c.client_id)
                    .collect();
                ids.sort();
                ids
            };
            let admins = placement_clients(c, &admin_id)?.unwrap();
            // The admin may send a job straight to Alice's client, but
            // "any of my clients" means the admin's own.
            assert_eq!(pool(admins.clone(), false), vec!["a1", "z1"]);
            assert_eq!(pool(admins, true), vec!["z1"]);
            let alices = placement_clients(c, &alice_id)?.unwrap();
            assert_eq!(alices.len(), 1, "other people's clients aren't even listed");
            assert_eq!(pool(alices, true), vec!["a1"]);
            assert_eq!(placement_clients(c, "nobody")?, None);
            Ok(())
        })
        .unwrap();
    }
}
