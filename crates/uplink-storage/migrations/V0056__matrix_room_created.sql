-- When a Matrix room was created (its m.room.create's time, ms), as the
-- SDK's state has it: how far back fetching its history has to go, for
-- its progress (SPEC §9.2).

ALTER TABLE matrix_room ADD COLUMN created INTEGER;

-- Rooms synced before have it unset: syncing afresh records every room
-- again (events already stored are not stored twice).
UPDATE account SET sync_state = NULL WHERE kind = 'matrix';
