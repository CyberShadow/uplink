-- Of Matrix events, only news notifies or highlights (SPEC §12.15): what
-- was sent once uplink began to sync the account (account.news_since).
-- The history a first sync or filling a gap stored, which the push rules
-- evaluated as any, was counted among mentions and what notifies.

UPDATE matrix_event SET notify = 0, highlight = 0
WHERE (notify OR highlight)
  AND ts < (SELECT a.news_since FROM matrix_room r JOIN account a ON a.id = r.account_id
            WHERE r.id = matrix_event.room);
