-- The procedure's own code of a failed job (design 5.6, ADR-475, ADR-481): the server applies a
-- job's reported state to its row, and a failure the procedure ended itself (failure code
-- `procedure`) carries a code of the procedure's, apart from the agent's own `failure_code`.
--
--   procedure_code  1 to 128 bytes, set only with `failure_code = 'procedure'`; null otherwise.
--                   No CHECK: the server writes only what its rule keeps (a code that does not
--                   fit is left out, never cut), and a bound in the column would make a bad
--                   report fail the agent's whole transaction
ALTER TABLE jobs ADD COLUMN procedure_code text;
