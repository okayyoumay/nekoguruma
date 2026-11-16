-- A default partition for each RANGE partitioned table (design 4.4, 16.3; db/README.md "Partition
-- ever-growing tables"). Without one, the first insert into `audit_log` or `change_feed` fails,
-- since no partition takes its row. A default partition takes every row, whatever its time.
--
-- The granularity of later range partitions (monthly is assumed) is decided separately. Attaching
-- a range partition to a table with a default partition checks the default for rows in that range,
-- so rows already in the default partition for the new range are moved out of it first.
CREATE TABLE change_feed_default PARTITION OF change_feed DEFAULT;
CREATE TABLE audit_log_default PARTITION OF audit_log DEFAULT;
