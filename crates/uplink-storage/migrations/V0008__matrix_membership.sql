-- Joining and leaving rooms (SPEC §9.7): the membership the user asked
-- for, until the account's sync task has it (NULL: none asked for), and
-- why the homeserver last refused it.
ALTER TABLE matrix_room ADD COLUMN desired TEXT CHECK (desired IN ('joined', 'left'));
ALTER TABLE matrix_room ADD COLUMN membership_error TEXT;
