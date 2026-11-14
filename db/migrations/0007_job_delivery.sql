-- Job delivery (design 5.3, 14.1, ADR-476): what the server's delivery queue records about a job's
-- command while the job is still `received` (5.6). `state` is unchanged by delivery.
--
--   delivery_attempts  how many times the stored instruction has been put on a connection
--   delivery_sent_at   when the latest of them was marked; null when none is outstanding. A mark
--                      older than the lease, or released by the instance that made it, lets the
--                      job be taken again
--   delivered_at       when the agent acknowledged a delivery; the job is then never sent again
ALTER TABLE jobs
    ADD COLUMN delivery_attempts integer NOT NULL DEFAULT 0,
    ADD COLUMN delivery_sent_at  timestamptz,
    ADD COLUMN delivered_at      timestamptz;
