SELECT name, type, "notnull", pk
FROM pragma_table_info(?1)
ORDER BY cid;
