//! Short names for imported tables, created on request.
//!
//! `IMPORT FOREIGN SCHEMA` names every table for its API group -- `core_pods`,
//! `apps_deployments` -- because a name derived from anything else changes when
//! the cluster does (#80). A short name is a view over one of those tables,
//! created only when someone calls for it, and never repointed afterwards:
//!
//! ```sql
//! SELECT * FROM axiom_create_short_names('k8s');        -- pods, deployments, ...
//! SELECT axiom_create_short_name('k8s', 'gw', 'gateway_networking_k8s_io_gateways');
//! ```
//!
//! The rules, each of which exists to keep a short name meaning one thing:
//!
//! - **The core group wins the bare plural.** `pods` means `core_pods` even
//!   beside `metrics_k8s_io_pods`, so running this again next month gives the
//!   same answer.
//! - **Anything else ambiguous is reported, not guessed.** Two CRDs that are
//!   both `gateways`, or core `pods` from two servers sharing a schema through
//!   `prefix`, get no short name until one is chosen explicitly.
//! - **An existing short name is never repointed**, and an object this did not
//!   create is never touched. Views are marked with a comment when created;
//!   anything unmarked belongs to someone else.
//! - **`security_invoker`**, so a query through the view is checked against the
//!   caller's privileges rather than those of whoever created it.
//!
//! Both functions run with the caller's privileges and create ordinary views
//! that are not members of the extension: they need `CREATE` on the schema,
//! and `DROP EXTENSION` leaves them alone. A view does not survive the
//! `DROP ... CASCADE` that refreshing its table requires, so the answer after
//! a re-import is to call this again.
//!
//! `PL/pgSQL` rather than Rust: this is catalog lookups and DDL, which SQL says
//! more plainly, and it keeps the functions out of the backend's Rust paths.

use pgrx::prelude::*;

extension_sql!(
    r"
CREATE FUNCTION axiom_create_short_name(schema_name text, short_name text, target text)
RETURNS text
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $fn$
DECLARE
    marker constant text := 'axiom short name for ';
    target_oid oid;
    existing oid;
    existing_comment text;
BEGIN
    SELECT c.oid INTO target_oid
      FROM pg_class c
      JOIN pg_namespace n ON n.oid = c.relnamespace
      JOIN pg_foreign_table ft ON ft.ftrelid = c.oid
      JOIN pg_foreign_server s ON s.oid = ft.ftserver
      JOIN pg_foreign_data_wrapper w ON w.oid = s.srvfdw
     WHERE n.nspname = schema_name AND c.relname = target AND w.fdwname = 'axiom_fdw';
    IF target_oid IS NULL THEN
        RAISE EXCEPTION 'axiom: %.% is not an axiom foreign table',
            quote_ident(schema_name), quote_ident(target)
            USING ERRCODE = 'undefined_table';
    END IF;

    SELECT c.oid, obj_description(c.oid, 'pg_class') INTO existing, existing_comment
      FROM pg_class c
      JOIN pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = schema_name AND c.relname = short_name;
    IF existing IS NOT NULL THEN
        IF existing_comment = marker || target THEN
            RETURN 'exists';
        ELSIF starts_with(coalesce(existing_comment, ''), marker) THEN
            RETURN 'skipped: already the short name for '
                || substr(existing_comment, length(marker) + 1);
        ELSE
            RETURN 'skipped: exists and was not created by axiom';
        END IF;
    END IF;

    EXECUTE format(
        'CREATE VIEW %I.%I WITH (security_invoker = true) AS SELECT * FROM %I.%I',
        schema_name, short_name, schema_name, target);
    EXECUTE format('COMMENT ON VIEW %I.%I IS %L', schema_name, short_name, marker || target);
    RETURN 'created';
END
$fn$;

COMMENT ON FUNCTION axiom_create_short_name(text, text, text) IS
    'Creates a view named short_name over the axiom foreign table target. Never replaces an existing object.';

CREATE FUNCTION axiom_create_short_names(schema_name text, server_name text DEFAULT NULL)
RETURNS TABLE (short_name text, target text, status text)
LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $fn$
DECLARE
    r record;
BEGIN
    FOR r IN
        WITH candidate AS (
            SELECT c.relname::text AS tbl,
                   (SELECT substr(o, length('resource=') + 1)
                      FROM unnest(ft.ftoptions) o WHERE starts_with(o, 'resource=')) AS resource,
                   coalesce((SELECT substr(o, length('group=') + 1)
                               FROM unnest(ft.ftoptions) o WHERE starts_with(o, 'group=')), '') AS grp
              FROM pg_foreign_table ft
              JOIN pg_class c ON c.oid = ft.ftrelid
              JOIN pg_namespace n ON n.oid = c.relnamespace
              JOIN pg_foreign_server s ON s.oid = ft.ftserver
              JOIN pg_foreign_data_wrapper w ON w.oid = s.srvfdw
             WHERE n.nspname = schema_name
               AND w.fdwname = 'axiom_fdw'
               AND (server_name IS NULL OR s.srvname = server_name)
        )
        SELECT resource,
               count(*) FILTER (WHERE grp = '') AS core_count,
               count(*) AS total,
               min(tbl) FILTER (WHERE grp = '') AS core_table,
               array_agg(tbl ORDER BY tbl) AS tables
          FROM candidate
         WHERE resource IS NOT NULL
         GROUP BY resource
         ORDER BY resource
    LOOP
        short_name := r.resource;
        IF r.core_count = 1 THEN
            target := r.core_table;
        ELSIF r.core_count = 0 AND r.total = 1 THEN
            target := r.tables[1];
        ELSE
            target := NULL;
            status := 'skipped: ambiguous between ' || array_to_string(r.tables, ', ')
                || '; choose one with axiom_create_short_name';
            RETURN NEXT;
            CONTINUE;
        END IF;
        status := @extschema@.axiom_create_short_name(schema_name, short_name, target);
        RETURN NEXT;
    END LOOP;
END
$fn$;

COMMENT ON FUNCTION axiom_create_short_names(text, text) IS
    'Creates a view named for each resource plural over the matching axiom foreign table in schema_name. The core group wins a shared plural; any other ambiguity is reported and skipped.';
",
    name = "axiom_short_names",
    requires = ["axiom_fdw_wrapper"],
);
