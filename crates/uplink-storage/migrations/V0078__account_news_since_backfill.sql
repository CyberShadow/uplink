-- Accounts first synced after V0053 by a build that did not yet set
-- account.news_since got none, and since V0060 storing an event that
-- notifies fails for them (`?17 AND news` is NULL), which stops their
-- sync. As V0053: of an account already synced, its first log entry's
-- time.
UPDATE account SET news_since = coalesce(
  (SELECT min(ts) FROM tx_log WHERE tx_log.account_id = account.id),
  CAST(unixepoch('subsec') * 1000 AS INTEGER))
WHERE news_since IS NULL;
