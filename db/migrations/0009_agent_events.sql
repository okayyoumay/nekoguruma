-- The agent's guaranteed events (design 5.3, 5.4, ADR-478 item 6, ADR-479): one row per event the
-- server has stored, keyed by the agent and the number the agent's outbox gave the event. The
-- cursor row of `agent_event_cursor` is moved in the same transaction that inserts the rows, so
-- the cursor names the highest number stored (or stored once and deleted since).
--
--   seq                the event's number in the agent's outbox, at least 1
--   kind               `jobState` or `hmiRequest` as the agent wrote it; no CHECK, so a later
--                      kind needs no migration
--   job_id             the job the event names, when its text is a canonical UUID; null otherwise.
--                      No foreign key: an event for a job the server does not know is kept
--   body               the exact text of the control frame the agent sent, so a repeat of the
--                      event can be compared byte for byte (not jsonb: jsonb refuses the \u0000
--                      escape and does not keep the text)
--   occurred_at_agent  the device time the event states, for reference only (design 16.3); null
--                      when absent or not a time PostgreSQL is known to accept
--   received_at        the server's time of the insert, the authoritative one
--
-- The two indexes serve the retention pass and the VIN deletion (design 4.3, 15.2): the rows of
-- one job, and the rows older than a bound. Deleting rows does not move the cursor, and a repeat
-- of a deleted row is answered with the cursor. The table is not partitioned.
CREATE TABLE agent_events (
    tenant_id         uuid NOT NULL,
    agent_id          uuid NOT NULL,
    seq               bigint NOT NULL CONSTRAINT agent_events_seq_positive CHECK (seq >= 1),
    kind              text NOT NULL,
    job_id            uuid,
    body              text NOT NULL,
    occurred_at_agent timestamptz,
    received_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, agent_id, seq)
);

CREATE INDEX agent_events_by_job ON agent_events (tenant_id, job_id) WHERE job_id IS NOT NULL;
CREATE INDEX agent_events_by_received ON agent_events (tenant_id, received_at);
