# Data model notes

`migrations/0001_init.sql` is the initial schema.
`migrations/0002_extensions_without_vci_profile.sql` removes `vciProfile` from the extension kinds:
VCI profiles are installed on devices outside this software (design 9.3, ADR-228).
`migrations/0003_instruction_keys.sql` adds `instruction_keys`, the public half of the server's job
instruction key (design 11.1, ADR-462).
`migrations/0004_agent_mode_device.sql` renames the agent mode `machine` to `device` in
`agents.mode` (design 6.7, ADR-463); the column `machine_hint` keeps its name.
`migrations/0005_agent_registration.sql` adds `agent_registration_codes`, the nullable
`agents.owner_id` and the unique index `agents_public_key_unrevoked` (design 6.4, 6.7, ADR-463,
ADR-465).
`migrations/0006_local_users.sql` adds `operator_credentials`, the Argon2id password hashes of the
local users (design 13.3, ADR-469).
`migrations/0007_job_delivery.sql` adds the three delivery columns of `jobs` (design 5.3, 14.1,
ADR-476).
`migrations/0008_job_creation.sql` adds `jobs.request_hash`, replaces `jobs.ir_digest` with
`ir_digests`, lets `jobs.kind` be `identify` and comments `jobs.id` (design 4.4, 5.1, ADR-477).
`migrations/0009_agent_events.sql` adds `agent_events`, the agent's stored outbox events (design
5.3, 5.4, ADR-478 item 6, ADR-479).
`migrations/0010_job_procedure_code.sql` adds `jobs.procedure_code`, the procedure's own code of a
failed job (design 5.6, ADR-475, ADR-481).
`migrations/0011_default_partitions.sql` gives `change_feed` and `audit_log` a default partition, so
their first insert no longer fails for want of a partition.
`migrations/0012_confirmation_defaults.sql` adds `confirmation_defaults` and seeds it (design 6.3, ADR-488).

## How migrations are applied

The server embeds the files of `migrations/` when it is built (`sqlx::migrate!`; `build.rs` makes
cargo rebuild the crate when the directory changes) and applies the ones the database lacks when
it starts, before it listens (ADR-458). Nothing else applies them. sqlx records each applied
migration in the table `_sqlx_migrations` (version, description, checksum of the file's bytes,
success flag) and runs each file in one transaction.

- Names are `NNNN_snake_case.sql`: the digits before the first underscore are the version, and
  versions apply in numeric order.
- A merged file is immutable, byte for byte. The recorded checksum is compared at every start,
  and a server whose embedded file differs refuses to start; a fix is a new file.
- Line endings are LF: `.gitattributes` pins `migrations/*.sql`, since a checkout that converts
  them would change the checksum. A test fails when an embedded migration contains a carriage
  return.
- A database that holds a migration the server does not carry (a newer server migrated it)
  refuses the start of the older one, so a downgrade needs the database restored.
- Two servers starting together are safe: sqlx's advisory lock lets one apply the migrations and
  the other find nothing to do. The lock is held per session and counts how often it
  was taken, so after a failed run the server closes that connection and exits instead of
  retrying. PostgreSQL's own `lock_timeout` and `statement_timeout` bound a run to 60 seconds.

## Design decisions

**Put tenant ID in every table**
P6 (tenant and contract management) is undecided, but retrofitting tenant ID is a large migration.
It leads the composite primary keys, which also prepares for future sharding (the XL stage in 15.1).

**One instruction key per deployment (the exception to the tenant rule)**
`instruction_keys` has no tenant ID: the server's instruction key belongs to the deployment, not
to a tenant (ADR-461 Decision item 7), so it is an exception to the rule above (`tenants` itself has none either). `key_id` (the SHA-256
of the SPKI, 64 lowercase hex digits, the value of `jobs.signing_key_id`) is the primary key, and a
partial unique index allows one row with `retired_at` null, so the database decides which key is
active when two servers start together. Retired rows stay as history. The private key is not in
the database: it is an owner-only file (ADR-462).

**Registration codes are keyed by hash (another exception to the tenant rule)**
`agent_registration_codes` has the SHA-256 of the code as its primary key and no index besides:
a redemption knows only the code, so the tenant cannot lead the key. Only the hash is stored,
never the code; `tenant_id` and `owner_id` say whom the code registers for, `agent_key_id` is the
fingerprint of the agent key it is bound to, and `instruction_key_id` the server key it was issued
under. Rows stay after use, expiry or revocation as the record of the registration. The lifetime
bound (1 to 60 minutes) is not a CHECK: the issuing command owns it.

