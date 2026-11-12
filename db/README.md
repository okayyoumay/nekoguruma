# Data model notes

`migrations/0001_init.sql` is the initial schema.
`migrations/0002_extensions_without_vci_profile.sql` removes `vciProfile` from the extension kinds:
VCI profiles are installed on devices outside this software (design 9.3, ADR-228).

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
- **Session tokens**: OIDC access tokens are not stored

## Column values

- `ir_documents.source_format`: `odx` or `csv-starlark` (the proprietary format, ADR-254).
  The comment in `0001_init.sql` still names the earlier `csv-js`; the column is free text, so
  no migration is needed.
