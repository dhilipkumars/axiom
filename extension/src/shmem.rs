//! Shared memory: the fixed control segment (subscription table) and the
//! DSA-backed object cache. This is the impure side of `cache.rs`.
//!
//! Layout
//! - `CONTROL`: a pgrx `PgLwLock<Control>` in the main shared-memory segment,
//!   allocated at postmaster start when the library is preloaded. It holds up
//!   to [`MAX_SUBS`] subscription slots plus the DSA handle.
//! - Objects live in a DSA area created by the background worker and pinned
//!   for the postmaster's lifetime. Each subscription owns a chained hash
//!   index (bucket array + entries) inside that area.
//!
//! Locking: one `LWLock` for everything. The worker takes it exclusively for
//! writes; backends take it shared to copy matching objects out, then decode
//! outside the lock. Adequate for this phase; partitioned locking (dshash)
//! is a Phase 8 concern once contention is measurable. dshash itself has no
//! pgrx bindings and its parameter struct changed layout in pg17, which is why
//! the index is self-managed here.
//!
//! Safety: every DSA pointer dereference is bounds-checked against the entry
//! header it was read from, and object JSON is size-capped before it is
//! written (docs/RULES.md §3).

use std::cell::Cell;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pgrx::pg_sys;
use pgrx::prelude::*;
use pgrx::shmem::PGRXSharedMemory;
use pgrx::{pg_shmem_init, PgLwLock};

use crate::cache::{buckets_for, key_hash, key_matches, tombstone_expired, SubState};
use crate::resource::Resource;
use crate::table::MAX_OBJECT_BYTES;
use crate::transport::Target;

/// Maximum concurrent subscriptions (cluster × kind × namespace filter).
pub const MAX_SUBS: usize = 64;

const ENDPOINT_MAX: usize = 256;
const CA_MAX: usize = 512;
const NS_MAX: usize = 64;
/// Kubernetes resourceVersions are opaque; in practice small decimal strings.
/// A token that does not fit is never truncated: `set_bookmark` rejects it and
/// the worker falls back to a full relist.
const RV_MAX: usize = 128;
const REASON_MAX: usize = 160;

/// One subscription. All fields are plain data so the struct is valid when
/// zeroed (the `Default`).
#[repr(C)]
#[derive(Copy, Clone)]
pub struct SubSlot {
    in_use: bool,
    state: u8,
    ns_len: u8,
    rv_len: u8,
    reason_len: u8,
    endpoint_len: u16,
    ca_len: u16,
    id: u32,
    rpc_timeout_secs: u32,
    object_count: u32,
    live_count: u32,
    tombstone_count: u32,
    nbuckets: u32,
    buckets: pg_sys::dsa_pointer,
    /// The kind this subscription watches. Stored in full rather than as an
    /// index into a fixed table, because Phase 4 kinds are discovered at
    /// runtime; `Resource` is plain data and valid when zeroed, so the slot
    /// stays memcpy-safe.
    resource: Resource,
    last_event_us: i64,
    last_full_list_us: i64,
    state_since_us: i64,
    endpoint: [u8; ENDPOINT_MAX],
    ca_path: [u8; CA_MAX],
    namespace: [u8; NS_MAX],
    bookmark_rv: [u8; RV_MAX],
    reason: [u8; REASON_MAX],
}

/// The control segment.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct Control {
    dsa_ready: bool,
    dsa_handle: pg_sys::dsa_handle,
    tranche: i32,
    next_id: u32,
    worker_pid: i32,
    subs: [SubSlot; MAX_SUBS],
}

impl Default for Control {
    fn default() -> Self {
        // SAFETY: every field is an integer, bool, or array thereof; all-zero is valid.
        unsafe { std::mem::zeroed() }
    }
}

// SAFETY: `Control` is `repr(C)` plain data with no pointers into process memory.
unsafe impl PGRXSharedMemory for Control {}

// SAFETY: the name is unique to this extension; nothing else registers an
// LWLock called "axiom_control". pgrx 0.19 made the name explicit rather than
// deriving it, so a collision is the caller's responsibility to rule out.
static CONTROL: PgLwLock<Control> = unsafe { PgLwLock::new(c"axiom_control") };
static INITIALIZED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// This process's attachment to the DSA area (null until first use).
    static AREA: Cell<*mut pg_sys::dsa_area> = const { Cell::new(std::ptr::null_mut()) };
}