**One unrevoked agent row per key, in the whole database**
`agents_public_key_unrevoked` is a partial unique index on `agents (public_key) WHERE revoked_at IS
NULL`, so it spans tenants: registering a key in one tenant revokes its row in another (only the
key's holder can cause that). `agents.public_key` is the canonical DER SPKI of the agent key.
Migration 0005 meets databases that already hold duplicate unrevoked rows of one key: it keeps the
newest by `(registered_at, id)`, sets `revoked_at` on the others and reports the count as a
warning before it creates the index. `agents.owner_id` (the operator the agent is registered to) is
null only on rows that predate registration.

**Passwords are in their own table, without a tenant ID (another exception to the tenant rule)**
`operator_credentials` is keyed by `operator_id` alone and holds the Argon2id PHC string of a
local user's password (ADR-469). Like the registration codes it is an exception to the tenant
rule: the operator row it references already carries the tenant, and every read goes through the
operator. A login session remembers the SHA-256 of the stored string, so a new password ends the
sessions of the old one; `updated_at` is for the administrator and is compared with nothing.

**A job's ID is the server's, and its idempotency key is the client's**
The job's ID (`jobs.id`, UUIDv7) is made by the server when it creates the job; the first schema's
comment called it client-generated, and the column's comment now says the server makes it (design
4.4). The client's token for a job is `idempotency_key`, unique per tenant and kept for the row's
life. `request_hash` is the SHA-256 of the normalized request that created the job (32 bytes, a
CHECK; null for a row inserted by other means, which no request replays): a request that finds its
key taken replays the stored job only for the same operator with an equal hash (ADR-477).
`ir_digests` holds the job's IR documents as lowercase hex SHA-256, ascending and without
duplicates, the list the signed instruction's document hashes were made from; it replaces the
single `ir_digest`, which nothing wrote or read. `jobs_kind_check` also allows `identify`, a kind
the protocol has (design 16.3).

