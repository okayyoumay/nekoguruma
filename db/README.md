# Data model notes

`migrations/0001_init.sql` is the initial schema.
`migrations/0002_extensions_without_vci_profile.sql` removes `vciProfile` from the extension kinds:
VCI profiles are installed on devices outside this software (design 9.3, ADR-228).

## Design decisions

**Put tenant ID in every table**
P6 (tenant and contract management) is undecided, but retrofitting tenant ID is a large migration.
It leads the composite primary keys, which also prepares for future sharding (the XL stage in 15.1).

**Do not store raw data bodies in the DB**
`dataset_chunks` holds only object storage keys and hashes.
`chunk_format` carries a format version so that, once P1 (scale undecided) is settled,
we can migrate to a columnar format.

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
