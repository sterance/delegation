# The job queue

Every chat reply is a **job**: a unit of work for a client, kept in the
`jobs` table so a restart doesn't lose it. One scheduler places every job on
a client, whether it was sent to one client or to "any of my clients", and a
job whose client disconnects, goes quiet or is lost to a server restart goes
back to the queue and runs again.

Chat replies are the first and so far the only job kind. The code is in
[`src/jobs/`](src/jobs/) (types and `can_move` in `mod.rs`, SQL and restart
recovery in `store.rs`, the queue in `scheduler.rs`, placement in `pick.rs`,
the HTTP API in `api.rs`) and [`src/replies.rs`](src/replies.rs), the
executor for chat replies. The tables are described in
[`DATABASE.md`](DATABASE.md).

```
HTTP send / retry ──► submit(job) ──► jobs table (queued) + hub
                                          │
              ┌───────────── scheduler ◄──┘  woken by: submit, attempt ended,
              │  picks an eligible client     client connected / inventory changed,
              │  for each queued job (FIFO)   timer (deadlines, backoff)
              ▼
       ReplyRunner::assign(job, client) ──► chat_request ──► client
              ▲                                                │
              └──── attempt ended (done / error / lost) ◄──────┘
                         │
                         ├─ completed / failed / cancelled ─► final
                         └─ lost, timed out, refused, interrupted
                              ├─ attempts left ─► back to queued (same job)
                              └─ none left     ─► orphaned (final)
```

The database is the source of truth. The scheduler keeps an in-memory index
of the queued jobs (and of which client each running one is on) and rebuilds
it from the database at startup.

## States

A job is in one of seven states. For a chat reply, `messages.status` always
holds the same value: every write changes the job and its message in one
transaction.

| State | Meaning |
|---|---|
| `queued` | Waiting for a client. |
| `assigned` | Sent to a client; nothing back yet (it may be loading the model). |
| `running` | The first piece of output has arrived. |
| `completed` | Final. The client finished. |
| `failed` | Final. The model or runtime reported an error, the reply grew too long, it waited in the queue past its deadline, or it was refused too many times. A person can Retry. |
| `cancelled` | Final. Stop, or its conversation or owner was deleted. |
| `orphaned` | Final. Its last attempt was lost (the client went away, went quiet, or the server restarted). The partial text is kept and a person can Retry. |

`can_move` in `src/jobs/mod.rs` is the single place that decides which
changes are legal; everything else is refused and logged.

| From | To |
|---|---|
| `queued` | `assigned`, `failed`, `cancelled`, `orphaned` |
| `assigned` | `running`, `completed`, `failed`, `cancelled`, `orphaned`, **`queued`** |
| `running` | `completed`, `failed`, `cancelled`, `orphaned`, **`queued`** |
| `failed`, `orphaned` | `queued` (manual Retry) |

The two moves in bold are the **re-queue**: the attempt ended without ending
the job.

## Targets and pools

A job is aimed at a target, chosen when it's sent:

- **`{ "kind": "client", "client_id", "model" }`**: directed. The job is a
  queue entry pinned to that client. It only ever runs there, and waits for
  it if it's busy or disconnected.
- **`{ "kind": "pool", "model" }`**: pooled, "any of my clients". The job
  runs on the next free client in its owner's pool that offers the model.
  Model ids are catalog ids, so the same model has the same id on every
  client, which is what makes this meaningful.

Both go through the same queue and the same scheduler.

**Who is in a user's pool** is decided in `src/access.rs`
(`ClientAction::Pool`), like every other decision about clients:

- A user's pool is the clients they own.
- The admin's pool is also only the admin's own clients. The admin can
  still send a job straight to anyone's client (`Use`), but a pooled job
  must never land on a volunteer's machine just because the admin's own
  clients are busy.
- When lending arrives, a borrower will get `Pool` along with `Use`. The
  lines are there, commented out.

