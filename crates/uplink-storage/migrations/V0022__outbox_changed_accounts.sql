-- `outbox_changed` events name the accounts whose outbox changed, no
-- longer the mail outbox items (since V0018, SPEC §5.4): recorded events are
-- replayed, so they are rewritten to the new shape. An observation's is
-- its account; an op's, the accounts of its items still kept (none, of
-- those deleted once synced back: clients list the outbox again on any
-- such event, whatever accounts it names).

UPDATE tx_log
SET events_json = (
  SELECT json_group_array(
    CASE
      WHEN json_extract(e.value, '$.type') = 'outbox_changed'
           AND json_type(e.value, '$.items') IS NOT NULL
      THEN json_object(
        'type', 'outbox_changed',
        'accounts', CASE
          WHEN tx_log.account_id IS NOT NULL THEN json_array(tx_log.account_id)
          ELSE (SELECT json_group_array(DISTINCT s.account_id) FROM mail_send s
                WHERE s.id IN (SELECT i.value FROM json_each(e.value, '$.items') i))
        END)
      ELSE json(e.value)
    END)
  FROM json_each(tx_log.events_json) e
)
WHERE EXISTS (
  SELECT 1 FROM json_each(tx_log.events_json) e
  WHERE json_extract(e.value, '$.type') = 'outbox_changed'
    AND json_type(e.value, '$.items') IS NOT NULL
);
