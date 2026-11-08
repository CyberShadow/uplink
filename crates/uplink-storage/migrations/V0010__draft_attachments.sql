-- Files attached to drafts (SPEC §7.4): uploaded blobs, by content hash
-- (under blobs/ in the data root). Changed only through draft ops; ids are
-- never reused, since log entries refer to them.
CREATE TABLE draft_attachment (
  id     INTEGER PRIMARY KEY AUTOINCREMENT,
  draft  TEXT NOT NULL REFERENCES draft(id) ON DELETE CASCADE,
  blob   TEXT NOT NULL,              -- lowercase hex SHA-256
  name   TEXT NOT NULL,              -- the file name it is sent with
  mime   TEXT NOT NULL,
  size   INTEGER NOT NULL            -- bytes
);
CREATE INDEX draft_attachment_by_draft ON draft_attachment(draft, id);

-- Drafts as JSON, in the outbox and in log entries, now list attachments.
UPDATE mail_outbox SET draft = json_set(draft, '$.attachments', json('[]'));
UPDATE tx_log SET op_json = json_set(op_json, '$.attachments', json('[]'))
  WHERE op_type = 'draft.put';
UPDATE tx_log SET inverse_json = json_set(inverse_json, '$.attachments', json('[]'))
  WHERE inverse_type = 'draft.put';
