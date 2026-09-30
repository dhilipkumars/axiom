# Helpers for the gates that run the programs under examples/ as shipped.
# Sourced after stack.sh, whose `compose` they use.

# psql_file FILE [-v name=value ...]: run one of the example files as shipped.
psql_file() {
  local file="$1"; shift
  compose exec -T "$E2E_SVC_POSTGRES" psql -X -q -v ON_ERROR_STOP=1 -U "$E2E_PG_USER" -d "$E2E_PG_DB" -At \
    -v server=kind "$@" -f - < "$file"
}

# psql_table FILE [-v name=value ...]: the same, printed as psql's aligned
# table, for the output the READMEs quote.
psql_table() {
  local file="$1"; shift
  compose exec -T "$E2E_SVC_POSTGRES" psql -X -q -v ON_ERROR_STOP=1 -U "$E2E_PG_USER" -d "$E2E_PG_DB" \
    -P pager=off -v server=kind "$@" -f - < "$file"
}
