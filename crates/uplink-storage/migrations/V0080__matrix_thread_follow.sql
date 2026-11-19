-- Following a Matrix thread (MSC4306 thread subscriptions, SPEC §9.2,
-- §9.5): whether the account is subscribed to the thread of root `root`,
-- reconciled as a field (§6.6.1). Each row holds the thread's
-- `uplink_tx_log::reconcile::Field<bool>` as JSON (`field`), with the
-- columns queries need generated from it. A thread of no row is not
-- followed, and was never asked to be, as known from the start: the
-- homeserver's state of it is read when a message of it comes.
--
-- First V0079, which was the to-do's room event's; a database that
-- applied it so has its table, and its record dropped before migrating
-- (`uplink_storage::renumber_thread_follow`), so it is created only if
-- it is not there.

CREATE TABLE IF NOT EXISTS matrix_thread_follow (
  room       INTEGER NOT NULL REFERENCES matrix_room(id) ON DELETE CASCADE,
  root       TEXT NOT NULL,
  field      TEXT NOT NULL CHECK (json_valid(field)),
  -- Shown, and what its messages notify by.
  desired    INTEGER GENERATED ALWAYS AS (json_extract(field, '$.desired')) VIRTUAL,
  -- The op asking, while pending.
  desired_by INTEGER GENERATED ALWAYS AS (json_extract(field, '$.opinion')) VIRTUAL,
  -- Whether, and why, it waits for the user's decision.
  decision   TEXT GENERATED ALWAYS AS (json_extract(field, '$.decision')) VIRTUAL,
  PRIMARY KEY (room, root)
);
CREATE INDEX IF NOT EXISTS matrix_thread_follow_pending ON matrix_thread_follow(room)
  WHERE desired_by IS NOT NULL;
