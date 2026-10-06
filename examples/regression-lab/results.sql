-- Every run: where it has got to, its result, and what the Postgres under test
-- used while it ran. The query is the lab.results view, which setup.sql
-- creates and documents.
--
--   psql -f results.sql
\set ON_ERROR_STOP on

SELECT * FROM lab.results;