Before anything is created, a send is checked (`check_target` in
`src/chat.rs`): a directed target must name a connected client the user may
use that offers the model (it may be busy); a pooled target needs at least
one connected client in the user's pool offering the model (409 "None of
your clients offer … right now" otherwise), so a job is never queued with
nowhere to go; and the queue must hold fewer than `JOB_QUEUE_MAX` jobs
(429 with `Retry-After`). The per-user and per-conversation reply limits
count queued replies as they always did.

## Placement

A single scheduler task makes every assignment, so two jobs can never take
the same last slot. It doesn't poll. It runs a pass when:

- a job was submitted or re-queued;
- an attempt ended;
- a client connected, disconnected or sent a new inventory;
- a deadline or a backoff came due.

Each pass walks the queued jobs oldest first (by when they last entered the
queue). A job that can't be placed is skipped, so it doesn't hold up the
jobs behind it.

A client is **eligible** for a job when all of these hold:

| Rule | Why |
|---|---|
| It's connected, and its inventory has the job's model `available`, `loading` or `loaded`. Echo counts only while `INFERENCE_ECHO_MODEL` is on. | It has to be able to answer now. |
| For a pinned job, it's the pinned client. For a pooled job, it's in the owner's pool, **checked again at every placement**. | Access can change while a job waits: a client deleted, later a loan revoked. |
| Its active attempts are fewer than `INFERENCE_MAX_CONCURRENT`. | That's how many replies a client generates at once; the server sends every client the same value in `welcome`. One more would be refused. |
| All of its active attempts use the job's model. | llama.cpp loads one model at a time. A request for another model mid-reply fails with `BusyWithAnotherModel`. |
| The job's `not_before` has passed. | Backoff after a refusal. |

Among eligible clients, `pick` (`src/jobs/pick.rs`, a pure function with its
own tests) chooses in this order:

1. **Not the client that just lost this job**, unless it's the only eligible
   one. Whatever lost it there (a flaky link, a wedged runtime) may happen
   again; but if it's the only choice, it may simply have reconnected.
2. **The model already `loaded`, then `loading`, then `available`.** A warm
   model answers in seconds; a cold one can take a minute to load.
3. **Fewest active attempts.**
4. **Idle longest**, which spreads the load, then `client_id`, so the choice
   is deterministic.

(The spec listed "not the client that just lost it" third. It's first here so
that it holds even when that client is the only one with the model loaded,
which is the common case right after a timeout.)

Assignment (`ReplyRunner::assign`) trims the history to that client's
context for that model, which is why it happens at assignment and not at
submit; creates the attempt; writes the job, the message and a
`job_attempts` row in one transaction; and sends `chat_request`.

A queued job that isn't placed within `JOB_QUEUE_TIMEOUT_SECS` of entering
the queue fails: "No client offering {model} was free within {n} s." for a
pooled job, "{client} wasn't free (or connected) within {n} s." for a pinned
one. A pinned job whose client is deleted fails at once.

While a job waits, the reply's stream carries
`queue: { position, waiting_for, client_label }`. `position` is 1-based among
the queued jobs that may use one of the same clients; it's an approximation,
since it ignores which model each wants and where each will end up.
`waiting_for` is `a_free_client` when clients that could run the job are
connected but busy, and `a_client` when none is.

## Attempts and outcomes

