SELECT "table", "from", "to", on_update, on_delete
FROM pragma_foreign_key_list(?1)
ORDER BY id, seq;
