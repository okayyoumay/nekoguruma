-- An operator's request for the communication log of one job's run (design 8.11, ADR-508): one
-- row per job and ownership generation. The row is what lets the server store a log; the begin
-- route of the data channel refuses a `commLog` upload for a job without one.
--
--   job_id, generation  the job and the ownership generation whose log is asked for
--   agent_id            the agent the job belongs to; only that agent's begin counts the row
--   requested_by        the operator who asked
--   requested_at        when the row was written
--   state               requested -> accepted, refused or expired. `accepted` is the agent's
--                       answer that the log is queued for upload; `refused` is its answer that
--                       it cannot send one (`refusal` says why); `expired` is a request nobody
--                       answered before its limit, which stops the server's delivery of it.
--                       A stored log is not a state: it is the job's `job_objects` row
--   refusal             the agent's reason, set when the state is `refused` and only then
--   sent_at             when the server last sent the command to the agent
--   answered_at         when the agent's answer was applied
--
-- The foreign keys have no cascade: a job, an agent or an operator that has requests is not
-- deleted under them. The partial index serves the delivery's scan of one agent's open requests.
CREATE TABLE comm_log_requests (
    tenant_id       uuid NOT NULL REFERENCES tenants(id),
    job_id          uuid NOT NULL,
    generation      bigint NOT NULL CHECK (generation >= 0),
    agent_id        uuid NOT NULL REFERENCES agents(id),
    requested_by    uuid NOT NULL REFERENCES operators(id),
    requested_at    timestamptz NOT NULL DEFAULT now(),
    state           text NOT NULL DEFAULT 'requested'
        CHECK (state IN ('requested','accepted','refused','expired')),
    refusal         text CHECK (refusal IN ('unknownJob','noLog','busy','tooLarge')),
    sent_at         timestamptz,
    answered_at     timestamptz,
    PRIMARY KEY (tenant_id, job_id, generation),
    FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    CHECK ((state = 'refused') = (refusal IS NOT NULL))
);

CREATE INDEX comm_log_requests_open ON comm_log_requests (tenant_id, agent_id)
    WHERE state = 'requested';
