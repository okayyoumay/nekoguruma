-- The digest and storage key of the two tables that map a content address to an object (design
-- 5.2, 8.2.6, 11.2). The download route reads them: it looks a tenant's digest up and signs a URL
-- for the key the row holds, so both must be in the one form the route compares with.
--
--   digest       the object's SHA-256 as 64 lowercase hex digits, bare (the form the signed job
--                instruction, `jobs.artifact_digest` and `job_objects.digest` use). The comment
--                of `artifacts.digest` in 0001 shows a `sha256:` prefix that no code has ever
--                written; that migration is applied and stays as it is, so the comment is set
--                here.
--   storage_key  the key the object store issued (a version 4 UUID in lowercase text, 36
--                characters, the length `job_objects.storage_key` also checks), and unique: two
--                rows never name one object.
--
-- A row of either table exists only for an object the store has read back and found to match its
-- digest; the writer of these tables (a later item) keeps that, and the download route only reads.
-- The metadata columns of `ir_documents` stay NOT NULL; the writer fills them.
ALTER TABLE ir_documents
    ADD CONSTRAINT ir_documents_digest_hex CHECK (digest ~ '^[0-9a-f]{64}$'),
    ADD CONSTRAINT ir_documents_storage_key_len CHECK (char_length(storage_key) = 36),
    ADD CONSTRAINT ir_documents_storage_key_unique UNIQUE (storage_key);

ALTER TABLE artifacts
    ADD CONSTRAINT artifacts_digest_hex CHECK (digest ~ '^[0-9a-f]{64}$'),
    ADD CONSTRAINT artifacts_storage_key_len CHECK (char_length(storage_key) = 36),
    ADD CONSTRAINT artifacts_storage_key_unique UNIQUE (storage_key);

COMMENT ON COLUMN ir_documents.digest IS 'SHA-256 of the document, 64 lowercase hex digits';
COMMENT ON COLUMN artifacts.digest IS 'SHA-256 of the artifact, 64 lowercase hex digits';