/// Errors from the shared-memory layer. Displayed to SQL users verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShmemError {
    /// Library was not preloaded; there is no shared memory.
    NotAvailable,
    /// The background worker has not created the cache area yet.
    CacheNotReady,
    /// All subscription slots are taken.
    Full,
    /// The slot was freed or reused since the caller looked it up.
    SlotGone,
    /// DSA refused an allocation (size limit reached).
    OutOfMemory,
    /// Object exceeds [`MAX_OBJECT_BYTES`] or a field exceeds its fixed width.
    TooLarge(&'static str),
}

impl fmt::Display for ShmemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAvailable => write!(
                f,
                "axiom is not in shared_preload_libraries; the watch cache is unavailable"
            ),
            Self::CacheNotReady => write!(
                f,
                "the axiom background worker has not initialised the cache yet"
            ),
            Self::Full => write!(
                f,
                "all {MAX_SUBS} axiom watch subscription slots are in use"
            ),
            Self::SlotGone => write!(f, "watch subscription disappeared"),
            Self::OutOfMemory => write!(f, "axiom cache is full (axiom.cache_size_mb)"),
            Self::TooLarge(what) => write!(f, "{what} is too large for the axiom cache"),
        }
    }
}

impl std::error::Error for ShmemError {}

/// Registers the control segment. Call from `_PG_init` only while Postgres is
/// processing `shared_preload_libraries`.
pub fn init() {
    pg_shmem_init!(CONTROL);
    INITIALIZED.store(true, Ordering::SeqCst);
}

/// Whether shared memory was set up (i.e. the library was preloaded).
pub fn available() -> bool {
    INITIALIZED.load(Ordering::SeqCst)
}

fn ensure_available() -> Result<(), ShmemError> {
    if available() {
        Ok(())
    } else {
        Err(ShmemError::NotAvailable)
    }
}

// --- fixed-width string helpers -----------------------------------------------------

fn put(buf: &mut [u8], s: &str) -> Result<usize, ShmemError> {
    let b = s.as_bytes();
    if b.len() > buf.len() {
        return Err(ShmemError::TooLarge("field"));
    }
    buf[..b.len()].copy_from_slice(b);
    Ok(b.len())
}

fn get(buf: &[u8], len: usize) -> String {
    String::from_utf8_lossy(&buf[..len.min(buf.len())]).into_owned()
}

fn now_us() -> i64 {
    // SAFETY: plain clock read.
    unsafe { pg_sys::GetCurrentTimestamp() }
}

// --- subscription table ---------------------------------------------------------------------

/// Identity of a subscription as requested by scans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubSpec {
    /// Slot index.
    pub slot: usize,
    /// Slot generation id; a slot reused for another subscription gets a new id.
    pub id: u32,
    /// Gateway.
    pub target: Target,
    /// Per-RPC deadline for this gateway.
    pub rpc_timeout: Duration,
    /// Kind watched.
    pub resource: Resource,
    /// Namespace filter ("" = all).
    pub namespace: String,
    /// Current state.
    pub state: SubState,
    /// Last resourceVersion bookmark (resume point).
    pub bookmark: String,
}

/// Snapshot of one slot for `axiom_watch_status()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubStatus {
    /// Gateway endpoint.
    pub endpoint: String,
    /// `resource` name.
    pub resource: String,
    /// Namespace filter ("" = all).
    pub namespace: String,
    /// State.
    pub state: SubState,
    /// Live (non-tombstoned) objects.
    pub objects: u32,
    /// Objects deleted but still held for the tombstone grace period.
    ///
    /// Not visible to a scan, but they occupy cache memory until `sweep`
    /// removes them, so an operator watching cache growth needs to see them.
    /// It is also the only way to observe that sweeping honours the grace
    /// period at all: a tombstone is invisible to a scan whether or not it has
    /// been swept, so without this, sweeping too early has no detectable
    /// effect.
    pub tombstones: u32,
    /// Bookmark.
    pub resource_version: String,
    /// Microseconds since 2000-01-01 of the last stream event, 0 = never.
    pub last_event_us: i64,
    /// When the current state was entered.
    pub state_since_us: i64,
    /// Human-readable reason for the current state (e.g. why degraded).
    pub reason: String,
}

