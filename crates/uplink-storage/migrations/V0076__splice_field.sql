-- A splice is of a text (SPEC §7.2), which a draft's edit names beside
-- it: the field of the draft, where the splice named it itself. The
-- log's edits, their inverses and their events say so, as undo and
-- replay read them. (An edit's entry has the one event.)

UPDATE tx_log SET op_json = json_remove(
  json_set(op_json, '$.field', json_extract(op_json, '$.splice.field')),
  '$.splice.field')
WHERE op_type = 'draft.edit';

UPDATE tx_log SET inverse_json = json_remove(
  json_set(inverse_json, '$.field', json_extract(inverse_json, '$.splice.field')),
  '$.splice.field')
WHERE inverse_type = 'draft.edit';

UPDATE tx_log SET events_json = json_remove(
  json_set(events_json, '$[0].field', json_extract(events_json, '$[0].splice.field')),
  '$[0].splice.field')
WHERE op_type = 'draft.edit';
