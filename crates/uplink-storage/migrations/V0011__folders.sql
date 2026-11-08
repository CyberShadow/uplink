-- Folders (SPEC §8.3): labels of which an entry is in exactly one, as an
-- M365 message is in one mail folder. Gmail has none.
ALTER TABLE label ADD COLUMN folder INTEGER NOT NULL DEFAULT 0 CHECK (folder IN (0, 1));
