-- Search and lists stay quick on large mailboxes (SPEC §4.5, §8.7).

-- Prefix indexes: every search word is a prefix, and a short one matches
-- thousands of terms, which without them are merged at query time.
CREATE VIRTUAL TABLE message_fts_new USING fts5(
  subject, from_addr, from_name, to_addrs, body_text,
  tokenize = 'unicode61 remove_diacritics 2',
  prefix = '1 2 3'
);
INSERT INTO message_fts_new (rowid, subject, from_addr, from_name, to_addrs, body_text)
  SELECT rowid, subject, from_addr, from_name, to_addrs, body_text FROM message_fts;
DROP TABLE message_fts;
ALTER TABLE message_fts_new RENAME TO message_fts;

-- Search results that match much of the mail are found by walking all
-- entries newest first.
CREATE INDEX mail_entry_by_received ON mail_entry(received_at DESC, id DESC);
