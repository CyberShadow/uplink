-- Whether a Matrix room is a direct chat (the account's m.direct, as the
-- SDK has it), so that queries can name direct chats (SPEC §8.7, §9.5).

ALTER TABLE matrix_room ADD COLUMN direct INTEGER NOT NULL DEFAULT 0;

-- Rooms synced before have it unset: syncing afresh records every room
-- again (events already stored are not stored twice).
UPDATE account SET sync_state = NULL WHERE kind = 'matrix';
