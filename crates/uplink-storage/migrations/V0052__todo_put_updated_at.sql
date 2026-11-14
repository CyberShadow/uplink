-- A to-do says when it last changed (`Todo.updated_at`), which restoring
-- it keeps: logged restores, of to-dos deleted before, say it was when
-- they were made, the last time known.
UPDATE tx_log SET op_json = json_set(op_json, '$.todo.updated_at',
  json_extract(op_json, '$.todo.created_at'))
WHERE op_type = 'todo.put';
UPDATE tx_log SET inverse_json = json_set(inverse_json, '$.todo.updated_at',
  json_extract(inverse_json, '$.todo.created_at'))
WHERE inverse_type = 'todo.put';
