//! Foreign data wrapper glue: the Postgres FDW callbacks for read-only,
//! on-demand scans (Phase 1). This file is deliberately thin. Everything
//! decidable without Postgres (options, qual → filter, JSON → row, error
//! classification) lives in the pure modules and is unit-tested there.
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
//! Errors: every failure is raised as a proper SQL error with an FDW SQLSTATE;
//! nothing here panics on bad input.

use std::collections::VecDeque;
use std::ffi::{c_char, c_int, c_void, CStr};

use pgrx::prelude::*;
use pgrx::{pg_sys, JsonB, PgList, PgMemoryContexts};

use crate::client::{list_pods, ClientError, ErrorClass};
use crate::options::{self, Catalog, OptionsError, ServerOptions, TableOptions};
use crate::pods::{PodColumn, PodError, PodRow, SqlType, MAX_OBJECT_BYTES};
use crate::quals::{PodFilter, Qual};

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
        ErrorClass::InvalidRequest | ErrorClass::GatewayUnconfigured | ErrorClass::Internal => {
            PgSqlErrorCode::ERRCODE_FDW_ERROR
        }
    }
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
    #[allow(
        dead_code,
        reason = "Phase 1 serves one resource; kept so the scan path is resource-aware from the start"
    )]
    table: TableOptions,
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
        ScanConfig { server, table }
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
        let filter = PodFilter::from_quals(&quals);
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
    filter: PodFilter,
    /// One entry per attribute of the scan tuple; `None` for dropped columns.
    columns: Vec<Option<PodColumn>>,
    /// `None` until the first `Iterate` fetches; then the remaining rows.
    rows: Option<VecDeque<PodRow>>,
}

/// Builds the attribute → column map, validating names and types.
unsafe fn column_map(tupdesc: pg_sys::TupleDesc) -> Vec<Option<PodColumn>> {
    // SAFETY: called by the executor/planner with valid node pointers; see module docs.
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
            let Some(col) = PodColumn::from_name(&name) else {
                raise(
                    PgSqlErrorCode::ERRCODE_FDW_COLUMN_NAME_NOT_FOUND,
                    format!(
                        "column {name:?} is not a Pod column; supported columns: {}",
                        PodColumn::NAMES.join(", ")
                    ),
                );
            };
            let (want_oid, want_name) = match col.sql_type() {
                SqlType::Text => (pg_sys::TEXTOID, "text"),
                SqlType::Jsonb => (pg_sys::JSONBOID, "jsonb"),
            };
            if (*att).atttypid != want_oid {
                raise(
                    PgSqlErrorCode::ERRCODE_FDW_INVALID_DATA_TYPE,
                    format!("column {name:?} must be of type {want_name}"),
                );
            }
            out.push(Some(col));
        }
        out
    }
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
        let state = ScanState {
            config: scan_config(rel_oid),
            filter: PodFilter::from_quals(&quals),
            columns: column_map((*rel).rd_att),
            rows: None,
        };
        // Dropped when the executor's per-query context is reset, error or not.
        (*node).fdw_state = PgMemoryContexts::CurrentMemoryContext
            .leak_and_drop_on_delete(state)
            .cast::<c_void>();
    }
}

/// Fetches all matching rows with one RPC (or none, for an impossible filter).
fn fetch_rows(state: &ScanState) -> VecDeque<PodRow> {
    if state.filter.impossible {
        return VecDeque::new();
    }
    let objects = match list_pods(&state.config.server, &state.filter) {
        Ok(o) => o,
        Err(e) => raise(
            client_sqlstate(&e),
            format!(
                "axiom: cannot reach gateway {}: {e}",
                state.config.server.target.endpoint
            ),
        ),
    };
    let mut rows = VecDeque::with_capacity(objects.len());
    for json in &objects {
        match PodRow::from_json(json, MAX_OBJECT_BYTES) {
            Ok(r) => rows.push_back(r),
            Err(e @ (PodError::TooLarge { .. } | PodError::Json(_) | PodError::Shape(_))) => {
                raise(
                    PgSqlErrorCode::ERRCODE_FDW_INVALID_DATA_TYPE,
                    format!("axiom: bad object from gateway: {e}"),
                );
            }
        }
    }
    rows
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
        // Clear via the slot's own ops (ExecClearTuple is a static inline in C).
        if let Some(clear) = (*(*slot).tts_ops).clear {
            clear(slot);
        }
        let Some(row) = state.rows.as_mut().and_then(VecDeque::pop_front) else {
            return slot; // empty slot = end of scan
        };
        let natts = state.columns.len();
        let values = std::slice::from_raw_parts_mut((*slot).tts_values, natts);
        let isnull = std::slice::from_raw_parts_mut((*slot).tts_isnull, natts);
        // Row datums belong to the per-tuple context, which ExecScan resets per row.
        let per_tuple = (*(*node).ss.ps.ps_ExprContext).ecxt_per_tuple_memory;
        PgMemoryContexts::For(per_tuple).switch_to(|_| {
            for (i, col) in state.columns.iter().enumerate() {
                let datum = match col {
                    None => None,
                    Some(PodColumn::Raw) => JsonB(row.raw.clone()).into_datum(),
                    Some(c) => row
                        .text(*c)
                        .map(str::to_owned)
                        .and_then(IntoDatum::into_datum),
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
