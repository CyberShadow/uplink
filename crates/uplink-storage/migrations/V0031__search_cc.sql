-- `cc:` and `to:` search Cc too (SPEC §8.7): the full-text index gets it,
-- from the messages stored before.
CREATE VIRTUAL TABLE message_fts_new USING fts5(
  subject, from_addr, from_name, to_addrs, cc_addrs, body_text,
  tokenize = 'unicode61 remove_diacritics 2',
  prefix = '1 2 3'
);
INSERT INTO message_fts_new (rowid, subject, from_addr, from_name, to_addrs, cc_addrs, body_text)
  SELECT f.rowid, f.subject, f.from_addr, f.from_name, f.to_addrs,
         (SELECT coalesce(group_concat(value, ' '), '') FROM json_each(m.cc_addrs)),
         f.body_text
  FROM message_fts f JOIN message m ON m.id = f.rowid;
DROP TABLE message_fts;
ALTER TABLE message_fts_new RENAME TO message_fts;
