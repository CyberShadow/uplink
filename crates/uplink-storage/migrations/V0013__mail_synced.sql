-- Whether a mail account's mailbox is mirrored: its first listing is
-- done. Until then, `sync_state` may hold how far the listing has got
-- (Gmail's, which resumes after an interruption).
ALTER TABLE account ADD COLUMN synced INTEGER NOT NULL DEFAULT 0;
UPDATE account SET synced = sync_state IS NOT NULL;
