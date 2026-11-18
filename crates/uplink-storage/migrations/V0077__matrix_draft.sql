-- What is being written to a Matrix room (SPEC §7.6): the text of its
-- box, or of one of its threads' (`thread`, its root; NULL for the
-- room's), as typed, edited by splices each based on the
-- revision before it, as mail drafts are (§7.2); what it replies to; and,
-- of the room's box, the files to send with it. Local to uplink, and
-- changed only through ops. A box never written to has no row: it is
-- empty, at revision 0. A row stays once its box is emptied, so that its
-- revision only rises.
CREATE TABLE matrix_draft (
  id        INTEGER PRIMARY KEY AUTOINCREMENT,
  room      INTEGER NOT NULL REFERENCES matrix_room(id),
  thread    INTEGER REFERENCES matrix_event(id),
  revision  INTEGER NOT NULL,          -- edits so far
  text      TEXT NOT NULL,
  reply_to  INTEGER REFERENCES matrix_event(id)
);
CREATE UNIQUE INDEX matrix_draft_box ON matrix_draft(room, coalesce(thread, 0));

-- Ids are never reused, since log entries refer to them.
CREATE TABLE matrix_draft_attachment (
  id     INTEGER PRIMARY KEY AUTOINCREMENT,
  draft  INTEGER NOT NULL REFERENCES matrix_draft(id),
  blob   TEXT NOT NULL,              -- lowercase hex SHA-256
  name   TEXT NOT NULL,              -- the file name it is sent with
  mime   TEXT NOT NULL,
  size   INTEGER NOT NULL            -- bytes
);
CREATE INDEX matrix_draft_attachment_by_draft ON matrix_draft_attachment(draft, id);
