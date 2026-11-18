-- A bridged Matrix room's network (m.bridge's protocol): its name and
-- icon, which the room's avatar shows as a badge (SPEC §9.5). Both or
-- neither.

ALTER TABLE matrix_room ADD COLUMN bridge_name TEXT;
ALTER TABLE matrix_room ADD COLUMN bridge_icon TEXT
  CHECK ((bridge_name IS NULL) = (bridge_icon IS NULL));

-- Rooms synced before have none: syncing afresh records every room
-- again (events already stored are not stored twice).
UPDATE account SET sync_state = NULL WHERE kind = 'matrix';
