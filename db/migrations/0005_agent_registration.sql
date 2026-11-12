-- Agent registration (design 6.4, 6.7, 14.4, ADR-463, ADR-465): the single-use codes an
-- administrator issues, the owner of an agent, and one unrevoked agent row per key.

-- A code is kept by the SHA-256 of its text, never as the text: the input is random, so no slow
-- hash is needed. The rows stay as the record of each registration. The primary key is the hash,
-- not the tenant: a redemption knows only the code, so this table does not lead its keys with
-- tenant_id.
CREATE TABLE agent_registration_codes (
    code_sha256     bytea PRIMARY KEY CHECK (octet_length(code_sha256) = 32),
    tenant_id       uuid NOT NULL REFERENCES tenants(id),
    owner_id        uuid NOT NULL REFERENCES operators(id),
    display_name    text NOT NULL CHECK (display_name <> ''),
    -- The fingerprint of the agent key the code is bound to: SHA-256 of its SPKI, lowercase hex
    agent_key_id    text NOT NULL CHECK (agent_key_id ~ '^[0-9a-f]{64}$'),
    -- The server key the code was issued under; it must still be active when the code is used
    instruction_key_id text NOT NULL REFERENCES instruction_keys(key_id),
    created_at      timestamptz NOT NULL DEFAULT now(),
    expires_at      timestamptz NOT NULL,
    used_at         timestamptz,
    revoked_at      timestamptz,
    agent_id        uuid REFERENCES agents(id),
    CHECK (expires_at > created_at),
    CHECK (agent_id IS NULL OR used_at IS NOT NULL)
);

-- The operator an agent is registered to. Null only on rows that predate registration.
ALTER TABLE agents ADD COLUMN owner_id uuid REFERENCES operators(id);

-- One unrevoked row per key in the whole database. Rows written before this migration may hold
-- several of one key (no route wrote them, but nothing forbade it): keep the newest by
-- (registered_at, id) and revoke the rest, so that the index can be created.
DO $$
DECLARE
    revoked_count bigint;
BEGIN
    WITH ranked AS (
        SELECT id,
               row_number() OVER (
                   PARTITION BY public_key ORDER BY registered_at DESC, id DESC
               ) AS position
        FROM agents
        WHERE revoked_at IS NULL
    ),
    revoked AS (
        UPDATE agents
        SET revoked_at = now()
        FROM ranked
        WHERE agents.id = ranked.id AND ranked.position > 1
        RETURNING agents.id
    )
    SELECT count(*) INTO revoked_count FROM revoked;
    IF revoked_count > 0 THEN
        RAISE WARNING 'agent registration migration revoked % duplicate unrevoked agent rows', revoked_count;
    END IF;
END
$$;

CREATE UNIQUE INDEX agents_public_key_unrevoked ON agents (public_key) WHERE revoked_at IS NULL;
