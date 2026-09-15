//! Foreign data wrapper glue: the Postgres FDW callbacks for on-demand scans
//! (Phase 1) and row-level writes (Phase 2). This file is deliberately thin.
//! Everything decidable without Postgres (options, qual → filter, JSON → row,
//! write-body construction, error classification) lives in the pure modules
//! and is unit-tested there.
//!
//! Scan lifecycle:
//! 1. `GetForeignRelSize`: validate options, derive the pushed-down filter
//!    from `baserestrictinfo`, set the row estimate.
//! 2. `GetForeignPaths`/`GetForeignPlan`: one plain foreign-scan path; all
//!    quals stay in `plan.qual` so Postgres re-checks them (pushdown narrows
//!    the fetch, it never replaces local evaluation).
//! 3. `BeginForeignScan`: re-derive the filter from `plan.qual` (avoids
//!    serialising private state through `fdw_private`, which differs across
//!    pg14–17 node layouts), build the column map, allocate scan state in the
//!    per-query memory context.
//! 4. `IterateForeignScan`: first call issues one `List` RPC; subsequent
//!    calls pop rows. Row datums live in the per-tuple context.
//!
//! Write lifecycle (writable kinds only, see `Kind::writable`):
//! 1. `AddForeignUpdateTargets`: add the row's `raw` column as a resjunk
//!    target (`axiom_raw`) so UPDATE/DELETE know the object's identity and
//!    the `resourceVersion` it was read at. Tables without `raw` are
//!    read-only for UPDATE/DELETE.
//! 2. `BeginForeignModify`: resolve options, column map, junk attno.
//! 3. `ExecForeignInsert/Update/Delete`: one unary RPC each, straight to the
//!    gateway. Writes are never cache-served (docs/DESIGN.md §5.5). A stale
//!    `resourceVersion` surfaces as SQLSTATE 40001 (`serialization_failure`)
//!    so callers can re-read and retry.
//!
//! Transactions: like `postgres_fdw` against a remote with no 2PC, a write
//! takes effect at the API server when the statement executes; a later
//! ROLLBACK does not undo it (Kubernetes has no transactions, DESIGN.md §2).
//!
//! Errors: every failure is raised as a proper SQL error with an FDW SQLSTATE;
//! nothing here panics on bad input.

use std::collections::VecDeque;
use std::ffi::{c_char, c_int, c_void, CStr};

use pgrx::prelude::*;
use pgrx::{pg_sys, JsonB, PgList, PgMemoryContexts};

use crate::cache::{decide_tier, CacheMode, Tier, CACHE_FULL_REASON};
use crate::client::{self, ClientError, ErrorClass};
use crate::import::{self, ImportColumn, ImportKind, ImportOptions};
use crate::options::{self, Catalog, OptionsError, ServerOptions, TableOptions};
use crate::quals::{Filter, Qual};
use crate::resource::Resource;
use crate::schema::SqlType;
use crate::shmem::{self, ShmemError};
use crate::table::{
    self, Cell, DecodeError, NewCell, NewRow, Row, TableSchema, WriteError, MAX_OBJECT_BYTES,
};

// --- SQL surface --------------------------------------------------------------

/// Returns the FDW callback table. Referenced by `CREATE FOREIGN DATA WRAPPER`.
#[pg_extern]
fn axiom_fdw_handler() -> PgBox<pg_sys::FdwRoutine> {
    // SAFETY: allocating a zeroed node in the current memory context and
    // filling in function pointers is exactly what every FDW handler does.
    unsafe {
        let mut routine = PgBox::<pg_sys::FdwRoutine>::alloc_node(pg_sys::NodeTag::T_FdwRoutine);
        routine.GetForeignRelSize = Some(get_foreign_rel_size);
        routine.GetForeignPaths = Some(get_foreign_paths);
        routine.GetForeignPlan = Some(get_foreign_plan);
        routine.BeginForeignScan = Some(begin_foreign_scan);
        routine.IterateForeignScan = Some(iterate_foreign_scan);
        routine.ReScanForeignScan = Some(rescan_foreign_scan);
        routine.EndForeignScan = Some(end_foreign_scan);
        routine.IsForeignRelUpdatable = Some(is_foreign_rel_updatable);
        routine.AddForeignUpdateTargets = Some(add_foreign_update_targets);
        routine.PlanForeignModify = Some(plan_foreign_modify);
        routine.BeginForeignModify = Some(begin_foreign_modify);
        routine.ExecForeignInsert = Some(exec_foreign_insert);
        routine.ExecForeignUpdate = Some(exec_foreign_update);
        routine.ExecForeignDelete = Some(exec_foreign_delete);
        routine.EndForeignModify = Some(end_foreign_modify);
        routine.ImportForeignSchema = Some(import_foreign_schema);
        routine.into_pg_boxed()
    }
}

/// Validates `OPTIONS (...)` on `CREATE/ALTER FOREIGN DATA WRAPPER | SERVER |
/// FOREIGN TABLE | USER MAPPING`. Raises an FDW SQLSTATE error naming the
/// offending option; accepts silently otherwise.
#[pg_extern]
fn axiom_fdw_validator(options: Vec<Option<String>>, catalog: Option<pg_sys::Oid>) {
    let catalog = match catalog {
        Some(pg_sys::ForeignDataWrapperRelationId) => Catalog::Wrapper,
        Some(pg_sys::ForeignServerRelationId) => Catalog::Server,
        Some(pg_sys::ForeignTableRelationId) => Catalog::Table,
        Some(pg_sys::UserMappingRelationId) => Catalog::UserMapping,
        // Postgres only calls validators for the four catalogs above.
        _ => return,
    };
    let mut pairs = Vec::with_capacity(options.len());
    for opt in options.into_iter().flatten() {
        match options::split_option(&opt) {
            Some(kv) => pairs.push(kv),
            None => raise(
                PgSqlErrorCode::ERRCODE_FDW_INVALID_OPTION_NAME,
                format!("malformed option {opt:?}"),
            ),
        }
    }
    if let Err(e) = options::validate(catalog, &pairs) {
        raise(options_sqlstate(&e), e.to_string());
    }
}

extension_sql!(
    "CREATE FOREIGN DATA WRAPPER axiom_fdw HANDLER axiom_fdw_handler VALIDATOR axiom_fdw_validator;",
    name = "axiom_fdw_wrapper",
    requires = [axiom_fdw_handler, axiom_fdw_validator],
);

// --- error plumbing -------------------------------------------------------------

/// Raises a SQL ERROR with `code`. Never returns.
fn raise(code: PgSqlErrorCode, msg: String) -> ! {
    pgrx::ereport!(PgLogLevel::ERROR, code, msg);
    // ereport at ERROR level unwinds to the pg_guard boundary and never returns.
    unreachable!("ereport(ERROR) returned")
}

fn options_sqlstate(e: &OptionsError) -> PgSqlErrorCode {
    match e {
        OptionsError::Unknown { .. } | OptionsError::Duplicate(_) => {
            PgSqlErrorCode::ERRCODE_FDW_INVALID_OPTION_NAME
        }
        OptionsError::Missing(_) => PgSqlErrorCode::ERRCODE_FDW_OPTION_NAME_NOT_FOUND,
        OptionsError::Endpoint(_)
        | OptionsError::Timeout(_)
        | OptionsError::Resource(_)
        | OptionsError::CacheMode(_)
        | OptionsError::Identity(_)
        | OptionsError::Bool(..)
        | OptionsError::ForcedWritable(_) => PgSqlErrorCode::ERRCODE_FDW_INVALID_ATTRIBUTE_VALUE,
    }
}

