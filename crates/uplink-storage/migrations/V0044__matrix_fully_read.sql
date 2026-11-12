-- A room's fully-read marker (SPEC §12.15): where catching up has got
-- to, as Matrix's `m.fully_read`; desired state, as the read marker is,
-- and the latest pushed or observed. Until now catching up was reading.
ALTER TABLE matrix_room ADD COLUMN fully_read INTEGER REFERENCES matrix_event(id);
ALTER TABLE matrix_room ADD COLUMN fully_read_pushed INTEGER REFERENCES matrix_event(id);
UPDATE matrix_room SET fully_read = read_up_to;