Each time a job is sent to a client is an **attempt**, with a new
`attempt_id` (sent as the request's `request_id`) and a row in
`job_attempts`. Every attempt ends with exactly one outcome.

| Cause | Outcome | What happens to the job |
|---|---|---|
| `chat_done` | `completed` | `completed` (final). |
| `chat_error` from the model or runtime | `failed` | `failed` (final). Not retried automatically, because the same input usually fails the same way. A person can Retry. |
| Stop, or the conversation or user was deleted | `cancelled` | `cancelled` (final). |
| The reply grew past `CHAT_MAX_REPLY_CHARS` | `failed` | `failed` (final). |
| The client's connection closed | `lost` | Re-queued if attempts remain, else `orphaned`. |
| Nothing from the client for `CHAT_IDLE_TIMEOUT_MS` | `timed_out` | `chat_cancel` is sent, then re-queued if attempts remain, else `orphaned`. |
| The client refused because it's busy, or its outbound queue was full | `refused` | Re-queued with `not_before = now + JOB_REFUSED_BACKOFF_MS`. Doesn't use up an attempt. After `JOB_MAX_REFUSALS` refusals in a row the job fails, so a confused client can't loop forever. |
| The server restarted mid-attempt | `interrupted` | Re-queued at startup if attempts remain, else `orphaned`. |

**Re-queueing** a job, in one write:

- it goes from `assigned` or `running` back to `queued`;
- the reply's partial text is cleared: the next attempt starts again;
- the message's client, client label and model are cleared;
- `queued_at` is set to now, so it re-enters the queue at the back;
- `deadline_at` is set to now plus `JOB_QUEUE_TIMEOUT_SECS`.

The reply's hub survives, so streams stay attached: they get a `requeued`
event (`{ attempt, max_attempts, reason, previous_client_label }`), then a
`status` (queued), then the fresh reply.

**Pinned jobs re-queue to the same client.** A pinned job whose client
dropped waits for it to reconnect, up to the deadline. Clients reconnect
within seconds after a network blip or a restart, so this turns most manual
retries into automatic ones.

**Attempts.** `JOB_MAX_ATTEMPTS` counts every attempt that reached a client,
except refusals. When the last one is lost, the job ends `orphaned` with an
error that says so: "The client disconnected before the reply finished (3
attempts)." A manual Retry gives the job a fresh budget:
`max_attempts = attempts + JOB_MAX_ATTEMPTS`. By default it keeps the job's
original target, so a pooled reply retries pooled.

**Refusals** come from the client's `chat_error`, which carries an optional
`code` (`protocol::ChatErrorCode`):

| `code` | Treated as |
|---|---|
| `busy`, `busy_with_another_model` | `refused` |
| `model_unavailable` | `refused` for a pooled job (another client may have the model); `failed` for a pinned one |
| none (an older client), or `other` | `failed`, as before the queue existed |

A server with the queue works with clients that don't send a code: their
errors are failures, as they always were. Deploy the server first, then the
clients.

## Stale results

Chunks and endings are applied only from a job's **current attempt**, on the
connection that attempt was sent to. Anything else is dropped and logged at
debug level. Re-queueing needs nothing more than that: a client that
reconnects and keeps sending for an attempt that has since been re-queued is
talking about an attempt that isn't current, so its chunks, and even its
`chat_done`, are ignored, and the reply being generated elsewhere isn't
affected.

## Restart recovery

`store::recover` runs at startup, before anything is served. Nothing is
running any more, so for each active job:

- its open attempt is closed as `interrupted`;
- if attempts remain, it goes back to `queued` (text cleared, fresh
  deadline);
- otherwise it ends `orphaned`, with "Interrupted by a server restart",
  keeping its partial text.

Every queued job then gets a fresh deadline and is loaded into the
scheduler. Clients take a few seconds to reconnect after a restart (their
backoff tops out at `RECONNECT_BACKOFF_MAX_MS`, 30 s by default); jobs simply
wait for them.

The migration that added the job tables only adds tables, so a deploy that
fails its health check rolls back to a build that knows nothing about jobs.
That build orphans active replies on its own startup and creates replies
without jobs. Recovery handles what it leaves behind:

- A reply that's active but has no job is orphaned, as that build would.
- Where a job is active but its reply has already finished, the reply wins:
  its status is copied to the job, the job's open attempt is closed as
  `interrupted`, and nothing is re-queued.

A reply from before the queue has no job; retrying it creates one.

## Settings

| Setting | Default | Range | Meaning |
|---|---|---|---|
| `JOB_QUEUE_TIMEOUT_SECS` | `120` | 5–3600 | How long a job may wait in the queue each time it's queued |
| `JOB_MAX_ATTEMPTS` | `3` | 1–10 | Attempts per submit or Retry, refusals not counted |
| `JOB_MAX_REFUSALS` | `5` | 1–50 | Refusals in a row before the job fails |
| `JOB_REFUSED_BACKOFF_MS` | `2000` | 100–60000 | Wait after a refusal before that job can be placed again |
| `JOB_QUEUE_MAX` | `200` | 1–10000 | Queued jobs across all users before submits get 429 |

