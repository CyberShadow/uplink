-- A label's kind (SPEC §6.6), where it was whether it is a folder: a tag,
-- under no rule of where an entry is; a mailbox, where an entry is in any
-- number of them (JMAP's); or a folder, where it is alone.
ALTER TABLE label ADD COLUMN kind TEXT NOT NULL DEFAULT 'tag'
  CHECK (kind IN ('tag', 'mailbox', 'folder'));
UPDATE label SET kind = 'folder' WHERE folder;
ALTER TABLE label DROP COLUMN folder;