fn slot_spec(i: usize, s: &SubSlot) -> SubSpec {
    let endpoint = get(&s.endpoint, s.endpoint_len as usize);
    let ca = if s.ca_len == 0 {
        None
    } else {
        Some(get(&s.ca_path, s.ca_len as usize))
    };
    // Re-derive the TLS server name from the stored endpoint; the slot keeps
    // only what a scan passed in.
    let target = Target::parse(Some(&endpoint), ca.as_deref()).unwrap_or(Target {
        endpoint: endpoint.clone(),
        tls_server_name: String::new(),
        ca_cert_path: ca,
    });
    SubSpec {
        slot: i,
        id: s.id,
        target,
        rpc_timeout: Duration::from_secs(u64::from(s.rpc_timeout_secs)),
        resource: s.resource,
        namespace: get(&s.namespace, s.ns_len as usize),
        state: SubState::from_u8(s.state),
        bookmark: get(&s.bookmark_rv, s.rv_len as usize),
    }
}

fn set_state_inner(s: &mut SubSlot, state: SubState, reason: &str, now: i64) {
    if s.state != state as u8 {
        s.state_since_us = now;
    }
    s.state = state as u8;
    let n = reason.len().min(REASON_MAX);
    s.reason[..n].copy_from_slice(&reason.as_bytes()[..n]);
    // Reason is truncated (never rejected) so a long transport error cannot block a state change.
    #[allow(clippy::cast_possible_truncation, reason = "n <= REASON_MAX = 160")]
    let n8 = n as u8;
    s.reason_len = n8;
}

/// Finds the subscription serving `(target, kind, namespace)` or requests a new
/// one. A cluster-wide subscription (empty namespace) also serves any namespace.
///
/// Returns the slot, its generation id, and its current state. The state is
/// `Requested` for a freshly created slot; the caller falls through to an RPC.
pub fn lookup_or_request(
    target: &Target,
    rpc_timeout: Duration,
    resource: &Resource,
    namespace: &str,
) -> Result<(usize, u32, SubState), ShmemError> {
    ensure_available()?;
    let mut ctl = CONTROL.exclusive();
    let mut exact = None;
    let mut wide = None;
    let want_ca = target.ca_cert_path.as_deref().unwrap_or("");
    for (i, s) in ctl.subs.iter().enumerate() {
        // Identity is (endpoint, CA, kind): two servers with the same endpoint
        // but different trust roots must never share a cache, and two versions
        // of one kind are separate subscriptions.
        if !s.in_use
            || s.resource != *resource
            || get(&s.endpoint, s.endpoint_len as usize) != target.endpoint
            || get(&s.ca_path, s.ca_len as usize) != want_ca
        {
            continue;
        }
        let ns = get(&s.namespace, s.ns_len as usize);
        if ns == namespace {
            exact = Some(i);
            break;
        }
        if ns.is_empty() {
            wide = Some(i);
        }
    }
    if let Some(i) = exact.or(wide) {
        let s = &ctl.subs[i];
        return Ok((i, s.id, SubState::from_u8(s.state)));
    }
    let Some(i) = ctl.subs.iter().position(|s| !s.in_use) else {
        return Err(ShmemError::Full);
    };
    // Build the slot privately; publish only after every fallible copy succeeded,
    // so a too-long endpoint/CA path can never leave a half-initialised slot.
    let mut fresh = SubSlot::default_zeroed();
    fresh.resource = *resource;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "put() bounds the length by the buffer size (<= 512)"
    )]
    {
        fresh.endpoint_len = put(&mut fresh.endpoint, &target.endpoint)? as u16;
        fresh.ca_len = put(&mut fresh.ca_path, want_ca)? as u16;
        fresh.ns_len = put(&mut fresh.namespace, namespace)? as u8;
    }
    fresh.rpc_timeout_secs = u32::try_from(rpc_timeout.as_secs()).unwrap_or(u32::MAX);
    ctl.next_id = ctl.next_id.wrapping_add(1).max(1);
    let id = ctl.next_id;
    fresh.id = id;
    fresh.in_use = true;
    set_state_inner(
        &mut fresh,
        SubState::Requested,
        "requested by a scan",
        now_us(),
    );
    ctl.subs[i] = fresh;
    Ok((i, id, SubState::Requested))
}

impl SubSlot {
    fn default_zeroed() -> Self {
        // SAFETY: plain data, all-zero is valid.
        unsafe { std::mem::zeroed() }
    }
}

/// All in-use subscriptions (worker side).
pub fn subscriptions() -> Result<Vec<SubSpec>, ShmemError> {
    ensure_available()?;
    let ctl = CONTROL.share();
    Ok(ctl
        .subs
        .iter()
        .enumerate()
        .filter(|(_, s)| s.in_use)
        .map(|(i, s)| slot_spec(i, s))
        .collect())
}

