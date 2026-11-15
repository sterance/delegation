# The server database

The server keeps everything (accounts, sessions, clients, pairing codes,
conversations, jobs) in one SQLite file, `delegation.db`, in WAL mode. In
production it lives at `/opt/apps/delegation/data/delegation.db` on the host
(`/app/data` in the `delegation` container, deployed by `sterance/infra`'s
`deploy-backend.yml`). It's `data/delegation.db` for a native run, and the
`dev-server-data` volume for `dev/dev.sh`.

## How the schema is versioned

`PRAGMA user_version` is the version of the last migration applied. The
migrations are an ordered, forward-only list in
[`src/db/migrations.rs`](src/db/migrations.rs), and this build's schema
version is the last one's. On startup the server does one of these:

| Database                              | What happens                                                                 |
| ------------------------------------- | ---------------------------------------------------------------------------- |
| Empty (new)                           | Every migration is applied. No backup, since there's nothing to back up.     |
| At this build's version               | Starts.                                                                      |
| Older                                 | Backs up the database, then applies each pending migration in order, logging `[db] applying migration N (name)`. |
| Newer, and compatible with this build | Starts, with a warning. This is normal right after a rollback across an additive migration. |
| Newer, and not compatible             | Refuses to start, naming both versions and the backups next to the database. |
| Version 0 with tables (from before auth and chat persistence) | Refuses to start. It can't be upgraded: delete it and re-enroll every client. |

Each migration runs in its own transaction together with the `user_version`
update, so a failure leaves the database at the last migration that fully
applied. The server then refuses to start and logs `REFUSING TO START`,
naming the failed migration, the error and the pre-migration backup. It
doesn't retry. Fix the cause and deploy again, or restore the backup.

### Rollback compatibility

When a deploy fails its health check (`/api/health`), the infra pipeline goes
back to the previous image. If the new image already migrated the database,
that older build then starts against a newer schema. The `schema_meta` table
(added by migration 2) records `min_compatible_version`: the oldest schema
version whose code still works against this database. The older build
starts if its own version is at least that, and refuses otherwise.

- **Additive** migrations (new tables, new nullable columns or columns with
  defaults, new indexes) keep `min_compatible` where it was, so a rollback
  keeps working.
- **Breaking** migrations (dropping or renaming columns or tables, tightening
  constraints, changing what a value means) raise `min_compatible` to their
  own version. A rollback across one refuses to start, which is correct: the
  older code would misread the data. Restore the pre-migration backup to go
  back.

This only protects rollbacks to builds that know about `schema_meta`, which is
schema version 2 onwards. A build at schema version 1 (the first auth and chat
persistence release) refuses every newer database. So if the deploy that
first upgrades to version 2 fails and rolls back, restore the backup to
recover (see below).

## The job tables

Migration 3 added the job queue's two tables. [`JOBS.md`](JOBS.md) describes
how they're used.

**`jobs`**: one row per job. Chat replies are the only kind so far.

