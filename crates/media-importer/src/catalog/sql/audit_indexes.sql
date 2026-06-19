SELECT name, CAST("unique" AS TEXT), origin
FROM pragma_index_list(?1)
ORDER BY name;
