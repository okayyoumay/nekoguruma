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
-- A new request for a job and generation whose row is `refused` or `expired` is made by updating
-- the row in place: the state goes back to `requested`, `refusal`, `sent_at` and `answered_at` are
-- cleared, and `requested_by` and `requested_at` become the new request's. A row in state
-- `requested` or `accepted` is left as it is. The state `accepted` or `refused` is set with
-- `answered_at` and no other state has one (a CHECK); expiry applies to `requested` alone.
--
-- The agent is the job's own: the foreign key over `(tenant_id, job_id, agent_id)` needs the
-- unique constraint on `jobs` that the migration adds (nothing changes a job's agent). The agent
-- and operator keys do not carry the tenant, as `job_objects` has them: `agents` and `operators`
-- have no unique `(tenant_id, id)`, and the job's key already ties the row's tenant to the job's.
-- The foreign keys have no cascade: a job, an agent or an operator that has requests is not
-- deleted under them. The partial index serves the delivery's scan of one agent's open requests.
ALTER TABLE jobs ADD CONSTRAINT jobs_tenant_id_id_agent_id_key UNIQUE (tenant_id, id, agent_id);

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
    FOREIGN KEY (tenant_id, job_id, agent_id) REFERENCES jobs (tenant_id, id, agent_id),
    CHECK ((state = 'refused') = (refusal IS NOT NULL)),
    CHECK ((state IN ('accepted','refused')) = (answered_at IS NOT NULL))
);

CREATE INDEX comm_log_requests_open ON comm_log_requests (tenant_id, agent_id)
    WHERE state = 'requested';