/// Status snapshot for SQL.
pub fn status() -> Result<Vec<SubStatus>, ShmemError> {
    ensure_available()?;
    let ctl = CONTROL.share();
    Ok(ctl
        .subs
        .iter()
        .filter(|s| s.in_use)
        .map(|s| SubStatus {
            endpoint: get(&s.endpoint, s.endpoint_len as usize),
            resource: s.resource.to_string(),
            namespace: get(&s.namespace, s.ns_len as usize),
            state: SubState::from_u8(s.state),
            objects: s.live_count,
            tombstones: s.tombstone_count,
            resource_version: get(&s.bookmark_rv, s.rv_len as usize),
            last_event_us: s.last_event_us,
            state_since_us: s.state_since_us,
            reason: get(&s.reason, s.reason_len as usize),
        })
        .collect())
}

/// Sets a slot's state and reason if the slot still has generation `id`.
pub fn set_state(slot: usize, id: u32, state: SubState, reason: &str) -> Result<(), ShmemError> {
    ensure_available()?;
    let mut ctl = CONTROL.exclusive();
    let now = now_us();
    let s = ctl
        .subs
        .get_mut(slot)
        .filter(|s| s.in_use && s.id == id)
        .ok_or(ShmemError::SlotGone)?;
    set_state_inner(s, state, reason, now);
    Ok(())
}

/// Records a resourceVersion bookmark (and, for a full sync, the sync time).
///
/// Returns `TooLarge("resourceVersion")` without changing the stored bookmark
/// if the token exceeds the slot's capacity; callers must then treat the
/// subscription as having no resume point (full relist), never truncate.
pub fn set_bookmark(slot: usize, id: u32, rv: &str, full_sync: bool) -> Result<(), ShmemError> {
    ensure_available()?;
    if rv.len() > RV_MAX {
        return Err(ShmemError::TooLarge("resourceVersion"));
    }
    let mut ctl = CONTROL.exclusive();
    let now = now_us();
    let s = ctl
        .subs
        .get_mut(slot)
        .filter(|s| s.in_use && s.id == id)
        .ok_or(ShmemError::SlotGone)?;
    s.bookmark_rv[..rv.len()].copy_from_slice(rv.as_bytes());
    #[allow(clippy::cast_possible_truncation, reason = "rv.len() <= RV_MAX = 128")]
    let n8 = rv.len() as u8;
    s.rv_len = n8;
    s.last_event_us = now;
    if full_sync {
        s.last_full_list_us = now;
    }
    Ok(())
}

/// Forgets the bookmark (e.g. when a token could not be stored) so a worker
/// restart relists instead of resuming from a stale or missing point.
pub fn clear_bookmark(slot: usize, id: u32) -> Result<(), ShmemError> {
    ensure_available()?;
    let mut ctl = CONTROL.exclusive();
    let s = ctl
        .subs
        .get_mut(slot)
        .filter(|s| s.in_use && s.id == id)
        .ok_or(ShmemError::SlotGone)?;
    s.rv_len = 0;
    Ok(())
}

// --- DSA area -----------------------------------------------------------------------------------

const TRANCHE_NAME: &std::ffi::CStr = c"axiom_cache";

/// Creates (or re-attaches to) the DSA area. Worker only; call once at start.
/// Marks pre-existing subscriptions as needing a stream (the worker that owned
/// them is gone).
pub fn worker_init(cache_limit_bytes: usize) -> Result<(), ShmemError> {
    ensure_available()?;
    let mut ctl = CONTROL.exclusive();
    // SAFETY: standard LWLock tranche and DSA lifecycle calls, made from a
    // process attached to shared memory, in TopMemoryContext so the mapping
    // outlives any transaction.
    unsafe {
        if ctl.tranche == 0 {
            ctl.tranche = pg_sys::LWLockNewTrancheId();
        }
        pg_sys::LWLockRegisterTranche(ctl.tranche, TRANCHE_NAME.as_ptr());
        let old = pg_sys::MemoryContextSwitchTo(pg_sys::TopMemoryContext);
        let area = if ctl.dsa_ready {
            pg_sys::dsa_attach(ctl.dsa_handle)
        } else {
            // pg16 has dsa_create; pg17 replaced it with dsa_create_ext.
            // Gated on pg16 rather than on pg17 so every later major takes the
            // new arm by default: naming pg17 explicitly would have put pg18
            // on the pg16 path and failed to compile.
            #[cfg(feature = "pg16")]
            let a = pg_sys::dsa_create(ctl.tranche);
            #[cfg(not(feature = "pg16"))]
            let a =
                pg_sys::dsa_create_ext(ctl.tranche, 1 << 20, 1usize << pg_sys::DSA_OFFSET_WIDTH);
            pg_sys::dsa_pin(a);
            ctl.dsa_handle = pg_sys::dsa_get_handle(a);
            ctl.dsa_ready = true;
            a
        };
        pg_sys::dsa_pin_mapping(area);
        pg_sys::dsa_set_size_limit(area, cache_limit_bytes);
        pg_sys::MemoryContextSwitchTo(old);
        AREA.with(|c| c.set(area));
        ctl.worker_pid = pg_sys::MyProcPid;
    }
    let now = now_us();
    for s in ctl.subs.iter_mut().filter(|s| s.in_use) {
        let next = match SubState::from_u8(s.state) {
            SubState::Active | SubState::Degraded => SubState::Degraded,
            _ => SubState::Requested,
        };
        set_state_inner(s, next, "background worker restarted", now);
    }
    Ok(())
}

