-- The events not decrypted, few of the many, found as a room key comes
-- (each key of a sync's) without a scan of every event.
CREATE INDEX matrix_event_undecryptable ON matrix_event(room) WHERE encryption = 'undecryptable';
