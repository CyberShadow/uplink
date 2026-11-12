-- What came before places were seen (V0042) counts as seen, as it did
-- when nothing had a dot: Home, which holds all its buckets have, was
-- seen with what was stored then. Else the first dots are of the whole
-- mailbox, which their query walks.
INSERT INTO seen (place, mail, room_event, todo, event)
SELECT 'home',
       (SELECT coalesce(max(id), 0) FROM mail_entry),
       (SELECT coalesce(max(id), 0) FROM matrix_event),
       (SELECT coalesce(max(id), 0) FROM todo),
       (SELECT coalesce(max(id), 0) FROM calendar_event)
WHERE NOT EXISTS (SELECT 1 FROM seen WHERE place = 'home');
