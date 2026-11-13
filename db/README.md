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

**Do not store raw data bodies in the DB**
`dataset_chunks` holds only object storage keys and hashes.
`chunk_format` carries a format version. The first version, `records/1`, stores the
responses received from the vehicle with their timestamps (ADR-236); a columnar format for
longer recordings can be added later as a new version without migrating existing chunks.

**Store signed instructions in jobs**
Do not re-sign on every delivery. Resends carry the same bytes, which keeps
agent-side duplicate detection (5.3) simple.

**Build queues from partial indexes**
`jobs_pending` and `jobs_open` are partial indexes. They find delivery targets and
reconciliation targets on reconnection without scanning all jobs (15.1).

**Version work records per item**
The unit of 3-way merge (4.4) is the item, so `work_record_items.version` exists.
`origin` distinguishes auto-filled values (from the vehicle, not editable) from manual entries.

**Partition ever-growing tables**
`change_feed` and `audit_log` are `RANGE` partitioned. They are operated by setting a retention period
and dropping old partitions (15.1).

## Tables not created

- **Dedicated job queue table**: partial indexes on `jobs` are enough (S/M stages in 15.1).
  Moving to `LISTEN/NOTIFY` at the L stage needs no extra table either
- **Monitoring frames**: display only, not recorded (4.6).
  Only captured windows go into `datasets`
- **Session tokens**: OIDC access tokens are not stored. Nor are the operators' login sessions
  (ADR-469): the server keeps them in memory, so a restart signs everyone out

## Column values

- `ir_documents.source_format`: `odx` or `csv-starlark` (the proprietary format, ADR-254).
  The comment in `0001_init.sql` still names the earlier `csv-js`; the column is free text, so
  no migration is needed.
- `agents.last_seen_at`: the control channel writes it at connect and at disconnect only (design
  15.1, ADR-468). The comment in `0001_init.sql`, which says the column is batch-written, no
  longer holds; a merged migration cannot change it (ADR-458 item 6).
