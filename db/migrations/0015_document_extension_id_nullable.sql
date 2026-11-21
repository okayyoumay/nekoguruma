-- The document store route (design 8.2.6, 14.3, ADR-500) writes `ir_documents` rows from a
-- document alone. The document carries no ID of an extension package (design 9.4 packages come
-- with the M6 ingestion), so `extension_id` has nothing true to hold: a placeholder would look
-- like the ID of a real package. The column may be NULL, and a row the route writes leaves it so;
-- a row that ingestion of a package writes later fills it.
--
-- Only this column changes. Every other metadata column stays NOT NULL and the route fills it
-- from the document.
ALTER TABLE ir_documents ALTER COLUMN extension_id DROP NOT NULL;

COMMENT ON COLUMN ir_documents.extension_id IS
    'The extension package the document came from; NULL for a document stored on its own (ADR-500)';
