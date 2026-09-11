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

use crate::client::{self, ClientError, ErrorClass};
use crate::kinds::{
    self, Cell, DecodeError, Kind, NewRow, Row, SqlType, WriteError, MAX_OBJECT_BYTES,
};
use crate::options::{self, Catalog, OptionsError, ServerOptions, TableOptions};
use crate::quals::{Filter, Qual};

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
        OptionsError::Endpoint(_) | OptionsError::Timeout(_) | OptionsError::Resource(_) => {
            PgSqlErrorCode::ERRCODE_FDW_INVALID_ATTRIBUTE_VALUE
        }
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
        WriteError::ReadOnly(_) | WriteError::IdentityChange(_) => {
            PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED
        }
        WriteError::MissingName | WriteError::MissingNamespace => {
            PgSqlErrorCode::ERRCODE_NOT_NULL_VIOLATION
        }
        WriteError::InvalidName(..)
        | WriteError::NotAnObject(_)
        | WriteError::DataValueNotString(_) => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
        WriteError::BadOldRaw(_) => PgSqlErrorCode::ERRCODE_FDW_ERROR,
    }
}

/// Raises the SQL error for a failed gateway call.
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
    kind: Kind,
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
            kind: table.resource,
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
unsafe extern "C" fn get_foreign_rel_size(
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
unsafe extern "C" fn get_foreign_paths(
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
        #[cfg(any(feature = "pg14", feature = "pg15", feature = "pg16"))]
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
        pg_sys::add_path(baserel, path.cast::<pg_sys::Path>());
    }
}

#[pg_guard]
unsafe extern "C" fn get_foreign_plan(
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
    /// One entry per attribute of the scan tuple: index into
    /// `kind.columns()`, or `None` for dropped columns.
    columns: Vec<Option<usize>>,
    /// `None` until the first `Iterate` fetches; then the remaining rows.
    rows: Option<VecDeque<Row>>,
}

/// Builds the attribute → kind-column map, validating names and types.
unsafe fn column_map(tupdesc: pg_sys::TupleDesc, kind: Kind) -> Vec<Option<usize>> {
    // SAFETY: tupdesc is a live TupleDesc supplied by the executor.
    unsafe {
        let natts = usize::try_from((*tupdesc).natts).unwrap_or(0);
        let mut out = Vec::with_capacity(natts);
        for i in 0..natts {
            let att = (*tupdesc).attrs.as_ptr().add(i);
            if (*att).attisdropped {
                out.push(None);
                continue;
            }
            let name = CStr::from_ptr((*att).attname.data.as_ptr().cast::<c_char>())
                .to_string_lossy()
                .into_owned();
            let Some(idx) = kind.column_index(&name) else {
                let supported: Vec<&str> = kind.columns().iter().map(|c| c.name).collect();
                raise(
                    PgSqlErrorCode::ERRCODE_FDW_COLUMN_NAME_NOT_FOUND,
                    format!(
                        "column {name:?} is not a {kind:?} column; supported columns: {}",
                        supported.join(", ")
                    ),
                );
            };
            let (want_oid, want_name) = match kind.columns()[idx].sql_type {
                SqlType::Text => (pg_sys::TEXTOID, "text"),
                SqlType::Jsonb => (pg_sys::JSONBOID, "jsonb"),
            };
            if (*att).atttypid != want_oid {
                raise(
                    PgSqlErrorCode::ERRCODE_FDW_INVALID_DATA_TYPE,
                    format!("column {name:?} must be of type {want_name}"),
                );
            }
            out.push(Some(idx));
        }
        out
    }
}

