-- The key with which the server signs job instructions (design 11.1, 16.3; ADR-462). The private
-- key is a file on the server host; this table records the public half, so that a start which
-- finds a different key file refuses instead of signing under a second identity.
--
-- One key per deployment: there is no tenant_id (the first table without one). The history is
-- kept: a retired row explains old jobs.signing_key_id values.
CREATE TABLE instruction_keys (
    key_id     text PRIMARY KEY,           -- SHA-256 of the SPKI document, 64 lowercase hex digits
    public_key bytea NOT NULL,             -- the canonical DER SPKI
    created_at timestamptz NOT NULL DEFAULT now(),
    retired_at timestamptz
);

-- At most one active key: a second row with retired_at NULL is refused.
CREATE UNIQUE INDEX instruction_keys_one_active ON instruction_keys ((true)) WHERE retired_at IS NULL;
