-- A send a client asks for carries the key the client chose (SPEC §9.3):
-- asked again with it, as a request resent on a new connection is, it is
-- answered by the entry that queued it, not queued twice. Those queued
-- otherwise (sent anew by an undo) have none.
ALTER TABLE matrix_send ADD COLUMN key TEXT;
CREATE UNIQUE INDEX matrix_send_key ON matrix_send(key);
