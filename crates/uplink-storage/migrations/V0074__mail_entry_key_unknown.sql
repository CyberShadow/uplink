-- An entry whose key a push changed without telling the new one (an IMAP
-- MOVE without COPYUID, SPEC §6.6.5): left out of pushes and read-backs,
-- which would go to the key gone, until a sync finds it again.
ALTER TABLE mail_entry ADD COLUMN key_unknown INTEGER NOT NULL DEFAULT 0;
