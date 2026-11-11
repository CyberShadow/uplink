-- A room's topic and how many members have joined it, which its header
-- shows (SPEC §9.5). Rooms get them when they next sync.

ALTER TABLE matrix_room ADD COLUMN topic TEXT;
ALTER TABLE matrix_room ADD COLUMN joined_members INTEGER NOT NULL DEFAULT 0;
