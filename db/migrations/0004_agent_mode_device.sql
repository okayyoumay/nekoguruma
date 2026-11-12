-- The agent's second mode is named "device" rather than "machine" (design 6.4, 6.7, ADR-463): it is
-- the mode in which the agent runs for the device and not for one user. The columns machine_hint and
-- os_user_hint keep their names; they are hints about the host, not the mode.
ALTER TABLE agents DROP CONSTRAINT agents_mode_check;
UPDATE agents SET mode = 'device' WHERE mode = 'machine';
ALTER TABLE agents ADD CONSTRAINT agents_mode_check CHECK (mode IN ('user','device'));