`JOB_QUEUE_TIMEOUT_SECS` × 1000 must be at least `RECONNECT_BACKOFF_MAX_MS` +
`LIVENESS_TIMEOUT_MS`, checked at startup. Otherwise a pinned job could fail
before its client finishes even one reconnect cycle. With the defaults
that's 120 000 ≥ 34 000.

Related settings from elsewhere: `INFERENCE_MAX_CONCURRENT` (slots per
client), `CHAT_IDLE_TIMEOUT_MS` (when a quiet attempt times out), and
`CHAT_MAX_ACTIVE_REPLIES_PER_USER`.

## API

- `POST /api/conversations/{id}/messages` and
  `POST /api/messages/{id}/retry` take either kind of target.
- `GET /api/chat/models` returns `{ pools, clients }`: one pool entry per
  model offered by at least one connected client in the user's pool (with
  how many clients offer it and how many have it loaded), and the per-client
  entries.
- `GET /api/jobs?status=active|recent&limit=`: the user's jobs, each with
  its attempts (client label, model, times, outcome, error), how long it
  waited in the queue and how long its newest attempt ran. The admin can add
  `all=1` for everyone's. `GET /api/jobs/{id}` returns one. Both return
  metadata and the conversation id only, never message text.
- `GET /api/clients` reports `active_jobs` (attempts running on each client)
  and `queued_for` (queued jobs pinned to it).

## Adding a second job kind

Not built, but the queue is meant to carry other work (batch prompts,
non-inference GPU jobs). What it would take:

**On the server**

- A `JobKind` variant, and a migration that rebuilds `jobs` to extend the
  `kind` CHECK list (the test that compares the lists with the enums fails
  until there is one). Its jobs need their own subject instead of
  `message_id`, which is why that column is nullable and required only for
  `chat_reply`.
- An **executor**: what `ReplyRunner` is for chat replies. It creates the
  job row, calls `Scheduler::enqueue`, and implements what the scheduler
  calls back: `assign` (start an attempt on the chosen client and send the
  request), `fail_queued` (the job ran out of time in the queue) and
  `queue_changed`. It calls `Scheduler::release` when a job ends and
  `enqueue` again to re-queue it. `Scheduler::pass` dispatches a placement
  to the executor for its kind.
- Its writes must change `jobs` and `job_attempts` the way the reply
  runner's do, through one writer, so `store::recover` and the jobs API keep
  working. `recover` re-queues by job; the part that resets the reply's
  message is specific to `chat_reply` and needs a counterpart.
- If the kind needs more than "offers the model" (minimum VRAM, say), that
  goes in `requirements_json` and becomes one more eligibility rule when the
  scheduler builds its candidates.

**On the client**

- A new request message in `protocol/` (and its progress, result and error
  messages), as the inference spec planned, and a new method on `Inference`
  to run it.
- The session routes the request to that method the way it routes
  `chat_request` to `chat::respond`, and reports `busy` with a code so the
  server re-queues instead of failing.

**In between**

- `ws.rs` hands the new result messages to the executor.
- The stale-result rule (current attempt, on the connection it was sent to)
  applies unchanged.
- The dashboard's Jobs page shows any kind; it only needs a label for it.

## Lending and fairness

Pool access is checked again at every placement, so when lending exists,
revoking a loan stops new placements on that client straight away. What
happens to an attempt already running on it is still the lending spec's open
question.

Placement is first in, first out across all users, with no priorities or
preemption. That's fine while every pool is its owner's own clients: users
never compete for a client (apart from the admin sending directly to
someone's). Once clients can be lent, a borrower and an owner will compete
for the same client, and the queue will need fairness between users.

Also out of scope for now: hardware requirements beyond "offers the model",
a per-client concurrency limit, installing a model on an idle client so it
can take queued work, and splitting one job across clients.
