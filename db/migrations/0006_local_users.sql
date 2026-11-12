-- Local users (design 13, 13.3, ADR-469): the passwords of the operators of the local
-- deployment profile.

-- One row per operator that can sign in with a password. The user name is operators.subject in
-- the local tenant, so this table has no tenant_id of its own: it is keyed and read by the
-- operator, and the operator row carries the tenant. password_hash is an Argon2id PHC string, so
-- the parameters and the salt live in the value; the login route and `ngr-server user` write and
-- check it, and nothing here constrains its format. The row is kept when the operator is
-- disabled. A sign-in session remembers the SHA-256 of this text, so replacing it ends them.
CREATE TABLE operator_credentials (
    operator_id     uuid PRIMARY KEY REFERENCES operators(id),
    password_hash   text NOT NULL,
    updated_at      timestamptz NOT NULL DEFAULT now()
);
