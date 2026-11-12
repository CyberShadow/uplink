-- A Matrix room's address (m.room.canonical_alias), to show and link to
-- it by (SPEC §12.11).

ALTER TABLE matrix_room ADD COLUMN alias TEXT;

-- Rooms synced before have none: syncing afresh records every room
-- again (events already stored are not stored twice).
UPDATE account SET sync_state = NULL WHERE kind = 'matrix';
