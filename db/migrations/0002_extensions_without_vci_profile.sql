-- VCI profiles are no longer extension packages (9.3, 9.4, ADR-228): they are installed on
-- devices outside this software. Drop 'vciProfile' from the allowed extension kinds.
DELETE FROM extensions WHERE kind = 'vciProfile';

ALTER TABLE extensions DROP CONSTRAINT extensions_kind_check;
ALTER TABLE extensions ADD CONSTRAINT extensions_kind_check
    CHECK (kind IN ('ir','screen','recordTemplate','message','unit'));