/// This process's DSA attachment, attaching on first use (backend side).
/// Must be called while holding `CONTROL` (any mode) so the handle is stable.
unsafe fn area_for(ctl: &Control) -> Result<*mut pg_sys::dsa_area, ShmemError> {
    let cur = AREA.with(Cell::get);
    if !cur.is_null() {
        return Ok(cur);
    }
    if !ctl.dsa_ready {
        return Err(ShmemError::CacheNotReady);
    }
    // SAFETY: attach in TopMemoryContext so the mapping persists for the backend's life.
    unsafe {
        pg_sys::LWLockRegisterTranche(ctl.tranche, TRANCHE_NAME.as_ptr());
        let old = pg_sys::MemoryContextSwitchTo(pg_sys::TopMemoryContext);
        let area = pg_sys::dsa_attach(ctl.dsa_handle);
        pg_sys::dsa_pin_mapping(area);
        pg_sys::MemoryContextSwitchTo(old);
        AREA.with(|c| c.set(area));
        Ok(area)
    }
}

// --- object index -------------------------------------------------------------------------------

/// Header of one cached object; followed by namespace, name, rv, json bytes.
#[repr(C)]
#[derive(Copy, Clone)]
struct EntryHdr {
    next: pg_sys::dsa_pointer,
    hash: u64,
    deleted_at_us: i64,
    json_len: u32,
    ns_len: u16,
    name_len: u16,
    rv_len: u16,
    tombstone: u8,
    _pad: u8,
}

const HDR: usize = std::mem::size_of::<EntryHdr>();

/// Borrowed view of an entry's variable-length parts.
struct EntryView<'a> {
    hdr: EntryHdr,
    ns: &'a [u8],
    name: &'a [u8],
    #[allow(
        dead_code,
        reason = "per-object resourceVersion is stored for Phase 8 diagnostics"
    )]
    rv: &'a [u8],
    json: &'a [u8],
}

