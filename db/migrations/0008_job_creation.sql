-- Job creation (design 4.4, 5.1, 12, ADR-477): what `POST /api/v1/jobs` stores beyond the first
-- schema.
--
--   request_hash  SHA-256 of the normalized request that created the job. A later request with
--                 the same idempotency key and the same operator replays the stored job only
--                 when its own hash equals this one; a row without a hash (one inserted by other
--                 means) is never replayed. 32 bytes when present.
--   ir_digests    the job's IR documents, as lowercase hex SHA-256, in ascending order without
--                 duplicates: what the signed instruction's document hashes were made from.
--                 Replaces the single, never-written ir_digest.
--   kind          also allows `identify`, a job kind the first schema predates (design 16.3).
ALTER TABLE jobs
    ADD COLUMN request_hash bytea CONSTRAINT jobs_request_hash_length CHECK (octet_length(request_hash) = 32),
    DROP COLUMN ir_digest,
    ADD COLUMN ir_digests text[] NOT NULL DEFAULT '{}';

ALTER TABLE jobs DROP CONSTRAINT jobs_kind_check;
ALTER TABLE jobs ADD CONSTRAINT jobs_kind_check CHECK (kind IN
    ('acquire','monitorSession','monitorCapture','runSequence','writeSettings','reprogram','identify'));

COMMENT ON COLUMN jobs.id IS
    'UUIDv7 made by the server when it creates the job (design 4.4); a client names a job only by its idempotency key';
