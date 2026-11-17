-- The product-wide default confirmation level of each job kind (design 6.3, 5.1, ADR-488).
-- `POST /api/v1/jobs` reads the level of the job it creates from this table, in the creating
-- transaction, and a kind without a row fails the creation. The table has no tenant column: the
-- defaults belong to the product, and a tenant's or an agent's policy may only tighten them (a
-- later item). `identify` has no row, since no route creates that kind yet.
--
-- The design's "FW write to an unattended device" row (two-person approval and an execution time
-- window) is a condition of the agent, not a job kind, and is not seeded here.
CREATE TABLE confirmation_defaults (
    kind    text PRIMARY KEY CHECK (kind IN
        ('acquire','monitorSession','monitorCapture','runSequence','writeSettings','reprogram','identify')),
    level   text NOT NULL
        CHECK (level IN ('notifyOnly','gracePeriod','onSiteApproval','twoPerson'))
);

INSERT INTO confirmation_defaults (kind, level) VALUES
    ('acquire',        'notifyOnly'),
    ('monitorSession', 'notifyOnly'),
    ('monitorCapture', 'notifyOnly'),
    ('runSequence',    'gracePeriod'),
    ('writeSettings',  'gracePeriod'),
    ('reprogram',      'onSiteApproval');