fn client_sqlstate(e: &ClientError) -> PgSqlErrorCode {
    match e.class() {
        ErrorClass::Connection => PgSqlErrorCode::ERRCODE_FDW_UNABLE_TO_ESTABLISH_CONNECTION,
        ErrorClass::Permission => PgSqlErrorCode::ERRCODE_INSUFFICIENT_PRIVILEGE,
        // Stale resourceVersion: same class as a serialization failure, and
        // equally retryable after re-reading the row.
        ErrorClass::Conflict => PgSqlErrorCode::ERRCODE_T_R_SERIALIZATION_FAILURE,
        ErrorClass::AlreadyExists => PgSqlErrorCode::ERRCODE_UNIQUE_VIOLATION,
        ErrorClass::NotFound => PgSqlErrorCode::ERRCODE_UNDEFINED_OBJECT,
        ErrorClass::InvalidRequest | ErrorClass::GatewayUnconfigured | ErrorClass::Internal => {
            PgSqlErrorCode::ERRCODE_FDW_ERROR
        }
    }
}

fn write_sqlstate(e: &WriteError) -> PgSqlErrorCode {
    match e {
        WriteError::ReadOnly(_) | WriteError::IdentityChange(_) | WriteError::NotWritable(_) => {
            PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED
        }
        WriteError::MissingName | WriteError::MissingNamespace | WriteError::NullNotAllowed(_) => {
            PgSqlErrorCode::ERRCODE_NOT_NULL_VIOLATION
        }
        WriteError::InvalidName(..)
        | WriteError::NotAnObject(_)
        | WriteError::DataValueNotString(_) => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
        WriteError::BadOldRaw(_) => PgSqlErrorCode::ERRCODE_FDW_ERROR,
    }
}

/// Raises the SQL error for a failed gateway call.
/// Raises for a failed import, naming the setting to change on a timeout.
///
/// A bare `Cancelled: Timeout expired` gives an operator nothing to act on. It
/// does not say which timeout applied, and the obvious guess -- that a scan
/// was slow -- is wrong: an import is a different shape of call, enumerating
/// every kind, fetching an `OpenAPI` document per API group and checking access
/// per kind. On a bare cluster with `--serve '*.*'` that already exceeded a 10
/// second `rpc_timeout_secs`.
///
/// Giving IMPORT its own longer budget was tried here and reverted. The
/// channel cache is keyed on `(target, rpc_timeout)`, so a different timeout
/// builds a second connection to the same gateway, and the whole-cluster
/// import then failed with `Unknown: transport error` before fetching
/// anything. Making the deadline per-call rather than per-channel is the way
/// to do it and is its own change; this message is the half that helps today.
fn raise_import(op: &str, server: &ServerOptions, e: &ClientError) -> ! {
    if e.is_deadline() {
        raise(
            client_sqlstate(e),
            format!(
                "axiom: IMPORT FOREIGN SCHEMA timed out after {}s. Discovery fetches an \
                 OpenAPI document per API group and runs an access check for every kind, \
                 so an import takes far longer than a scan of one table. Raise the \
                 server's rpc_timeout_secs with ALTER SERVER ... OPTIONS (SET \
                 rpc_timeout_secs '...'), or import one API group at a time instead of \
                 the whole cluster: {e}",
                server.rpc_timeout.as_secs(),
            ),
        );
    }
    raise_client(op, server, e);
}

fn raise_client(op: &str, server: &ServerOptions, e: &ClientError) -> ! {
    let msg = match e.class() {
        ErrorClass::Conflict => format!(
            "axiom: {op} rejected: object was modified concurrently (re-read the row and retry): {e}"
        ),
        ErrorClass::Connection => format!("axiom: cannot reach gateway {}: {e}", server.target.endpoint),
        _ => format!("axiom: {op} failed: {e}"),
    };
    raise(client_sqlstate(e), msg);
}

// --- catalog access -------------------------------------------------------------

/// Looks up a foreign server by name and returns its validated options.
///
/// For SQL-callable helpers that take a server name rather than running inside
/// a scan. Errors carry the server name so the message is actionable:
/// "no such server" and "the server exists but its options are wrong" are
/// different problems for whoever is reading them.
pub fn server_options_by_name(name: &str) -> Result<ServerOptions, String> {
    let cname = std::ffi::CString::new(name)
        .map_err(|_| format!("server name {name:?} contains an interior NUL byte"))?;
    // SAFETY: a NUL-terminated name; missing_ok=true returns null rather than
    // raising, so the not-found case is ours to report.
    let server = unsafe { pg_sys::GetForeignServerByName(cname.as_ptr(), true) };
    if server.is_null() {
        return Err(format!("server {name:?} does not exist"));
    }
    // A `#[pg_extern]` function is executable by PUBLIC unless the extension
    // revokes it, so without this check any role could name any server, make
    // this backend dial that server's endpoint, and read back its operational
    // counters. Requiring USAGE is the same bar Postgres puts on every other
    // use of a foreign server.
    //
    // Note what this deliberately excludes. docs/AUTH.md §6.1 has query roles
    // holding table grants and never USAGE ON FOREIGN SERVER, precisely so
    // they cannot rewrite their own user mapping; the same line makes this a
    // DBA-facing function rather than one every querying role can call. That
    // is the right side to err on for a diagnostic that opens a connection.
    //
    // SAFETY: non-null FormData_pg_foreign_server from the catalog lookup.
    let acl = unsafe { foreign_server_usage_aclcheck((*server).serverid) };
    if acl != pg_sys::AclResult::ACLCHECK_OK {
        return Err(format!(
            "permission denied for foreign server {name:?}: USAGE is required"
        ));
    }
    // SAFETY: non-null FormData_pg_foreign_server from the catalog lookup.
    let opts = unsafe { options_from_list((*server).options) };
    ServerOptions::parse(&opts).map_err(|e| format!("server {name:?}: {e}"))
}

/// `USAGE` privilege check on a foreign server, across supported majors.
///
/// Postgres 16 retired the per-catalog `pg_*_aclcheck` family in favour of one
/// `object_aclcheck` taking the catalog's OID. With pg16 as the supported
/// floor there is only one spelling left, so this is no longer version-gated.
///
/// # Safety
/// `srvid` must be a live foreign server OID from a catalog lookup.
unsafe fn foreign_server_usage_aclcheck(srvid: pg_sys::Oid) -> pg_sys::AclResult::Type {
    // SAFETY: caller guarantees a valid server OID; ForeignServerRelationId is
    // the catalog that OID belongs to.
    unsafe {
        pg_sys::object_aclcheck(
            pg_sys::ForeignServerRelationId,
            srvid,
            pg_sys::GetUserId(),
            pg_sys::AclMode::from(pg_sys::ACL_USAGE),
        )
    }
}

/// The `i`th attribute of a tuple descriptor.
///
/// pg18 reorganised `TupleDescData`. Up to pg17 it ends with an inline
/// `attrs` array of `FormData_pg_attribute`. pg18 replaced that with
/// `compact_attrs`, a smaller per-attribute struct used on hot paths, and
/// moved the full `FormData_pg_attribute` array to immediately after it. So
/// the full attribute array starts `natts` compact entries in.
///
/// pgrx has an equivalent internally but does not export it, and its version
/// takes a `PgBox` this code does not hold.
///
/// # Safety
/// `tupdesc` must be a live tuple descriptor and `i` less than its `natts`.
#[cfg(any(feature = "pg16", feature = "pg17"))]
unsafe fn tupdesc_attr(
    tupdesc: pg_sys::TupleDesc,
    i: usize,
) -> *const pg_sys::FormData_pg_attribute {
    // SAFETY: caller guarantees a live descriptor and an in-range index.
    unsafe { (*tupdesc).attrs.as_ptr().add(i) }
}