**Uploaded objects: `job_objects` (migration 0013, ADR-493 item 7)**
One row per upload of a job's `result` or `commLog` object, kept after it finishes. It names the
object (`storage_key`, unique, the key the store issued) and the multipart upload (`upload_id`,
the store's, a bearer: never answered or logged), the declared length (0 to 4 GiB) and SHA-256,
the part size the upload was begun with, and a state: `open`, `completing`, `finished` or `failed`.
The begin route writes `open`; the others are the completion task's, each a conditional
`UPDATE` on the state it expects: `open` to `completing` before the store's completion is called,
then from `completing` to `finished` (with `finished_at`) after the read-back verified the object,
back to `open` after a part problem, or to `failed` after a mismatch or when the upload and the
object are both gone (ADR-493 items 6 and 19). A store call that fails with an error that is not a part problem or a mismatch leaves
`completing`, which a repeated request verifies again. `finished_at` is set exactly when
the state is `finished` (a CHECK). A partial unique index on `(tenant_id, job_id, generation,
kind) WHERE state <> 'failed'` holds the idempotency of begin: two racing begins insert one row
(`ON CONFLICT ... DO NOTHING` names that index), and a `failed` row leaves the slot free for a new
begin. The foreign keys (tenant, agent, and the job through `(tenant_id, job_id)`) have no cascade:
a job, an agent or a tenant that has objects cannot be deleted under them.

**Do not store raw data bodies in the DB**
`dataset_chunks` holds only object storage keys and hashes.
A `storage_key` column holds the key the object store issued (`begin()`), a lowercase version 4
UUID, the object's name in the store's bucket and not a path. A row that keeps an upload open also
keeps the upload ID `begin()` returned (a bearer of access to the upload: never logged).
`chunk_format` carries a format version. The first version, `records/1`, stores the
responses received from the vehicle with their timestamps (ADR-236); a columnar format for
longer recordings can be added later as a new version without migrating existing chunks.

**Store signed instructions in jobs**
Do not re-sign on every delivery. Resends carry the same bytes, which keeps
agent-side duplicate detection (5.3) simple.

**Build queues from partial indexes**
`jobs_pending` and `jobs_open` are partial indexes. They find delivery targets and
reconciliation targets on reconnection without scanning all jobs (15.1).

**Delivery is recorded in three columns of jobs, and `state` stays the 5.6 state**
`delivery_attempts`, `delivery_sent_at` and `delivered_at` (migration 0007, ADR-476) say how often
the stored instruction was put on a connection, when the latest mark was made and when the agent
acknowledged one. A job is `received` until the agent reports a state, whatever its delivery.
The server's queue takes a job with `FOR UPDATE SKIP LOCKED`, joined to its agent (not revoked),
and marks it in the transaction that queued the frame; an instance takes only the jobs of agents
connected to it, the oldest jobs of each agent ranked within the agent and 64 in all by rank (every agent's first job before any agent's second), and two instances never hold one row at once (a commit
that fails after the frame was queued leaves the row as it was before the send, or at the new mark if it went through though it was reported as failed, so it can be sent again). `delivery_sent_at`
is the time of the mark statement; the answer's writes and a release match the row against the marks the delivery expects (the earlier value, with the earlier attempts, or its own, with one more, as a failed commit may or may not have happened). A mark that is not acknowledged is released when its connection is no longer
current, and a mark older than the lease (90 seconds) makes the job takeable again: delivery is
at least once, and a resend carries the same stored bytes under a new command ID.

**Events are stored as the agent's exact frame text, beside a cursor**
`agent_events` (migration 0009, ADR-479) holds one row per guaranteed event, keyed by tenant,
agent and the outbox number `seq`, and `agent_event_cursor.last_seq` is the highest number stored.
Both change in one transaction that first locks the cursor row (`FOR UPDATE`), so two server
instances serve one agent in turn. `body` is the frame's text, not `jsonb`, because a repeat is
compared with it byte for byte and `jsonb` refuses the `\u0000` escape; `kind`, `job_id` (only
when the text is a canonical UUID; no foreign key) and `occurred_at_agent` (only when a strict
check passes) are read out of it. `received_at` is the database's clock. The detail an event
carries is vehicle data (design 4.3): the table is shaped for the retention pass and the VIN
deletion, with an index on `(tenant_id, job_id)` for a job's rows and one on
`(tenant_id, received_at)` for age. Removing rows never lowers the
cursor, and a repeat of a removed row is acknowledged with the cursor, not stored again.

**A job's reported state is written to its row by the transaction that stores the event**
The server applies a stored `jobState` event to the sender's own job (ADR-481): `state`,
`last_reported_at`, `delivered_at` (set once), and for an end `finished_at`, `failure_code`,
`procedure_code` (migration 0010; only for a failed job with `failure_code = 'procedure'`, 1 to 128 bytes) and
`failure_detail` (at most 1024 bytes; U+0000 replaced by U+FFFD, empty as null). These columns carry
no CHECK: the server keeps only what its rule allows, and a bound in the column would turn a bad
report into a failure of the agent's whole transaction. `failure_detail` is vehicle data (design
4.3): the retention and the VIN deletion of design 4.3 must cover it and the job's stored
events; no pass does yet. A job the agent reports as needsOnSiteRecovery has
`finished_at` set; a later report that reopens it clears `finished_at` and the failure columns.
`progress_permille` is not written from events.

**`vehicle_locks`: one holder per tenant and VIN, expiry from the job kind**
`vehicle_locks` (design 8.8) holds at most one row per `(tenant_id, vin)`, the server's lock on a
vehicle. `POST /api/v1/jobs` takes it for a job that names a VIN, in the transaction that inserts
the job (ADR-480). The server sets `expires_at` to the job's start deadline plus an allowance for
the kind (the column has no default; no migration is needed):

| Kind | Allowance after the start deadline |
|---|---|
| `acquire` | 600 seconds |
| `writeSettings` | 1800 seconds |

A row whose `expires_at` has passed is replaced by the next take for the VIN; no sweeper deletes
expired rows. The job's end deletes its own row, whether the server writes it (the delivery queue's ends, ADR-480, and the
expiry of a job past its start deadline, ADR-487) or the agent reports it (ADR-481); a reopened needsOnSiteRecovery job holds no row. A new job
kind that the route creates needs a `confirmation_defaults` row from a migration and its
`Kind::lock_secs` arm.
The columns hold no foreign keys and a stale row keeps its VIN until a take replaces it.