/// Decodes gateway JSON into rows, raising on malformed objects.
fn decode_rows(kind: Kind, objects: &[Vec<u8>]) -> VecDeque<Row> {
    let mut rows = VecDeque::with_capacity(objects.len());
    for json in objects {
        match kind.decode(json, MAX_OBJECT_BYTES) {
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
    columns: &[Option<usize>],
    row: &Row,
    memcx: pg_sys::MemoryContext,
) {
    // SAFETY: slot is the executor's slot for this relation; columns matches its tupdesc.
    unsafe {
        if let Some(clear) = (*(*slot).tts_ops).clear {
            clear(slot);
        }
        let natts = columns.len();
        let values = std::slice::from_raw_parts_mut((*slot).tts_values, natts);
        let isnull = std::slice::from_raw_parts_mut((*slot).tts_isnull, natts);
        PgMemoryContexts::For(memcx).switch_to(|_| {
            for (i, col) in columns.iter().enumerate() {
                let datum = match col.and_then(|c| row.cells.get(c)).and_then(Option::as_ref) {
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

/// Reads the new tuple's columns into a [`NewRow`] aligned with `kind.columns()`.
unsafe fn new_row_from_slot(
    slot: *mut pg_sys::TupleTableSlot,
    columns: &[Option<usize>],
    kind: Kind,
) -> NewRow {
    let mut cells = vec![None; kind.columns().len()];
    for (i, col) in columns.iter().enumerate() {
        let Some(idx) = *col else { continue };
        let attno = i16::try_from(i + 1).unwrap_or(0);
        // SAFETY: attno is within the slot's tupdesc by construction of `columns`.
        let Some(datum) = (unsafe { slot_datum(slot, attno) }) else {
            continue;
        };
        cells[idx] = match kind.columns()[idx].sql_type {
            // SAFETY: column types were checked against the tupdesc in column_map.
            SqlType::Text => unsafe { String::from_datum(datum, false) }.map(Cell::Text),
            SqlType::Jsonb => unsafe { JsonB::from_datum(datum, false) }.map(|j| Cell::Json(j.0)),
        };
    }
    NewRow { cells }
}

#[pg_guard]
unsafe extern "C" fn begin_foreign_scan(node: *mut pg_sys::ForeignScanState, eflags: c_int) {
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
        let columns = column_map((*rel).rd_att, config.kind);
        let state = ScanState {
            config,
            filter: Filter::from_quals(&quals),
            columns,
            rows: None,
        };
        // Dropped when the executor's per-query context is reset, error or not.
        (*node).fdw_state = PgMemoryContexts::CurrentMemoryContext
            .leak_and_drop_on_delete(state)
            .cast::<c_void>();
    }
}

/// Fetches all matching rows with one RPC (or none, for an impossible filter).
fn fetch_rows(state: &ScanState) -> VecDeque<Row> {
    if state.filter.impossible {
        return VecDeque::new();
    }
    match client::list(&state.config.server, state.config.kind, &state.filter) {
        Ok(objects) => decode_rows(state.config.kind, &objects),
        Err(e) => raise_client("list", &state.config.server, &e),
    }
}

#[pg_guard]
unsafe extern "C" fn iterate_foreign_scan(
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
        if state.rows.is_none() {
            state.rows = Some(fetch_rows(state));
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
        store_row(slot, &state.columns, &row, per_tuple);
        slot
    }
}

#[pg_guard]
unsafe extern "C" fn rescan_foreign_scan(node: *mut pg_sys::ForeignScanState) {
    // SAFETY: called by the executor/planner with valid node pointers; see module docs.
    unsafe {
        let state_ptr = (*node).fdw_state.cast::<ScanState>();
        if !state_ptr.is_null() {
            // Quals with params could change between rescans; re-fetch.
            (*state_ptr).rows = None;
        }
    }
}

#[pg_guard]
unsafe extern "C" fn end_foreign_scan(_node: *mut pg_sys::ForeignScanState) {
    // State is owned by the per-query memory context (see begin_foreign_scan).
}

// --- modify callbacks ----------------------------------------------------------------

/// Name of the resjunk column carrying the old `raw` value through UPDATE/DELETE plans.
const JUNK_RAW: &CStr = c"axiom_raw";

/// Per-statement modify state, allocated in the per-query memory context.
struct ModifyState {
    config: ScanConfig,
    columns: Vec<Option<usize>>,
    /// Attribute number of `axiom_raw` in the subplan's output (UPDATE/DELETE), else 0.
    junk_raw_attno: i16,
}

/// Which DML a kind supports. Pods: none. `ConfigMaps`: all three.
#[pg_guard]
unsafe extern "C" fn is_foreign_rel_updatable(rel: pg_sys::Relation) -> c_int {
    // SAFETY: rel is an open relation supplied by the planner/executor.
    let cfg = unsafe { scan_config((*rel).rd_id) };
    if cfg.kind.writable() {
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
unsafe extern "C" fn add_foreign_update_targets(
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
            let att = (*tupdesc).attrs.as_ptr().add(i);
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
        // Var.varno is `Index` on pg14/15 and `int` on pg16+; makeVar follows suit.
        #[cfg(any(feature = "pg14", feature = "pg15"))]
        let varno = rtindex;
        #[cfg(not(any(feature = "pg14", feature = "pg15")))]
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
unsafe extern "C" fn plan_foreign_modify(
    _root: *mut pg_sys::PlannerInfo,
    _plan: *mut pg_sys::ModifyTable,
    _result_relation: pg_sys::Index,
    _subplan_index: c_int,
) -> *mut pg_sys::List {
    std::ptr::null_mut()
}

#[pg_guard]
unsafe extern "C" fn begin_foreign_modify(
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
        if !config.kind.writable() {
            raise(
                PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED,
                WriteError::ReadOnly(config.kind).to_string(),
            );
        }
        let columns = column_map((*rel).rd_att, config.kind);
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
            columns,
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
    unsafe { (*pg_sys::MakePerTupleExprContext(estate)).ecxt_per_tuple_memory }
}

#[pg_guard]
unsafe extern "C" fn exec_foreign_insert(
    estate: *mut pg_sys::EState,
    rinfo: *mut pg_sys::ResultRelInfo,
    slot: *mut pg_sys::TupleTableSlot,
    _plan_slot: *mut pg_sys::TupleTableSlot,
) -> *mut pg_sys::TupleTableSlot {
    // SAFETY: executor-supplied pointers.
    unsafe {
        let state = modify_state(rinfo);
        let kind = state.config.kind;
        let new = new_row_from_slot(slot, &state.columns, kind);
        let write = match kinds::insert_body(kind, &new) {
            Ok(w) => w,
            Err(e) => raise(write_sqlstate(&e), format!("axiom: INSERT: {e}")),
        };
        let json = match client::create(&state.config.server, kind, &write.identity, &write.body) {
            Ok(j) => j,
            Err(e) => raise_client("INSERT", &state.config.server, &e),
        };
        // Reflect the stored object (uid, resourceVersion, defaults) for RETURNING.
        if let Some(row) = decode_rows(kind, &[json]).pop_front() {
            store_row(slot, &state.columns, &row, per_tuple_memcx(estate));
        }
        slot
    }
}

#[pg_guard]
unsafe extern "C" fn exec_foreign_update(
    estate: *mut pg_sys::EState,
    rinfo: *mut pg_sys::ResultRelInfo,
    slot: *mut pg_sys::TupleTableSlot,
    plan_slot: *mut pg_sys::TupleTableSlot,
) -> *mut pg_sys::TupleTableSlot {
    // SAFETY: executor-supplied pointers.
    unsafe {
        let state = modify_state(rinfo);
        let kind = state.config.kind;
        let old = old_raw(state, plan_slot);
        let new = new_row_from_slot(slot, &state.columns, kind);
        let write = match kinds::update_body(kind, &old, &new) {
            Ok(w) => w,
            Err(e) => raise(write_sqlstate(&e), format!("axiom: UPDATE: {e}")),
        };
        let json = match client::update(&state.config.server, kind, &write.identity, &write.body) {
            Ok(j) => j,
            Err(e) => raise_client("UPDATE", &state.config.server, &e),
        };
        if let Some(row) = decode_rows(kind, &[json]).pop_front() {
            store_row(slot, &state.columns, &row, per_tuple_memcx(estate));
        }
        slot
    }
}

#[pg_guard]
unsafe extern "C" fn exec_foreign_delete(
    estate: *mut pg_sys::EState,
    rinfo: *mut pg_sys::ResultRelInfo,
    slot: *mut pg_sys::TupleTableSlot,
    plan_slot: *mut pg_sys::TupleTableSlot,
) -> *mut pg_sys::TupleTableSlot {
    // SAFETY: executor-supplied pointers.
    unsafe {
        let state = modify_state(rinfo);
        let kind = state.config.kind;
        let old = old_raw(state, plan_slot);
        let id = match kinds::identity_from_raw(&old) {
            Ok(id) => id,
            Err(e) => raise(write_sqlstate(&e), format!("axiom: DELETE: {e}")),
        };
        if let Err(e) = client::delete(&state.config.server, kind, &id) {
            raise_client("DELETE", &state.config.server, &e);
        }
        // RETURNING sees the row as it was read.
        if let Ok(row) = Row::from_value(kind, &old) {
            store_row(slot, &state.columns, &row, per_tuple_memcx(estate));
        }
        slot
    }
}

#[pg_guard]
unsafe extern "C" fn end_foreign_modify(
    _estate: *mut pg_sys::EState,
    _rinfo: *mut pg_sys::ResultRelInfo,
) {
    // State is owned by the per-query memory context (see begin_foreign_modify).
}