/// See the pg16/pg17 variant above.
///
/// # Safety
/// `tupdesc` must be a live tuple descriptor and `i` less than its `natts`.
#[cfg(not(any(feature = "pg16", feature = "pg17")))]
unsafe fn tupdesc_attr(
    tupdesc: pg_sys::TupleDesc,
    i: usize,
) -> *const pg_sys::FormData_pg_attribute {
    // SAFETY: caller guarantees a live descriptor and an in-range index. The
    // full attribute array begins after `natts` compact entries.
    unsafe {
        let natts = usize::try_from((*tupdesc).natts).unwrap_or(0);
        (*tupdesc)
            .compact_attrs
            .as_ptr()
            .add(natts)
            .cast::<pg_sys::FormData_pg_attribute>()
            .add(i)
    }
}

/// Reads a `List` of `DefElem` options into `(name, value)` pairs.
unsafe fn options_from_list(list: *mut pg_sys::List) -> Vec<(String, String)> {
    // SAFETY: called by the executor/planner with valid node pointers; see module docs.
    unsafe {
        let mut out = Vec::new();
        if list.is_null() {
            return out;
        }
        for def in PgList::<pg_sys::DefElem>::from_pg(list).iter_ptr() {
            let name = CStr::from_ptr((*def).defname)
                .to_string_lossy()
                .into_owned();
            // defGetString raises a SQL error itself for non-string values.
            let value = CStr::from_ptr(pg_sys::defGetString(def))
                .to_string_lossy()
                .into_owned();
            out.push((name, value));
        }
        out
    }
}

/// Everything a scan needs from the catalogs.
struct ScanConfig {
    server: ServerOptions,
    resource: Resource,
    writable: bool,
    cache_mode: CacheMode,
}

/// Loads and validates server + table options for a foreign table.
unsafe fn scan_config(foreigntableid: pg_sys::Oid) -> ScanConfig {
    // SAFETY: called by the executor/planner with valid node pointers; see module docs.
    unsafe {
        let table = pg_sys::GetForeignTable(foreigntableid);
        let server = pg_sys::GetForeignServer((*table).serverid);
        let table = match TableOptions::parse(&options_from_list((*table).options)) {
            Ok(t) => t,
            Err(e) => raise(options_sqlstate(&e), format!("foreign table: {e}")),
        };
        let server = match ServerOptions::parse(&options_from_list((*server).options)) {
            Ok(s) => s,
            Err(e) => raise(options_sqlstate(&e), format!("foreign server: {e}")),
        };
        ScanConfig {
            server,
            resource: table.resource,
            writable: table.writable,
            cache_mode: table.cache_mode,
        }
    }
}

// --- qual extraction -------------------------------------------------------------
//
// Postgres node structs are always palloc'd, hence MAXALIGN'd; casting a
// `*mut Node` to the concrete node type after checking `type_` is the C idiom
// (`castNode`) and cannot misalign.
#[allow(
    clippy::cast_ptr_alignment,
    reason = "castNode idiom on MAXALIGN'd palloc'd nodes"
)]
mod nodecast {
    use pgrx::pg_sys;
    pub(super) unsafe fn relabel(n: *mut pg_sys::Node) -> *mut pg_sys::RelabelType {
        n.cast()
    }
    pub(super) unsafe fn opexpr(n: *mut pg_sys::Node) -> *mut pg_sys::OpExpr {
        n.cast()
    }
    pub(super) unsafe fn var(n: *mut pg_sys::Node) -> *mut pg_sys::Var {
        n.cast()
    }
    pub(super) unsafe fn konst(n: *mut pg_sys::Node) -> *mut pg_sys::Const {
        n.cast()
    }
    pub(super) unsafe fn restrictinfo(n: *mut pg_sys::Node) -> *mut pg_sys::RestrictInfo {
        n.cast()
    }
}

/// Follows `RelabelType` wrappers down to the underlying node.
unsafe fn strip_relabel(mut node: *mut pg_sys::Node) -> *mut pg_sys::Node {
    // SAFETY: called by the executor/planner with valid node pointers; see module docs.
    unsafe {
        while !node.is_null() && (*node).type_ == pg_sys::NodeTag::T_RelabelType {
            node = (*nodecast::relabel(node)).arg.cast();
        }
        node
    }
}

/// If `node` is `Var = Const(text)` (either order) on relation `varno`,
/// returns the column name and literal. Anything else yields `None` and is
/// left for Postgres to evaluate.
unsafe fn text_equality(node: *mut pg_sys::Node, varno: i64, rel_oid: pg_sys::Oid) -> Option<Qual> {
    // SAFETY: called by the executor/planner with valid node pointers; see module docs.
    unsafe {
        let node = strip_relabel(node);
        if node.is_null() || (*node).type_ != pg_sys::NodeTag::T_OpExpr {
            return None;
        }
        let op = nodecast::opexpr(node);
        if (*op).opno != pg_sys::Oid::from(pg_sys::TextEqualOperator) {
            return None;
        }
        let args = PgList::<pg_sys::Node>::from_pg((*op).args);
        if args.len() != 2 {
            return None;
        }
        let (a, b) = (
            strip_relabel(args.get_ptr(0)?),
            strip_relabel(args.get_ptr(1)?),
        );
        let (var, konst) = match ((*a).type_, (*b).type_) {
            (pg_sys::NodeTag::T_Var, pg_sys::NodeTag::T_Const) => {
                (nodecast::var(a), nodecast::konst(b))
            }
            (pg_sys::NodeTag::T_Const, pg_sys::NodeTag::T_Var) => {
                (nodecast::var(b), nodecast::konst(a))
            }
            _ => return None,
        };
        if i64::from((*var).varno) != varno || (*var).varlevelsup != 0 || (*var).varattno <= 0 {
            return None;
        }
        if (*konst).consttype != pg_sys::TEXTOID || (*konst).constisnull {
            return None;
        }
        let value = String::from_datum((*konst).constvalue, false)?;
        let attname = pg_sys::get_attname(rel_oid, (*var).varattno, false);
        if attname.is_null() {
            return None;
        }
        let column = CStr::from_ptr(attname).to_string_lossy().into_owned();
        pg_sys::pfree(attname.cast::<c_void>());
        Some(Qual { column, value })
    }
}

/// Extracts pushable quals from a list of `RestrictInfo` (planning) or bare
/// `Expr` (execution) nodes.
unsafe fn extract_quals(list: *mut pg_sys::List, varno: i64, rel_oid: pg_sys::Oid) -> Vec<Qual> {
    // SAFETY: called by the executor/planner with valid node pointers; see module docs.
    unsafe {
        let mut out = Vec::new();
        if list.is_null() {
            return out;
        }
        for node in PgList::<pg_sys::Node>::from_pg(list).iter_ptr() {
            let expr = if (*node).type_ == pg_sys::NodeTag::T_RestrictInfo {
                (*nodecast::restrictinfo(node))
                    .clause
                    .cast::<pg_sys::Node>()
            } else {
                node
            };
            if let Some(q) = text_equality(expr, varno, rel_oid) {
                out.push(q);
            }
        }
        out
    }
}

// --- planner callbacks -------------------------------------------------------------