unsafe fn view<'a>(area: *mut pg_sys::dsa_area, ptr: pg_sys::dsa_pointer) -> EntryView<'a> {
    // SAFETY: ptr was produced by `alloc_entry` in this area; lengths in the
    // header describe exactly the bytes written after it.
    unsafe {
        let base = pg_sys::dsa_get_address(area, ptr).cast::<u8>();
        let hdr = std::ptr::read_unaligned(base.cast::<EntryHdr>());
        let mut off = HDR;
        let take = |off: &mut usize, len: usize| -> &'a [u8] {
            let s = std::slice::from_raw_parts(base.add(*off), len);
            *off += len;
            s
        };
        let ns = take(&mut off, hdr.ns_len as usize);
        let name = take(&mut off, hdr.name_len as usize);
        let rv = take(&mut off, hdr.rv_len as usize);
        let json = take(&mut off, hdr.json_len as usize);
        EntryView {
            hdr,
            ns,
            name,
            rv,
            json,
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "private constructor mirroring the on-disk entry layout"
)]
unsafe fn alloc_entry(
    area: *mut pg_sys::dsa_area,
    next: pg_sys::dsa_pointer,
    hash: u64,
    ns: &str,
    name: &str,
    rv: &str,
    json: &[u8],
    tombstone: bool,
    deleted_at_us: i64,
) -> Result<pg_sys::dsa_pointer, ShmemError> {
    if json.len() > MAX_OBJECT_BYTES {
        return Err(ShmemError::TooLarge("object"));
    }
    if ns.len() > usize::from(u16::MAX)
        || name.len() > usize::from(u16::MAX)
        || rv.len() > usize::from(u16::MAX)
    {
        return Err(ShmemError::TooLarge("object key"));
    }
    let total = HDR + ns.len() + name.len() + rv.len() + json.len();
    // SAFETY: NO_OOM makes allocation failure a null pointer, never an ERROR.
    unsafe {
        #[allow(clippy::cast_possible_wrap, reason = "flags are small constants")]
        let flags = (pg_sys::DSA_ALLOC_NO_OOM | pg_sys::DSA_ALLOC_ZERO) as i32;
        let ptr = pg_sys::dsa_allocate_extended(area, total, flags);
        if ptr == 0 {
            return Err(ShmemError::OutOfMemory);
        }
        let base = pg_sys::dsa_get_address(area, ptr).cast::<u8>();
        #[allow(
            clippy::cast_possible_truncation,
            reason = "lengths were range-checked above"
        )]
        let hdr = EntryHdr {
            next,
            hash,
            deleted_at_us,
            json_len: json.len() as u32,
            ns_len: ns.len() as u16,
            name_len: name.len() as u16,
            rv_len: rv.len() as u16,
            tombstone: u8::from(tombstone),
            _pad: 0,
        };
        std::ptr::write_unaligned(base.cast::<EntryHdr>(), hdr);
        let mut off = HDR;
        for part in [ns.as_bytes(), name.as_bytes(), rv.as_bytes(), json] {
            std::ptr::copy_nonoverlapping(part.as_ptr(), base.add(off), part.len());
            off += part.len();
        }
        Ok(ptr)
    }
}

unsafe fn bucket_slice<'a>(
    area: *mut pg_sys::dsa_area,
    s: &SubSlot,
) -> &'a mut [pg_sys::dsa_pointer] {
    // SAFETY: `buckets` was allocated with exactly `nbuckets` pointers.
    unsafe {
        if s.buckets == 0 || s.nbuckets == 0 {
            return &mut [];
        }
        std::slice::from_raw_parts_mut(
            pg_sys::dsa_get_address(area, s.buckets).cast::<pg_sys::dsa_pointer>(),
            s.nbuckets as usize,
        )
    }
}

/// Ensures the bucket array exists and is large enough, rehashing if needed.
unsafe fn ensure_buckets(area: *mut pg_sys::dsa_area, s: &mut SubSlot) -> Result<(), ShmemError> {
    let want = buckets_for(
        s.nbuckets,
        s.live_count
            .saturating_add(s.tombstone_count)
            .saturating_add(1),
    );
    if s.buckets != 0 && want == s.nbuckets {
        return Ok(());
    }
    // SAFETY: allocate the new array, relink every entry, free the old array.
    unsafe {
        #[allow(clippy::cast_possible_wrap, reason = "flags are small constants")]
        let flags = (pg_sys::DSA_ALLOC_NO_OOM | pg_sys::DSA_ALLOC_ZERO) as i32;
        let new_ptr = pg_sys::dsa_allocate_extended(
            area,
            want as usize * std::mem::size_of::<pg_sys::dsa_pointer>(),
            flags,
        );
        if new_ptr == 0 {
            return Err(ShmemError::OutOfMemory);
        }
        let old_buckets: Vec<pg_sys::dsa_pointer> = bucket_slice(area, s).to_vec();
        let new_slice = std::slice::from_raw_parts_mut(
            pg_sys::dsa_get_address(area, new_ptr).cast::<pg_sys::dsa_pointer>(),
            want as usize,
        );
        for head in old_buckets {
            let mut p = head;
            while p != 0 {
                let base = pg_sys::dsa_get_address(area, p).cast::<EntryHdr>();
                let mut hdr = std::ptr::read_unaligned(base);
                let next = hdr.next;
                #[allow(clippy::cast_possible_truncation, reason = "modulo nbuckets fits u32")]
                let b = (hdr.hash % u64::from(want)) as usize;
                hdr.next = new_slice[b];
                std::ptr::write_unaligned(base, hdr);
                new_slice[b] = p;
                p = next;
            }
        }
        if s.buckets != 0 {
            pg_sys::dsa_free(area, s.buckets);
        }
        s.buckets = new_ptr;
        s.nbuckets = want;
        Ok(())
    }
}

