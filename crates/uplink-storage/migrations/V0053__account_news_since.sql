-- From when an account's mail is news (SPEC §12.15): what reached the
-- provider before uplink began to sync the account is its history, which
-- a listing of the mailbox stores, however late, not news. Unix ms; of
-- an account already synced, its first log entry's time.
ALTER TABLE account ADD COLUMN news_since INTEGER;
UPDATE account SET news_since = coalesce(
  (SELECT min(ts) FROM tx_log WHERE tx_log.account_id = account.id),
  CAST(unixepoch('subsec') * 1000 AS INTEGER));
