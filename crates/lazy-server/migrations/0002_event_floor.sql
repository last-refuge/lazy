-- Event-log retention.
--
-- `event_floor` is the highest position that has been pruned: events 1..=event_floor no longer
-- exist, and positions above it stay gapless up to `event_head`. A client cursor `after` can be
-- replayed only if `after >= event_floor`; older cursors must start again from a bootstrap.
--
-- The floor only ever rises. A prune deletes the events and raises the floor in one transaction
-- (while holding the workspace row lock), so any single statement sees both or neither.
ALTER TABLE workspaces
    ADD COLUMN event_floor bigint NOT NULL DEFAULT 0,
    ADD CONSTRAINT workspaces_event_floor_range CHECK (event_floor >= 0 AND event_floor <= event_head);