**`audit_log`: one row per recorded action, written by the transaction that does it**
The server writes a row inside the transaction of the action, so a rolled-back action leaves none
(ADR-484). Read the rows of a job or a tenant in the order `(recorded_at, id)`: `id` is a
`bigserial` and breaks ties between rows of one instant. `(recorded_at, id)` is not the commit
order: `recorded_at` is the transaction's start time, earlier than its commit by any lock waits, so a
reader that pages by keyset must allow for rows that commit late. Columns as the server fills them:
`tenant_id`, `operator_id`, `agent_id` and `job_id` name the job's own values; `vin` is the job's
VIN in clear, NULL when the job has none, and is the one column a VIN deletion redacts (design
4.3); `action` is free text with no CHECK, so a new action needs no migration; `confirmation_level`
is the job's level; `on_site_approved` and `agent_reported_at` are NULL for `jobCreated`; `offline_executed` is false; `recorded_at` is the database's
`now()` (the job's `created_at` for `jobCreated`, since one transaction has one `now()`); `detail`
is JSON that never holds a VIN or a value derived from it. Nothing enforces that rows are never
updated or deleted; the server only writes them. The actions written today:

- `jobCreated` (`POST /api/v1/jobs`, one row per created job, none for a replay or a refusal):
  `detail` has `kind`, `irDigests` (the sorted unique hex digests), `artifactDigest` when the job
  has one and `signingKeyId`. It holds no request hash, idempotency key or VCI ID.

**Version work records per item**
The unit of 3-way merge (4.4) is the item, so `work_record_items.version` exists.
`origin` distinguishes auto-filled values (from the vehicle, not editable) from manual entries.

**Partition ever-growing tables**
`change_feed` and `audit_log` are `RANGE` partitioned. They are operated by setting a retention period
and dropping old partitions (15.1). Each has a default partition (`change_feed_default`,
`audit_log_default`, migration 0011) that takes every row no range partition takes; with no range
partition yet, it takes them all. Attaching a range partition checks the default partition for rows
in that range, so those rows are moved out of it before the range partition is attached.

## Tables not created

- **Dedicated job queue table**: partial indexes on `jobs` are enough (S/M stages in 15.1).
  Moving to `LISTEN/NOTIFY` at the L stage needs no extra table either
- **Monitoring frames**: display only, not recorded (4.6).
  Only captured windows go into `datasets`
- **Session tokens**: OIDC access tokens are not stored. Nor are the operators' login sessions
  (ADR-469): the server keeps them in memory, so a restart signs everyone out

## Column values

- Seed data (ADR-488). `confirmation_defaults (kind, level)` is product-wide (no tenant ID) and
  holds the default confirmation level of each job kind (design 6.3, 5.1): `acquire`,
  `monitorSession` and `monitorCapture` are `notifyOnly`, `runSequence` and `writeSettings`
  are `gracePeriod`, `reprogram` is `onSiteApproval`. `identify` has no row, since no route creates
  it. A job creation reads its level here and fails without a row. The design's row for a firmware
  write to an unattended device is an agent-side condition and not a kind, so it is not seeded.
  Organization and per-agent policies may only tighten these defaults (a later item).
- Roles stay the `CHECK` list on `operator_roles.role` and are not a table (ADR-488). Their
  meaning (design 6.8, axis 1): `viewer` views acquired data; `worker` is the Operator role
  (acquisition, configuration value writes, work records); `reprogrammer` writes ECU firmware;
  `remote` executes while not on site (with the agent's own remote permission); `approver` is the
  approving side of two-person approval; `admin` publishes artifacts, sets policy, registers
  agents, fetches agents' operational logs, raises an agent's log level for a while and reads
  the SOVD `logs` resource.
- The preset default is the column default of `agents.preset`, `personal` (design 6.6, 13.3); the
  other presets are `personalRemote` and `shared`. Relaxing a preset needs action on the device.

- `ir_documents.source_format`: `odx` or `csv-starlark` (the proprietary format, ADR-254).
  The comment in `0001_init.sql` still names the earlier `csv-js`; the column is free text, so
  no migration is needed.
- `agents.last_seen_at`: the control channel writes it at connect and at disconnect only (design
  15.1, ADR-468). The comment in `0001_init.sql`, which says the column is batch-written, no
  longer holds; a merged migration cannot change it (ADR-458 item 6).