| Column | Meaning |
|---|---|
| `id` | The job's id. |
| `kind` | `chat_reply`. |
| `owner_user_id` | Whose job it is. Deleting the user deletes their jobs. |
| `message_id` | The chat reply this job produces: unique, and required for `chat_reply` jobs. Deleting the message (with its conversation) deletes the job. |
| `status` | `queued`, `assigned`, `running`, `completed`, `failed`, `cancelled` or `orphaned`. This is the truth; `messages.status` is written with the same value in the same transaction. |
| `target_kind` | `client` (pinned to one client) or `pool` (any client in the owner's pool). |
| `pinned_client_id` | The client a `client` job is pinned to. Set to NULL when that client is deleted, which a CHECK only allows once the job has finished, so the server fails such jobs before deleting the client. |
| `model` | The model the job needs. |
| `requirements_json` | Reserved for later requirements (minimum VRAM, say). NULL for now. |
| `attempts`, `max_attempts` | Attempts that reached a client (refusals not counted), and how many it may have. A manual Retry raises `max_attempts`. |
| `attempt_id`, `client_id` | The current attempt and the client that has it. NULL while queued. |
| `error` | Why it failed or was orphaned. |
| `created_at` | When it was first submitted. |
| `queued_at` | When it last entered the queue. The scheduler serves oldest first. |
| `deadline_at` | A queued job not assigned by then fails. |
| `not_before` | Not assigned before this (backoff after a client refused it). |
| `finished_at` | When it reached a final status. |

**`job_attempts`**: one row each time a job was sent to a client.

| Column | Meaning |
|---|---|
| `id` | The attempt id, sent to the client as the request's `request_id`. |
| `job_id` | Its job. Deleted with it. |
| `number` | 1, 2, 3 … A refused attempt doesn't use up a number, so the next one has the same. |
| `client_id`, `client_label` | Who ran it. `client_id` becomes NULL if that client is deleted; the label still says who it was. |
| `model` | The model it ran. |
| `assigned_at`, `first_output_at`, `ended_at` | When it was sent, when its first output arrived, and when it ended. |
| `outcome` | `completed`, `failed`, `cancelled`, `lost`, `timed_out`, `refused` or `interrupted`. NULL while it's running. |
| `error` | What went wrong, if anything. |

Replies from before migration 3 have no job. Retrying one creates a job for
it. Deleting a conversation or a user cascades to their jobs and attempts,
so `CHAT_RETENTION_DAYS` needs nothing extra.

The migration only adds tables, so `min_compatible_version` stays 1: a
rollback to the build before the job queue starts against this schema. That
build ignores the job tables, and the next start of a newer build tidies up
after it (see "Restart recovery" in [`JOBS.md`](JOBS.md)).

## Changing the schema

1. Append a `Migration` to `MIGRATIONS` in `src/db/migrations.rs` with the
   next version number. Most migrations are a SQL string. `apply` is a
   function, so a migration can also transform data in Rust.
2. Set `min_compatible`: copy the previous migration's value for an additive
   change, or use the new version for a breaking one. Prefer additive. When
   something has to break, split it across two releases where you can: add
   the new column and write to both, then drop the old one in a later
   release.
3. Run `cargo test -p delegation-server`. The tests check that the list is
   consecutive and that `min_compatible` never goes down. They also check
   that migration 1 still matches the schema the live databases were created
   with, and that the schema accepts every `MessageStatus` (in `messages`
   and in `jobs`), `MessageRole`, `Role`, `JobKind`, `TargetKind` and
   `AttemptOutcome` the code has.
4. **Never edit a migration that has been deployed.** Databases that ran it
   won't run it again, so an edit only reaches new databases, and the two
   drift apart. Put fixes in a new migration.
5. Breaking and table-rebuild migrations need a note in the PR description:
   what breaks, and that a rollback across the migration needs the backup.

### Table rebuilds

SQLite can't change a column definition or a `CHECK` constraint in place.
Adding a variant to `MessageStatus`, `MessageRole`, `Role`, `JobKind`,
`TargetKind` or `AttemptOutcome` is one of these changes, because their
allowed values are `CHECK` lists in the schema. (A new `MessageStatus`
means rebuilding both `messages` and `jobs`.) Use
SQLite's table-rebuild procedure in a migration with
`rebuilds_tables: true`:

1. Create the new table under a temporary name.
2. Copy the rows across, listing the columns.
3. Drop the old table.
4. Rename the new table to the old name.
5. Recreate the indexes. Dropping the table dropped them.

The template is in the doc comment at the top of `migrations.rs`. The
migration runner turns foreign keys off before the transaction, runs
`PRAGMA foreign_key_check` before committing (any broken reference fails the
migration), and turns them back on afterwards.

## Backups

**Before migrating**, the server writes a consistent copy next to the
database with `VACUUM INTO`:

```text
delegation.db.pre-v<target version>-<UTC time>.bak
e.g. delegation.db.pre-v2-20261108T101500.123Z.bak
```

It keeps the newest `DB_MIGRATION_BACKUPS_KEEP` copies (default 3) and
deletes older ones. If the backup can't be written, it doesn't migrate.

**Any time**, `delegation-server backup [FILE]` makes the same kind of copy
while the server runs. With no file name, it writes
`delegation-backup-<timestamp>.db` next to the database:

```bash
docker exec delegation delegation-server backup
```

Don't copy `delegation.db` itself while the server is running. The copy
can catch the file half-written, and recent writes may still be in
`delegation.db-wal`.

## Restoring a backup

Run these on the host. The data files belong to the container's user, so
you may need `sudo`.

```bash
cd /opt/apps/delegation/data
docker stop delegation

# Keep the database you're replacing, in case you need it after all.
mv delegation.db delegation.db.replaced-$(date -u +%Y%m%dT%H%M%SZ)
rm -f delegation.db-wal delegation.db-shm     # they belong to the replaced file

cp delegation.db.pre-v2-20261108T101500.123Z.bak delegation.db   # the backup you want
docker start delegation
docker logs --tail 20 delegation
```

The image that's running has to be able to use the restored version. The
image that took the backup can, because it migrates the database again on
start. So can the build from before the upgrade, which is what a rollback
leaves running. To keep running the older build after restoring, make sure
the pipeline doesn't redeploy the newer one: fix it, or revert it on `main`.

## Development

- `dev/dev.sh reset` wipes the dev database (and the dev client's identity).
- Native runs use `data/delegation.db`. Delete it, along with any `-wal` and
  `-shm` files, to start over.
- The tests build databases at earlier versions with
  `ensure_schema(&mut conn, path, &MIGRATIONS[..n], keep)`. Follow that
  pattern to test a migration against data from the version before it.