#[pg_guard]
unsafe extern "C-unwind" fn get_foreign_rel_size(
    _root: *mut pg_sys::PlannerInfo,
    baserel: *mut pg_sys::RelOptInfo,
    foreigntableid: pg_sys::Oid,
) {
    // SAFETY: called by the executor/planner with valid node pointers; see module docs.
    unsafe {
        // Fail at plan time, not mid-scan, if the DDL is unusable.
        let _cfg = scan_config(foreigntableid);
        let quals = extract_quals(
            (*baserel).baserestrictinfo,
            i64::from((*baserel).relid),
            foreigntableid,
        );
        let filter = Filter::from_quals(&quals);
        (*baserel).rows = filter.estimated_rows();
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn get_foreign_paths(
    root: *mut pg_sys::PlannerInfo,
    baserel: *mut pg_sys::RelOptInfo,
    _foreigntableid: pg_sys::Oid,
) {
    // SAFETY: called by the executor/planner with valid node pointers; see module docs.
    unsafe {
        let rows = (*baserel).rows;
        // One RPC round-trip dominates; per-row cost is JSON decoding.
        let startup_cost = 100.0;
        let total_cost = startup_cost + rows * pg_sys::cpu_tuple_cost * 10.0;
        // create_foreignscan_path has grown twice: pg17 added fdw_restrictinfo,
        // and pg18 added disabled_nodes after rows. Three arities, so three
        // arms. The last is `not(pg16 or pg17)` rather than `pg18` so a later
        // major takes it by default and fails to compile if the shape changes
        // again, which is more useful than silently selecting an old arm.
        #[cfg(feature = "pg16")]
        let path = pg_sys::create_foreignscan_path(
            root,
            baserel,
            std::ptr::null_mut(),
            rows,
            startup_cost,
            total_cost,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        #[cfg(feature = "pg17")]
        let path = pg_sys::create_foreignscan_path(
            root,
            baserel,
            std::ptr::null_mut(),
            rows,
            startup_cost,
            total_cost,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        // disabled_nodes is 0. pg18 lets the planner count how many nodes in a
        // path were produced by a disabled node type (one of the enable_*
        // settings turned off), and prefers the path with fewer of them before
        // it compares costs. Nothing disables a foreign scan, so this path
        // contributes none.
        #[cfg(not(any(feature = "pg16", feature = "pg17")))]
        let path = pg_sys::create_foreignscan_path(
            root,
            baserel,
            std::ptr::null_mut(),
            rows,
            0,
            startup_cost,
            total_cost,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        pg_sys::add_path(baserel, path.cast::<pg_sys::Path>());
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn get_foreign_plan(
    _root: *mut pg_sys::PlannerInfo,
    baserel: *mut pg_sys::RelOptInfo,
    _foreigntableid: pg_sys::Oid,
    _best_path: *mut pg_sys::ForeignPath,
    tlist: *mut pg_sys::List,
    scan_clauses: *mut pg_sys::List,
    outer_plan: *mut pg_sys::Plan,
) -> *mut pg_sys::ForeignScan {
    // SAFETY: called by the executor/planner with valid node pointers; see module docs.
    unsafe {
        // Keep every clause as a local qual: pushdown only narrows the fetch.
        let quals = pg_sys::extract_actual_clauses(scan_clauses, false);
        pg_sys::make_foreignscan(
            tlist,
            quals,
            (*baserel).relid,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            outer_plan,
        )
    }
}

// --- executor callbacks -------------------------------------------------------------

/// Per-scan state, allocated in the per-query memory context.
struct ScanState {
    config: ScanConfig,
    filter: Filter,
    /// The table's resolved columns, in attribute order.
    schema: TableSchema,
    /// `None` until the first `Iterate` fetches; then the remaining rows of
    /// the current page.
    rows: Option<VecDeque<Row>>,
    /// Where the next page starts. `None` before the first fetch; `Some("")`
    /// once the gateway has said there are no more pages.
    ///
    /// Distinguishing "not started" from "finished" matters: both have no rows
    /// left, and confusing them either re-lists a finished scan forever or
    /// returns nothing for a fresh one.
    next_page: Option<String>,
    /// True once the whole collection has been read. Only meaningful for an
    /// on-demand scan; a cache-served scan returns everything at once.
    exhausted: bool,
}

/// Resolves the relation's declared columns against its kind, validating types.
///
/// Unlike Phases 1-3 there is no list of permitted names to check against: a
/// kind's fields are not known without discovery, and a scan deliberately never
/// discovers. A name matching no promoted column becomes a top-level lookup
/// that reads NULL if the object has no such field, which is what lets a
/// hand-written table target a CRD. Types are still strict, so a column
/// declared with the wrong type fails at scan rather than at cast time.
unsafe fn resolve_schema(
    tupdesc: pg_sys::TupleDesc,
    resource: &Resource,
    writable: bool,
) -> TableSchema {
    // SAFETY: tupdesc is a live TupleDesc supplied by the executor.
    unsafe {
        let natts = usize::try_from((*tupdesc).natts).unwrap_or(0);
        let mut names: Vec<Option<String>> = Vec::with_capacity(natts);
        for i in 0..natts {
            let att = tupdesc_attr(tupdesc, i);
            if (*att).attisdropped {
                names.push(None);
                continue;
            }
            names.push(Some(
                CStr::from_ptr((*att).attname.data.as_ptr().cast::<c_char>())
                    .to_string_lossy()
                    .into_owned(),
            ));
        }
        let borrowed: Vec<Option<&str>> = names.iter().map(|n| n.as_deref()).collect();
        let schema = TableSchema::resolve(*resource, writable, &borrowed);

        for (i, col) in schema.columns.iter().enumerate() {
            let Some(col) = col else { continue };
            let att = tupdesc_attr(tupdesc, i);
            let (want_oid, want_name) = match col.sql_type {
                SqlType::Text => (pg_sys::TEXTOID, "text"),
                SqlType::Jsonb => (pg_sys::JSONBOID, "jsonb"),
            };
            if (*att).atttypid != want_oid {
                raise(
                    PgSqlErrorCode::ERRCODE_FDW_INVALID_DATA_TYPE,
                    format!(
                        "column {:?} must be of type {want_name} for {}",
                        col.name, schema.resource
                    ),
                );
            }
        }
        schema
    }
}

/// Decodes gateway JSON into rows, raising on malformed objects.
fn decode_rows(schema: &TableSchema, objects: &[Vec<u8>]) -> VecDeque<Row> {
    let mut rows = VecDeque::with_capacity(objects.len());
    for json in objects {
        match schema.decode(json, MAX_OBJECT_BYTES) {
            Ok(r) => rows.push_back(r),
            Err(
                e @ (DecodeError::TooLarge { .. } | DecodeError::Json(_) | DecodeError::Shape(_)),
            ) => {
                raise(
                    PgSqlErrorCode::ERRCODE_FDW_INVALID_DATA_TYPE,
                    format!("axiom: bad object from gateway: {e}"),
                );
            }
        }
    }
    rows
}

/// Stores `row` into `slot` as a virtual tuple, allocating datums in `memcx`.
unsafe fn store_row(
    slot: *mut pg_sys::TupleTableSlot,
    schema: &TableSchema,
    row: &Row,
    memcx: pg_sys::MemoryContext,
) {
    // SAFETY: slot is the executor's slot for this relation; the schema was
    // resolved from that relation's tupdesc, so cells align with attributes.
    unsafe {
        if let Some(clear) = (*(*slot).tts_ops).clear {
            clear(slot);
        }
        let natts = schema.columns.len();
        let values = std::slice::from_raw_parts_mut((*slot).tts_values, natts);
        let isnull = std::slice::from_raw_parts_mut((*slot).tts_isnull, natts);
        PgMemoryContexts::For(memcx).switch_to(|_| {
            for i in 0..natts {
                let datum = match row.cells.get(i).and_then(Option::as_ref) {
                    Some(Cell::Text(s)) => s.clone().into_datum(),
                    Some(Cell::Json(v)) => JsonB(v.clone()).into_datum(),
                    None => None,
                };
                if let Some(d) = datum {
                    values[i] = d;
                    isnull[i] = false;
                } else {
                    values[i] = pg_sys::Datum::from(0);
                    isnull[i] = true;
                }
            }
        });
        pg_sys::ExecStoreVirtualTuple(slot);
    }
}

/// Reads attribute `attno` (1-based) of `slot` as a datum, materialising the
/// slot if needed. `None` for SQL NULL.
unsafe fn slot_datum(slot: *mut pg_sys::TupleTableSlot, attno: i16) -> Option<pg_sys::Datum> {
    // SAFETY: executor-provided slot; attno validated by the caller against its tupdesc.
    unsafe {
        let n = usize::try_from(attno).ok().filter(|n| *n > 0)?;
        if usize::try_from((*slot).tts_nvalid).unwrap_or(0) < n {
            pg_sys::slot_getsomeattrs_int(slot, i32::from(attno));
        }
        if *(*slot).tts_isnull.add(n - 1) {
            None
        } else {
            Some(*(*slot).tts_values.add(n - 1))
        }
    }
}

/// Reads the new tuple's columns into a [`NewRow`] aligned with the table's
/// resolved columns.
unsafe fn new_row_from_slot(slot: *mut pg_sys::TupleTableSlot, schema: &TableSchema) -> NewRow {
    let mut row = NewRow::undeclared(schema);
    for (i, col) in schema.columns.iter().enumerate() {
        let Some(col) = col else { continue };
        let attno = i16::try_from(i + 1).unwrap_or(0);
        // SAFETY: attno is within the slot's tupdesc by construction of the schema.
        let Some(datum) = (unsafe { slot_datum(slot, attno) }) else {
            row.cells[i] = NewCell::Null;
            continue;
        };
        let value = match col.sql_type {
            // SAFETY: column types were checked against the tupdesc in resolve_schema.
            SqlType::Text => unsafe { String::from_datum(datum, false) }.map(Cell::Text),
            SqlType::Jsonb => unsafe { JsonB::from_datum(datum, false) }.map(|j| Cell::Json(j.0)),
        };
        row.cells[i] = value.map_or(NewCell::Null, NewCell::Value);
    }
    row
}

#[pg_guard]
unsafe extern "C-unwind" fn begin_foreign_scan(node: *mut pg_sys::ForeignScanState, eflags: c_int) {
    // SAFETY: called by the executor/planner with valid node pointers; see module docs.
    unsafe {
        if eflags & pg_sys::EXEC_FLAG_EXPLAIN_ONLY.cast_signed() != 0 {
            return;
        }
        let rel = (*node).ss.ss_currentRelation;
        let rel_oid = (*rel).rd_id;
        let plan = (*node).ss.ps.plan.cast::<pg_sys::ForeignScan>();
        let quals = extract_quals(
            (*plan).scan.plan.qual,
            i64::from((*plan).scan.scanrelid),
            rel_oid,
        );
        let config = scan_config(rel_oid);
        let schema = resolve_schema((*rel).rd_att, &config.resource, config.writable);
        let state = ScanState {
            config,
            filter: Filter::from_quals(&quals),
            schema,
            rows: None,
            next_page: None,
            exhausted: false,
        };
        // Dropped when the executor's per-query context is reset, error or not.
        (*node).fdw_state = PgMemoryContexts::CurrentMemoryContext
            .leak_and_drop_on_delete(state)
            .cast::<c_void>();
    }
}

/// Tries to serve a `cache_mode 'watch'` scan from the shared-memory cache.
/// Returns `None` to fall through to an RPC: no subscription yet (one is
/// requested), still syncing, or the cache infrastructure is unavailable
/// (with a WARNING so the fallback is never silent).
fn fetch_from_cache(state: &ScanState) -> Option<VecDeque<Row>> {
    let ns = state.filter.namespace.as_deref().unwrap_or("");
    let name = state.filter.name.as_deref().unwrap_or("");
    let looked_up = shmem::lookup_or_request(
        &state.config.server.target,
        state.config.server.rpc_timeout,
        &state.config.resource,
        ns,
    );
    let (slot, id, sub_state) = match looked_up {
        Ok(x) => x,
        Err(e @ (ShmemError::NotAvailable | ShmemError::Full)) => {
            warning!("axiom: cache_mode 'watch' unavailable ({e}); serving this scan on demand");
            return None;
        }
        Err(e) => {
            warning!("axiom: watch lookup failed ({e}); serving this scan on demand");
            return None;
        }
    };
    let tier = decide_tier(CacheMode::Watch, Some(sub_state));
    if tier == Tier::OnDemand {
        // A watch table that is not servable yet is usually just building its
        // cache, which is not worth a warning on every scan of a healthy
        // system. A cache that filled *before* it finished building is
        // different: it will not finish on its own, and this is the only path
        // by which the person running the query would ever learn that caching
        // stopped. The logs say so and `axiom_watch_status()` says so, but
        // neither is in front of them (issue #17).
        if shmem::reason(slot, id).unwrap_or_default() == CACHE_FULL_REASON {
            warning!(
                "axiom: not caching {} ({CACHE_FULL_REASON}); serving this scan from the gateway; see axiom_watch_status()",
                state.config.resource
            );
        }
        return None;
    }
    match shmem::scan(slot, id, ns, name) {
        Ok(objects) => {
            if tier == Tier::Stale {
                // Never mask staleness (docs/DESIGN.md §5.3): say so on every scan.
                let reason = shmem::reason(slot, id).unwrap_or_default();
                warning!(
                    "axiom: serving STALE data for {} from the watch cache: the watch is DEGRADED ({reason}); see axiom_watch_status()",
                    state.config.resource
                );
            }
            Some(decode_rows(&state.schema, &objects))
        }
        Err(e) => {
            warning!("axiom: cache read failed ({e}); serving this scan on demand");
            None
        }
    }
}

/// Fetches the next page of rows, advancing the scan's cursor.
///
/// An on-demand scan reads a collection over several RPCs rather than one,
/// because a whole collection does not fit a single gRPC message on any
/// cluster of size. The page size and the resume point are the gateway's to
/// choose; this only carries its token back.
///
/// A cache-served scan has no pages: when the table is in `cache_mode 'watch'`
/// and its subscription is servable, the shared-memory cache already holds the
/// whole collection, so it returns everything at once and marks the scan
/// exhausted. An impossible filter returns nothing and does the same.
fn fetch_rows(state: &mut ScanState) -> VecDeque<Row> {
    if state.filter.impossible {
        state.exhausted = true;
        return VecDeque::new();
    }
    // The cache is consulted once, before the first page, and never again.
    //
    // A subscription can become servable partway through a scan. Probing again
    // on a later page would hand back the whole collection from the cache on
    // top of the pages already returned, duplicating every row read so far and
    // mixing two snapshots. Which source a scan uses is decided when it starts
    // and holds for its lifetime.
    if state.next_page.is_none() && state.config.cache_mode == CacheMode::Watch {
        if let Some(rows) = fetch_from_cache(state) {
            state.exhausted = true;
            // Marks the source as chosen, so a later page cannot re-probe.
            state.next_page = Some(String::new());
            return rows;
        }
    }
    let token = state.next_page.clone().unwrap_or_default();
    match client::list_page(
        &state.config.server,
        &state.config.resource,
        &state.filter,
        &token,
    ) {
        Ok(page) => {
            state.exhausted = page.continue_token.is_empty();
            state.next_page = Some(page.continue_token);
            decode_rows(&state.schema, &page.objects)
        }
        Err(e) if e.class() == ErrorClass::Conflict => {
            // The gateway reports an expired continuation as a conflict, the
            // same class as a write losing a race, and the generic wording for
            // that is "re-read the row and retry" -- wrong advice here. This
            // scan has already returned rows to the executor from an earlier
            // page, so there is nothing to re-read: the whole scan has to
            // start again. Same SQLSTATE, because the recourse is still a
            // retry and callers catch it by name.
            raise(
                client_sqlstate(&e),
                format!(
                    "axiom: the listing expired part way through this scan and cannot be \
                     resumed; the snapshot it was reading was compacted by the API \
                     server. Run the query again (or retry the transaction): {e}"
                ),
            )
        }
        Err(e) => raise_client("list", &state.config.server, &e),
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn iterate_foreign_scan(
    node: *mut pg_sys::ForeignScanState,
) -> *mut pg_sys::TupleTableSlot {
    // SAFETY: called by the executor/planner with valid node pointers; see module docs.
    unsafe {
        let slot = (*node).ss.ss_ScanTupleSlot;
        let state_ptr = (*node).fdw_state.cast::<ScanState>();
        if state_ptr.is_null() {
            raise(
                PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
                "axiom: scan state missing in IterateForeignScan".to_owned(),
            );
        }
        let state = &mut *state_ptr;
        // Fetch when nothing is buffered, and again whenever the buffer empties
        // while the gateway still has pages. A page can legitimately come back
        // empty while a continue token remains, so this loops rather than
        // fetching once.
        while state.rows.as_ref().is_none_or(VecDeque::is_empty) {
            if state.rows.is_some() && state.exhausted {
                break;
            }
            let page = fetch_rows(state);
            state.rows = Some(page);
        }
        let Some(row) = state.rows.as_mut().and_then(VecDeque::pop_front) else {
            // End of scan: the executor stops only on an *empty* slot, so clear
            // whatever the previous iteration stored (ExecClearTuple is a C
            // static inline; call the slot's own clear op).
            if let Some(clear) = (*(*slot).tts_ops).clear {
                clear(slot);
            }
            return slot;
        };
        // Row datums belong to the per-tuple context, which ExecScan resets per row.
        let per_tuple = (*(*node).ss.ps.ps_ExprContext).ecxt_per_tuple_memory;
        store_row(slot, &state.schema, &row, per_tuple);
        slot
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn rescan_foreign_scan(node: *mut pg_sys::ForeignScanState) {
    // SAFETY: called by the executor/planner with valid node pointers; see module docs.
    unsafe {
        let state_ptr = (*node).fdw_state.cast::<ScanState>();
        if !state_ptr.is_null() {
            // Quals with params could change between rescans; re-fetch.
            (*state_ptr).rows = None;
            // The paging cursor goes back with them. A nested loop join rescans
            // the inner side once per outer row, and a rescan that began part
            // way through a page would otherwise resume from the previous
            // iteration's cursor rather than from the start of the collection.
            //
            // Defensive rather than demonstrated: no query could be built that
            // shows the difference, because an exhausted cursor is stored as
            // the empty token, which reads as "start from the beginning" on
            // the next fetch. So a surviving cursor currently self-corrects
            // once it runs off the end. That is an accident of the
            // representation, not a property worth relying on -- give the
            // empty token a distinct meaning and the bug becomes real.
            (*state_ptr).next_page = None;
            (*state_ptr).exhausted = false;
        }
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn end_foreign_scan(_node: *mut pg_sys::ForeignScanState) {
    // State is owned by the per-query memory context (see begin_foreign_scan).
}

// --- modify callbacks ----------------------------------------------------------------

/// Name of the resjunk column carrying the old `raw` value through UPDATE/DELETE plans.
const JUNK_RAW: &CStr = c"axiom_raw";

/// Per-statement modify state, allocated in the per-query memory context.
struct ModifyState {
    config: ScanConfig,
    schema: TableSchema,
    /// Attribute number of `axiom_raw` in the subplan's output (UPDATE/DELETE), else 0.
    junk_raw_attno: i16,
}

/// Which DML a kind supports. Pods: none. `ConfigMaps`: all three.
#[pg_guard]
unsafe extern "C-unwind" fn is_foreign_rel_updatable(rel: pg_sys::Relation) -> c_int {
    // SAFETY: rel is an open relation supplied by the planner/executor.
    let cfg = unsafe { scan_config((*rel).rd_id) };
    if cfg.writable {
        (1 << pg_sys::CmdType::CMD_INSERT)
            | (1 << pg_sys::CmdType::CMD_UPDATE)
            | (1 << pg_sys::CmdType::CMD_DELETE)
    } else {
        0
    }
}

/// Adds the row's `raw` column as a resjunk target so UPDATE/DELETE carry the
/// object identity and the `resourceVersion` it was read at.
#[pg_guard]
unsafe extern "C-unwind" fn add_foreign_update_targets(
    root: *mut pg_sys::PlannerInfo,
    rtindex: pg_sys::Index,
    _target_rte: *mut pg_sys::RangeTblEntry,
    target_relation: pg_sys::Relation,
) {
    // SAFETY: planner-supplied pointers; tupdesc is live for the relation.
    unsafe {
        let tupdesc = (*target_relation).rd_att;
        let natts = usize::try_from((*tupdesc).natts).unwrap_or(0);
        let mut raw_attno: Option<i16> = None;
        for i in 0..natts {
            let att = tupdesc_attr(tupdesc, i);
            if (*att).attisdropped {
                continue;
            }
            let name = CStr::from_ptr((*att).attname.data.as_ptr().cast::<c_char>());
            if name.to_bytes() == b"raw" && (*att).atttypid == pg_sys::JSONBOID {
                raw_attno = Some((*att).attnum);
            }
        }
        let Some(attno) = raw_attno else {
            raise(
                PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED,
                "UPDATE/DELETE on an axiom foreign table requires a \"raw jsonb\" column (it carries the object's identity and resourceVersion)".to_owned(),
            );
        };
        // Var.varno is `int` from pg16 on, which is the supported floor, so
        // the range table index is narrowed rather than passed through.
        let Ok(varno) = i32::try_from(rtindex) else {
            raise(
                PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
                format!("axiom: range table index {rtindex} out of range"),
            );
        };
        let var = pg_sys::makeVar(varno, attno, pg_sys::JSONBOID, -1, pg_sys::InvalidOid, 0);
        pg_sys::add_row_identity_var(root, var, rtindex, JUNK_RAW.as_ptr());
    }
}

/// No private plan data: everything is re-derived from the catalogs at Begin.
#[pg_guard]
unsafe extern "C-unwind" fn plan_foreign_modify(
    _root: *mut pg_sys::PlannerInfo,
    _plan: *mut pg_sys::ModifyTable,
    _result_relation: pg_sys::Index,
    _subplan_index: c_int,
) -> *mut pg_sys::List {
    std::ptr::null_mut()
}

#[pg_guard]
unsafe extern "C-unwind" fn begin_foreign_modify(
    mtstate: *mut pg_sys::ModifyTableState,
    rinfo: *mut pg_sys::ResultRelInfo,
    _fdw_private: *mut pg_sys::List,
    _subplan_index: c_int,
    eflags: c_int,
) {
    // SAFETY: executor-supplied pointers, valid for the statement.
    unsafe {
        if eflags & pg_sys::EXEC_FLAG_EXPLAIN_ONLY.cast_signed() != 0 {
            return;
        }
        let rel = (*rinfo).ri_RelationDesc;
        let config = scan_config((*rel).rd_id);
        if !config.writable {
            raise(
                PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED,
                WriteError::ReadOnly(config.resource.to_string()).to_string(),
            );
        }
        let schema = resolve_schema((*rel).rd_att, &config.resource, config.writable);
        let mut junk_raw_attno: i16 = 0;
        let op = (*mtstate).operation;
        if op == pg_sys::CmdType::CMD_UPDATE || op == pg_sys::CmdType::CMD_DELETE {
            let subplan = (*(*mtstate).ps.lefttree).plan;
            junk_raw_attno =
                pg_sys::ExecFindJunkAttributeInTlist((*subplan).targetlist, JUNK_RAW.as_ptr());
            if junk_raw_attno <= 0 {
                raise(
                    PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
                    "axiom: axiom_raw junk column missing from modify subplan".to_owned(),
                );
            }
        }
        let state = ModifyState {
            config,
            schema,
            junk_raw_attno,
        };
        (*rinfo).ri_FdwState = PgMemoryContexts::CurrentMemoryContext
            .leak_and_drop_on_delete(state)
            .cast::<c_void>();
    }
}

/// Returns the modify state stored by `begin_foreign_modify`.
unsafe fn modify_state<'a>(rinfo: *mut pg_sys::ResultRelInfo) -> &'a ModifyState {
    // SAFETY: ri_FdwState was set to a leaked ModifyState in begin_foreign_modify.
    unsafe {
        let p = (*rinfo).ri_FdwState.cast::<ModifyState>();
        if p.is_null() {
            raise(
                PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
                "axiom: modify state missing".to_owned(),
            );
        }
        &*p
    }
}

/// Reads the old `raw` object from the subplan's junk column.
unsafe fn old_raw(
    state: &ModifyState,
    plan_slot: *mut pg_sys::TupleTableSlot,
) -> serde_json::Value {
    // SAFETY: junk_raw_attno was located in this subplan's targetlist at Begin.
    let datum = unsafe { slot_datum(plan_slot, state.junk_raw_attno) };
    match datum.and_then(|d| unsafe { JsonB::from_datum(d, false) }) {
        Some(j) => j.0,
        None => raise(
            PgSqlErrorCode::ERRCODE_FDW_ERROR,
            WriteError::BadOldRaw("raw column is NULL").to_string(),
        ),
    }
}

/// Memory context for RETURNING datums: the per-tuple context of the modify node.
unsafe fn per_tuple_memcx(estate: *mut pg_sys::EState) -> pg_sys::MemoryContext {
    // SAFETY: estate is live for the statement; per-output-tuple context is reset per row.
    unsafe {
        // Mirror the C `GetPerTupleExprContext` macro: reuse the statement's
        // context, creating it only once. Calling MakePerTupleExprContext on
        // every row would register a fresh context per row until statement end.
        let mut ctx = (*estate).es_per_tuple_exprcontext;
        if ctx.is_null() {
            ctx = pg_sys::MakePerTupleExprContext(estate);
        }
        (*ctx).ecxt_per_tuple_memory
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn exec_foreign_insert(
    estate: *mut pg_sys::EState,
    rinfo: *mut pg_sys::ResultRelInfo,
    slot: *mut pg_sys::TupleTableSlot,
    _plan_slot: *mut pg_sys::TupleTableSlot,
) -> *mut pg_sys::TupleTableSlot {
    // SAFETY: executor-supplied pointers.
    unsafe {
        let state = modify_state(rinfo);
        let new = new_row_from_slot(slot, &state.schema);
        let write = match table::insert_body(&state.schema, &new) {
            Ok(w) => w,
            Err(e) => raise(write_sqlstate(&e), format!("axiom: INSERT: {e}")),
        };
        let json = match client::create(
            &state.config.server,
            &state.config.resource,
            &write.identity,
            &write.body,
        ) {
            Ok(j) => j,
            Err(e) => raise_client("INSERT", &state.config.server, &e),
        };
        // Reflect the stored object (uid, resourceVersion, defaults) for RETURNING.
        if let Some(row) = decode_rows(&state.schema, &[json]).pop_front() {
            store_row(slot, &state.schema, &row, per_tuple_memcx(estate));
        }
        slot
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn exec_foreign_update(
    estate: *mut pg_sys::EState,
    rinfo: *mut pg_sys::ResultRelInfo,
    slot: *mut pg_sys::TupleTableSlot,
    plan_slot: *mut pg_sys::TupleTableSlot,
) -> *mut pg_sys::TupleTableSlot {
    // SAFETY: executor-supplied pointers.
    unsafe {
        let state = modify_state(rinfo);
        let old = old_raw(state, plan_slot);
        let new = new_row_from_slot(slot, &state.schema);
        let write = match table::update_body(&state.schema, &old, &new) {
            Ok(w) => w,
            Err(e) => raise(write_sqlstate(&e), format!("axiom: UPDATE: {e}")),
        };
        let json = match client::update(
            &state.config.server,
            &state.config.resource,
            &write.identity,
            &write.body,
        ) {
            Ok(j) => j,
            Err(e) => raise_client("UPDATE", &state.config.server, &e),
        };
        if let Some(row) = decode_rows(&state.schema, &[json]).pop_front() {
            store_row(slot, &state.schema, &row, per_tuple_memcx(estate));
        }
        slot
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn exec_foreign_delete(
    estate: *mut pg_sys::EState,
    rinfo: *mut pg_sys::ResultRelInfo,
    slot: *mut pg_sys::TupleTableSlot,
    plan_slot: *mut pg_sys::TupleTableSlot,
) -> *mut pg_sys::TupleTableSlot {
    // SAFETY: executor-supplied pointers.
    unsafe {
        let state = modify_state(rinfo);
        let old = old_raw(state, plan_slot);
        let id = match table::identity_from_raw(&old) {
            Ok(id) => id,
            Err(e) => raise(write_sqlstate(&e), format!("axiom: DELETE: {e}")),
        };
        if let Err(e) = client::delete(&state.config.server, &state.config.resource, &id) {
            raise_client("DELETE", &state.config.server, &e);
        }
        // RETURNING sees the row as it was read.
        if let Ok(row) = state.schema.row_from_value(&old) {
            store_row(slot, &state.schema, &row, per_tuple_memcx(estate));
        }
        slot
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn end_foreign_modify(
    _estate: *mut pg_sys::EState,
    _rinfo: *mut pg_sys::ResultRelInfo,
) {
    // State is owned by the per-query memory context (see begin_foreign_modify).
}

// --- IMPORT FOREIGN SCHEMA ---------------------------------------------------------
//
// The only place the extension asks the gateway about schema. It runs at DDL
// time and writes the resolved identity into each generated table's options, so
// no scan ever needs discovery.

/// Reads `IMPORT FOREIGN SCHEMA ... OPTIONS (...)`.
///
/// The accepted names come from [`import::IMPORT_OPTION_DOCS`], which is also
/// what `make docs-generate` renders into the reference page. Interpreting a
/// value still needs a match arm per option, but *which* names are accepted
/// has one definition, so an option cannot be added here and go undocumented,
/// or documented and not accepted.
fn import_options(opts: &[(String, String)]) -> ImportOptions {
    let mut out = ImportOptions::default();
    for (k, v) in opts {
        if !import::IMPORT_OPTION_DOCS.iter().any(|d| d.name == k) {
            let names: Vec<&str> = import::IMPORT_OPTION_DOCS.iter().map(|d| d.name).collect();
            raise(
                PgSqlErrorCode::ERRCODE_FDW_INVALID_OPTION_NAME,
                format!(
                    "invalid option {k:?} for IMPORT FOREIGN SCHEMA: valid options are {}",
                    names.join(", ")
                ),
            );
        }
        match k.as_str() {
            "cache_mode" => match CacheMode::parse(v) {
                Some(m) => out.cache_mode = m,
                None => raise(
                    PgSqlErrorCode::ERRCODE_FDW_INVALID_OPTION_NAME,
                    format!("option \"cache_mode\" {v:?} is not supported; valid values: on_demand, watch"),
                ),
            },
            "prefix" => {
                if !v.is_empty() && !import::is_safe_ident(v) {
                    raise(
                        PgSqlErrorCode::ERRCODE_FDW_INVALID_OPTION_NAME,
                        format!(
                            "option \"prefix\" {v:?} is not a usable SQL identifier prefix \
                             (lowercase letters, digits and underscores only)"
                        ),
                    );
                }
                out.prefix.clone_from(v);
            }
            // Unreachable: the allowlist check above rejects any other name.
            // Kept so adding an entry to IMPORT_OPTION_DOCS without a match
            // arm here fails loudly at DDL time rather than being ignored.
            other => raise(
                PgSqlErrorCode::ERRCODE_FDW_INVALID_OPTION_NAME,
                format!(
                    "option {other:?} is documented for IMPORT FOREIGN SCHEMA \
                     but not implemented"
                ),
            ),
        }
    }
    out
}

/// Converts one wire schema into the DDL generator's input.
///
/// The column type comes from the wire enum rather than from a name, so a
/// gateway sending an unspecified type yields no column instead of a column
/// Postgres would reject at `CREATE` time.
fn import_kind_from_wire(k: &crate::proto::v1::KindSchema) -> Option<ImportKind> {
    let gvk = k.gvk.as_ref()?;
    let columns = k
        .columns
        .iter()
        .filter_map(|c| {
            let sql_type = match crate::proto::v1::SqlType::try_from(c.sql_type).ok()? {
                crate::proto::v1::SqlType::Text => "text",
                crate::proto::v1::SqlType::Jsonb => "jsonb",
                crate::proto::v1::SqlType::Unspecified => return None,
            };
            Some(ImportColumn {
                name: c.name.clone(),
                sql_type,
            })
        })
        .collect();
    Some(ImportKind {
        group: gvk.group.clone(),
        version: gvk.version.clone(),
        kind: gvk.kind.clone(),
        plural: k.plural.clone(),
        namespaced: k.namespaced,
        writable: k.writable,
        watchable: k.watchable,
        columns,
    })
}

/// Reads the `RangeVar` table names of a `LIMIT TO` / `EXCEPT` clause.
unsafe fn table_names(list: *mut pg_sys::List) -> Vec<String> {
    // SAFETY: the planner supplies a List of RangeVar for these clauses.
    unsafe {
        let mut out = Vec::new();
        if list.is_null() {
            return out;
        }
        let elems = (*list).elements;
        for i in 0..usize::try_from((*list).length).unwrap_or(0) {
            let node = (*elems.add(i)).ptr_value.cast::<pg_sys::RangeVar>();
            if node.is_null() || (*node).relname.is_null() {
                continue;
            }
            out.push(
                CStr::from_ptr((*node).relname)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        out
    }
}

/// Implements `IMPORT FOREIGN SCHEMA`.
///
/// The remote schema names an API group: `k8s` for everything this gateway
/// serves, `core` or `v1` for the core group, or a group name verbatim. `LIMIT
/// TO` is pushed to the gateway as a plural-name filter so the round-trip only
/// carries what was asked for; `EXCEPT` is applied locally, since the gateway
/// has no way to express a negative filter.
///
/// A kind that cannot be represented as a table is skipped with a `WARNING`
/// naming it, and the rest of the import proceeds. Failing the whole statement
/// because one CRD in the cluster has an unusable name would make IMPORT
/// unusable on exactly the clusters it is most needed on.
#[pg_guard]
unsafe extern "C-unwind" fn import_foreign_schema(
    stmt: *mut pg_sys::ImportForeignSchemaStmt,
    server_oid: pg_sys::Oid,
) -> *mut pg_sys::List {
    // SAFETY: the planner supplies a valid statement node and server OID.
    unsafe {
        let server = pg_sys::GetForeignServer(server_oid);
        let server_opts = match ServerOptions::parse(&options_from_list((*server).options)) {
            Ok(s) => s,
            Err(e) => raise(options_sqlstate(&e), format!("foreign server: {e}")),
        };
        let server_name = CStr::from_ptr((*server).servername)
            .to_string_lossy()
            .into_owned();
        let remote_schema = CStr::from_ptr((*stmt).remote_schema)
            .to_string_lossy()
            .into_owned();
        let local_schema = CStr::from_ptr((*stmt).local_schema)
            .to_string_lossy()
            .into_owned();
        let opts = import_options(&options_from_list((*stmt).options));

        let requested = table_names((*stmt).table_list);
        let limit_to =
            (*stmt).list_type == pg_sys::ImportForeignSchemaType::FDW_IMPORT_SCHEMA_LIMIT_TO;
        let except = (*stmt).list_type == pg_sys::ImportForeignSchemaType::FDW_IMPORT_SCHEMA_EXCEPT;

        let group = import::group_filter(&remote_schema, &server_name);
        let plurals: Vec<String> = if limit_to {
            requested.clone()
        } else {
            Vec::new()
        };
        let kinds = match client::list_kinds(&server_opts, group.as_deref(), &plurals) {
            Ok(k) => k,
            Err(e) => raise_import("list_kinds", &server_opts, &e),
        };

        // Decide every table name up front: a plural is only unique within an
        // API group, so the name a kind gets depends on the rest of the set.
        let selected: Vec<ImportKind> = kinds
            .iter()
            .filter_map(import_kind_from_wire)
            .filter(|k| !(except && requested.contains(&k.plural)))
            .collect();
        let table_names = import::assign_table_names(&selected, &opts.prefix);

        let mut statements = Vec::with_capacity(selected.len());
        for (kind, table) in selected.iter().zip(&table_names) {
            if table.is_empty() {
                pgrx::warning!(
                    "axiom: skipping {}: no usable table name is available for it",
                    kind.plural
                );
                continue;
            }
            match import::create_table_sql(kind, table, &server_name, &local_schema, &opts) {
                Ok((sql, dropped)) => {
                    if !dropped.is_empty() {
                        pgrx::warning!(
                            "axiom: {} column(s) of {} were skipped and are reachable only \
                             through \"raw\": {}",
                            dropped.len(),
                            kind.plural,
                            dropped.join(", ")
                        );
                    }
                    statements.push(sql);
                }
                Err(reason) => pgrx::warning!(
                    "axiom: skipping {}: {reason}",
                    if kind.plural.is_empty() {
                        "an unnamed resource".to_owned()
                    } else {
                        kind.plural.clone()
                    }
                ),
            }
        }

        if statements.is_empty() {
            pgrx::warning!(
                "axiom: no foreign tables were created for remote schema {remote_schema:?}; \
                 the gateway serves nothing matching it (its --serve allowlist bounds what \
                 it offers)"
            );
        }

        let mut list: *mut pg_sys::List = std::ptr::null_mut();
        for sql in statements {
            let c = std::ffi::CString::new(sql).unwrap_or_default();
            list = pg_sys::lappend(list, pg_sys::pstrdup(c.as_ptr()).cast());
        }
        list
    }
}