/// Finds the entry for (ns, name); returns (prev pointer or 0, entry pointer).
unsafe fn find(
    area: *mut pg_sys::dsa_area,
    s: &SubSlot,
    hash: u64,
    ns: &str,
    name: &str,
) -> Option<(pg_sys::dsa_pointer, pg_sys::dsa_pointer)> {
    // SAFETY: walks a chain built by `alloc_entry`/`ensure_buckets`.
    unsafe {
        let buckets = bucket_slice(area, s);
        if buckets.is_empty() {
            return None;
        }
        #[allow(clippy::cast_possible_truncation, reason = "modulo nbuckets fits u32")]
        let b = (hash % u64::from(s.nbuckets)) as usize;
        let mut prev = 0;
        let mut p = buckets[b];
        while p != 0 {
            let v = view(area, p);
            if v.hdr.hash == hash && v.ns == ns.as_bytes() && v.name == name.as_bytes() {
                return Some((prev, p));
            }
            prev = p;
            p = v.hdr.next;
        }
        None
    }
}

/// Unlinks and frees entry `p` whose predecessor in its chain is `prev` (0 = head).
unsafe fn unlink(
    area: *mut pg_sys::dsa_area,
    s: &mut SubSlot,
    prev: pg_sys::dsa_pointer,
    p: pg_sys::dsa_pointer,
) {
    // SAFETY: pointers come from `find` on this slot's chains.
    unsafe {
        let hdr = std::ptr::read_unaligned(pg_sys::dsa_get_address(area, p).cast::<EntryHdr>());
        if prev == 0 {
            #[allow(clippy::cast_possible_truncation, reason = "modulo nbuckets fits u32")]
            let b = (hdr.hash % u64::from(s.nbuckets)) as usize;
            bucket_slice(area, s)[b] = hdr.next;
        } else {
            let pb = pg_sys::dsa_get_address(area, prev).cast::<EntryHdr>();
            let mut ph = std::ptr::read_unaligned(pb);
            ph.next = hdr.next;
            std::ptr::write_unaligned(pb, ph);
        }
        if hdr.tombstone != 0 {
            s.tombstone_count = s.tombstone_count.saturating_sub(1);
        } else {
            s.live_count = s.live_count.saturating_sub(1);
        }
        pg_sys::dsa_free(area, p);
    }
}

fn checked_slot(ctl: &mut Control, slot: usize, id: u32) -> Result<&mut SubSlot, ShmemError> {
    ctl.subs
        .get_mut(slot)
        .filter(|s| s.in_use && s.id == id)
        .ok_or(ShmemError::SlotGone)
}

/// Inserts or replaces one object (worker side).
///
/// Allocation happens before the old entry is touched: if the cache is full,
/// the previous version of the object stays in place and the error propagates
/// (the worker degrades the subscription), so a stale cache never loses
/// objects it already had.
pub fn upsert(
    slot: usize,
    id: u32,
    ns: &str,
    name: &str,
    rv: &str,
    json: &[u8],
) -> Result<(), ShmemError> {
    ensure_available()?;
    let mut ctl = CONTROL.exclusive();
    // SAFETY: worker holds the exclusive lock for the whole mutation.
    unsafe {
        let area = area_for(&ctl)?;
        let now = now_us();
        let s = checked_slot(&mut ctl, slot, id)?;
        let hash = key_hash(ns, name);
        ensure_buckets(area, s)?;
        let new_entry = alloc_entry(area, 0, hash, ns, name, rv, json, false, 0)?;
        if let Some((prev, old)) = find(area, s, hash, ns, name) {
            unlink(area, s, prev, old);
        }
        #[allow(clippy::cast_possible_truncation, reason = "modulo nbuckets fits u32")]
        let b = (hash % u64::from(s.nbuckets)) as usize;
        let head = bucket_slice(area, s)[b];
        let base = pg_sys::dsa_get_address(area, new_entry).cast::<EntryHdr>();
        let mut hdr = std::ptr::read_unaligned(base);
        hdr.next = head;
        std::ptr::write_unaligned(base, hdr);
        bucket_slice(area, s)[b] = new_entry;
        s.live_count = s.live_count.saturating_add(1);
        s.object_count = s.live_count;
        s.last_event_us = now;
        Ok(())
    }
}

