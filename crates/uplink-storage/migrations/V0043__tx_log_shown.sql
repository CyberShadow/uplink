-- Ops a view dispatched by being shown, not by the user's hand (SPEC
-- §12.15): marking seen, and read, as things come on screen. `z` passes
-- over them.
ALTER TABLE tx_log ADD COLUMN shown INTEGER NOT NULL DEFAULT 0;
