-- How long before an event the user is reminded of it (SPEC §10.2), in
-- minutes; NULL for no reminder. As Graph has it (`isReminderOn`,
-- `reminderMinutesBeforeStart`), from the JSON kept.
ALTER TABLE calendar_event ADD COLUMN reminder INTEGER;
UPDATE calendar_event SET
  reminder = CASE WHEN json_extract(raw, '$.isReminderOn')
                  THEN json_extract(raw, '$.reminderMinutesBeforeStart') END;