/// Marks one object deleted; it stays visible as a tombstone for the grace
/// period and is removed by [`sweep`]. Unknown keys are ignored.
pub fn tombstone(slot: usize, id: u32, ns: &str, name: &str) -> Result<(), ShmemError> {
    ensure_available()?;
    let mut ctl = CONTROL.exclusive();
    // SAFETY: as for `upsert`.
    unsafe {
        let area = area_for(&ctl)?;
        let now = now_us();
        let s = checked_slot(&mut ctl, slot, id)?;
        let hash = key_hash(ns, name);
        if let Some((_, p)) = find(area, s, hash, ns, name) {
            let base = pg_sys::dsa_get_address(area, p).cast::<EntryHdr>();
            let mut hdr = std::ptr::read_unaligned(base);
            if hdr.tombstone == 0 {
                hdr.tombstone = 1;
                hdr.deleted_at_us = now;
                std::ptr::write_unaligned(base, hdr);
                s.live_count = s.live_count.saturating_sub(1);
                s.tombstone_count = s.tombstone_count.saturating_add(1);
            }
        }
        s.object_count = s.live_count;
        s.last_event_us = now;
        Ok(())
    }
}

/// Frees every object of a subscription (before a full relist).
pub fn clear(slot: usize, id: u32) -> Result<(), ShmemError> {
    ensure_available()?;
    let mut ctl = CONTROL.exclusive();
    // SAFETY: frees exactly the pointers this slot owns.
    unsafe {
        let area = area_for(&ctl)?;
        let s = checked_slot(&mut ctl, slot, id)?;
        for head in bucket_slice(area, s).to_vec() {
            let mut p = head;
            while p != 0 {
                let next =
                    std::ptr::read_unaligned(pg_sys::dsa_get_address(area, p).cast::<EntryHdr>())
                        .next;
                pg_sys::dsa_free(area, p);
                p = next;
            }
        }
        if s.buckets != 0 {
            pg_sys::dsa_free(area, s.buckets);
        }
        s.buckets = 0;
        s.nbuckets = 0;
        s.live_count = 0;
        s.tombstone_count = 0;
        s.object_count = 0;
        Ok(())
    }
}

/// Removes expired tombstones from every subscription. Returns how many.
pub fn sweep() -> Result<usize, ShmemError> {
    ensure_available()?;
    let mut ctl = CONTROL.exclusive();
    let now = now_us();
    let mut removed = 0;
    // SAFETY: walks and edits chains under the exclusive lock.
    unsafe {
        let area = match area_for(&ctl) {
            Ok(a) => a,
            Err(ShmemError::CacheNotReady) => return Ok(0),
            Err(e) => return Err(e),
        };
        for s in ctl
            .subs
            .iter_mut()
            .filter(|s| s.in_use && s.tombstone_count > 0)
        {
            for b in 0..s.nbuckets as usize {
                let mut prev = 0;
                let mut p = bucket_slice(area, s)[b];
                while p != 0 {
                    let hdr = std::ptr::read_unaligned(
                        pg_sys::dsa_get_address(area, p).cast::<EntryHdr>(),
                    );
                    if hdr.tombstone != 0 && tombstone_expired(hdr.deleted_at_us, now) {
                        unlink(area, s, prev, p);
                        removed += 1;
                    } else {
                        prev = p;
                    }
                    p = hdr.next;
                }
            }
        }
    }
    Ok(removed)
}

/// Copies out the JSON of every live object matching the filters (backend
/// side). Returns `SlotGone` if the slot was reused.
pub fn scan(
    slot: usize,
    id: u32,
    ns_filter: &str,
    name_filter: &str,
) -> Result<Vec<Vec<u8>>, ShmemError> {
    ensure_available()?;
    let ctl = CONTROL.share();
    let s = ctl
        .subs
        .get(slot)
        .filter(|s| s.in_use && s.id == id)
        .ok_or(ShmemError::SlotGone)?;
    // SAFETY: read-only walk under the shared lock; everything is copied out before release.
    unsafe {
        let area = area_for(&ctl)?;
        let mut out = Vec::with_capacity(s.live_count as usize);
        for b in 0..s.nbuckets as usize {
            let mut p = bucket_slice(area, s)[b];
            while p != 0 {
                let v = view(area, p);
                if v.hdr.tombstone == 0 {
                    let ns = std::str::from_utf8(v.ns).unwrap_or("");
                    let name = std::str::from_utf8(v.name).unwrap_or("");
                    if key_matches(ns_filter, name_filter, ns, name) {
                        out.push(v.json.to_vec());
                    }
                }
                p = v.hdr.next;
            }
        }
        Ok(out)
    }
}
