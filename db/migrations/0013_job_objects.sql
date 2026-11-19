-- The objects a job's agent uploads over the data channel (design 5.2, 14.3, ADR-493 item 7): one
-- row per upload, kept after it finishes. The bytes are in the object store; the row names them.
--
--   id           the route's uploadId; the store's key and upload ID never leave the server
--   job_id       the job the object belongs to, with the ownership generation it was begun under
--   kind         `result` (the run's final state) or `commLog` (the job's communication log)
--   agent_id     the agent that began the upload; only that agent reads or writes the row
--   storage_key  the key the object store issued (a version 4 UUID in lowercase text)
--   upload_id    the store's multipart upload ID: a bearer of access to the upload, never answered
--                or logged
--   part_bytes   the part size the upload was begun with; a later change of the setting does not
--                reinterpret an open upload
--   size_bytes   the declared length, 0 to 4 GiB; the digest is the declared SHA-256 as 64
--                lowercase hex digits
--   state        open -> completing -> finished, or failed. The begin route writes `open` only;
--                the other states are the completion's
--   finished_at  set when the state is `finished`, and only then
--
-- The partial unique index holds the idempotency of begin: one row that is not `failed` per job,
-- generation and kind, so two racing begins insert one row and a failed upload can be begun again.
-- The foreign keys have no cascade: a job, an agent or a tenant that has objects is not deleted
-- under them.
CREATE TABLE job_objects (
    tenant_id       uuid NOT NULL REFERENCES tenants(id),
    id              uuid NOT NULL,
    job_id          uuid NOT NULL,
    generation      bigint NOT NULL CHECK (generation >= 0),
    kind            text NOT NULL CHECK (kind IN ('result','commLog')),
    agent_id        uuid NOT NULL REFERENCES agents(id),
    storage_key     text NOT NULL CHECK (char_length(storage_key) = 36),
    upload_id       text NOT NULL CHECK (char_length(upload_id) BETWEEN 1 AND 1024),
    part_bytes      bigint NOT NULL CHECK (part_bytes > 0),
    size_bytes      bigint NOT NULL CHECK (size_bytes BETWEEN 0 AND 4294967296),
    digest          text NOT NULL CHECK (digest ~ '^[0-9a-f]{64}$'),
    state           text NOT NULL CHECK (state IN ('open','completing','finished','failed')),
    created_at      timestamptz NOT NULL DEFAULT now(),
    finished_at     timestamptz,
    PRIMARY KEY (tenant_id, id),
    UNIQUE (storage_key),
    FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    CHECK ((state = 'finished') = (finished_at IS NOT NULL))
);

CREATE UNIQUE INDEX job_objects_live ON job_objects (tenant_id, job_id, generation, kind)
    WHERE state <> 'failed';
