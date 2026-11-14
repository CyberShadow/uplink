-- A draft says whom it is from (SPEC §7.2, §8.5): an identity's address
-- as typed, or nothing for the account's own address, which drafts so
-- far were all sent from. Drafts as JSON, in the outbox and in log
-- entries, take it too.
ALTER TABLE draft ADD COLUMN from_addr TEXT NOT NULL DEFAULT '';

UPDATE mail_send SET draft = json_set(draft, '$.content.from', '');
UPDATE tx_log SET op_json = json_set(op_json, '$.content.from', '')
  WHERE op_type = 'draft.put';
UPDATE tx_log SET inverse_json = json_set(inverse_json, '$.content.from', '')
  WHERE inverse_type = 'draft.put';
