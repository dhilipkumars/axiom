SELECT count(*) AS tables,
       count(*) FILTER (WHERE foreign_table_name LIKE 'core\_%') AS core,
       count(*) FILTER (WHERE foreign_table_name = 'core_secrets') AS secrets
  FROM information_schema.foreign_tables
 WHERE foreign_table_schema = 'k8s';
