//! The Axiom background worker: impure glue between Postgres (GUCs, signals,
//! SPI for NOTIFY, elog) and the pure modules. One process, one
//! `current_thread` tokio runtime, driving:
//!
//! - the gateway **ping** loop (Phase 0), a liveness probe of `axiom.gateway_endpoint`;
//! - the **subscription manager** (Phase 3): one `Subscribe` stream task per
//!   slot in the shared subscription table, each writing into the shared-memory
//!   cache, with reconnect/backoff and resume-from-bookmark;
//! - the **tombstone sweeper**, and `NOTIFY axiom_events` on cache changes.
//!
//! Lifecycle: one static worker registered from `_PG_init` under
//! `shared_preload_libraries`. It runs until SIGTERM. Nothing here panics on a
//! failed RPC, a lost stream, or a full cache: failures are recorded in the
//! subscription's state (`DEGRADED` + reason) and retried with backoff.

use std::collections::HashMap;
use std::ffi::CString;
use std::rc::Rc;
use std::time::{Duration, Instant};

use pgrx::bgworkers::{
    BackgroundWorker, BackgroundWorkerBuilder, BgWorkerStartTime, SignalWakeFlags,
};
use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use pgrx::prelude::*;
use tonic::transport::Channel;

use crate::backoff::Backoff;
use crate::cache::{next_state, StreamEvent, SubState};
use crate::config::Settings;
use crate::ping::PingOutcome;
use crate::proto::v1::gateway_service_client::GatewayServiceClient;
use crate::proto::v1::subscribe_response::Type as EvType;
use crate::proto::v1::{PingRequest, SubscribeRequest, SubscribeResponse};
use crate::shmem::{self, ShmemError, SubSpec};
use crate::transport::{self, build_channel, build_channel_with, ChannelError, Keepalive};

/// Human-readable worker name; also its `backend_type` in `pg_stat_activity`.
pub const WORKER_NAME: &str = "axiom gateway pinger";

/// `LISTEN` channel on which cache changes are announced.
pub const NOTIFY_CHANNEL: &str = "axiom_events";

static GATEWAY_ENDPOINT: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
static GATEWAY_CA_CERT: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
static PING_INTERVAL_SECS: GucSetting<i32> = GucSetting::<i32>::new(10);
static RPC_TIMEOUT_SECS: GucSetting<i32> = GucSetting::<i32>::new(5);
static NOTIFY_DATABASE: GucSetting<Option<CString>> =
    GucSetting::<Option<CString>>::new(Some(c"postgres"));
static CACHE_SIZE_MB: GucSetting<i32> = GucSetting::<i32>::new(256);

const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_mins(1);
const TICK: Duration = Duration::from_millis(250);
const SWEEP_EVERY: Duration = Duration::from_secs(1);

/// Defines the `axiom.*` GUCs. Must be called from `_PG_init`.
///
/// Gateway/ping GUCs are `SIGHUP` context (re-read every tick after a reload).
/// `notify_database` and `cache_size_mb` take effect when the worker starts.
pub fn define_gucs() {
    GucRegistry::define_string_guc(
        c"axiom.gateway_endpoint",
        c"URL of the Axiom gateway to ping, e.g. https://gateway:8443",
        c"Must use https. Credentials are never embedded here.",
        &GATEWAY_ENDPOINT,
        GucContext::Sighup,
        GucFlags::SUPERUSER_ONLY,
    );
    GucRegistry::define_string_guc(
        c"axiom.gateway_ca_cert",
        c"Path to a PEM CA bundle used to verify the pinged gateway's TLS certificate",
        c"Leave unset to use the Mozilla webpki root store.",
        &GATEWAY_CA_CERT,
        GucContext::Sighup,
        GucFlags::SUPERUSER_ONLY,
    );
    GucRegistry::define_int_guc(
        c"axiom.ping_interval_secs",
        c"Seconds between gateway Ping round-trips",
        c"Must be greater than axiom.rpc_timeout_secs.",
        &PING_INTERVAL_SECS,
        1,
        3600,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"axiom.rpc_timeout_secs",
        c"Per-RPC deadline in seconds for the ping",
        c"Must be smaller than axiom.ping_interval_secs.",
        &RPC_TIMEOUT_SECS,
        1,
        3600,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_string_guc(
        c"axiom.notify_database",
        c"Database the background worker connects to for NOTIFY axiom_events",
        c"LISTEN axiom_events in this database to receive cache change notifications.",
        &NOTIFY_DATABASE,
        GucContext::Postmaster,
        GucFlags::SUPERUSER_ONLY,
    );
    GucRegistry::define_int_guc(
        c"axiom.cache_size_mb",
        c"Upper bound of the shared-memory watch cache, in MiB",
        c"When reached, affected subscriptions become DEGRADED rather than evicting.",
        &CACHE_SIZE_MB,
        16,
        1_048_576,
        GucContext::Postmaster,
        GucFlags::default(),
    );
}

/// Registers the static background worker. Only valid while Postgres is
/// processing `shared_preload_libraries`; the caller checks that.
pub fn register() {
    BackgroundWorkerBuilder::new(WORKER_NAME)
        .set_function("axiom_bgworker_main")
        .set_library("axiom")
        // Database access (for NOTIFY) is only permitted for workers starting
        // at or after ConsistentState; it also implies shared-memory access,
        // which the process latch and the cache require.
        .set_start_time(BgWorkerStartTime::RecoveryFinished)
        .enable_shmem_access(None)
        .enable_spi_access()
        .set_restart_time(Some(Duration::from_secs(5)))
        .load();
}

/// Reads the current ping GUC values and validates them.
fn read_settings() -> Result<Settings, crate::config::ConfigError> {
    let endpoint = GATEWAY_ENDPOINT
        .get()
        .map(|c| c.to_string_lossy().into_owned());
    let ca = GATEWAY_CA_CERT
        .get()
        .map(|c| c.to_string_lossy().into_owned());
    Settings::from_raw(
        endpoint.as_deref(),
        ca.as_deref(),
        i64::from(PING_INTERVAL_SECS.get()),
        i64::from(RPC_TIMEOUT_SECS.get()),
    )
}

// --- ping loop -------------------------------------------------------------------------

/// Connection state carried across ticks so the channel is reused while the
/// settings are unchanged.
struct Conn {
    settings: Settings,
    client: GatewayServiceClient<Channel>,
}

struct Pinger {
    backoff: Backoff,
    conn: Option<Conn>,
    nonce: u64,
    next_due: Instant,
}

impl Pinger {
    fn new() -> Self {
        Self {
            backoff: Backoff::new(BACKOFF_BASE, BACKOFF_MAX),
            conn: None,
            nonce: 0,
            next_due: Instant::now(),
        }
    }

    /// Runs one ping if due; schedules the next according to outcome.
    async fn tick(&mut self, rt: &tokio::runtime::Handle) {
        if Instant::now() < self.next_due {
            return;
        }
        let settings = match read_settings() {
            Ok(s) => s,
            Err(e) => {
                self.conn = None;
                warning!("{WORKER_NAME}: invalid configuration, not pinging: {e}");
                self.next_due = Instant::now() + self.backoff.on_failure();
                return;
            }
        };
        if self.conn.as_ref().is_none_or(|c| c.settings != settings) {
            let built = {
                let _guard = rt.enter();
                build_channel(&settings.target, settings.rpc_timeout)
            };
            match built {
                Ok(ch) => {
                    log!(
                        "{WORKER_NAME}: gateway endpoint {}",
                        settings.target.endpoint
                    );
                    self.conn = Some(Conn {
                        client: GatewayServiceClient::new(ch)
                            .max_decoding_message_size(transport::MAX_MESSAGE_BYTES)
                            .max_encoding_message_size(transport::MAX_MESSAGE_BYTES),
                        settings,
                    });
                }
                Err(e) => {
                    self.conn = None;
                    warning!("{WORKER_NAME}: {}", ChannelError::to_string(&e));
                    self.next_due = Instant::now() + self.backoff.on_failure();
                    return;
                }
            }
        }
        let Some(c) = self.conn.as_mut() else {
            self.next_due = Instant::now() + self.backoff.on_failure();
            return;
        };
        self.nonce = self.nonce.wrapping_add(1).max(1);
        let nonce = self.nonce;
        let timeout = c.settings.rpc_timeout;
        let result = match tokio::time::timeout(timeout, c.client.ping(PingRequest { nonce })).await
        {
            Ok(Ok(resp)) => Ok(resp.into_inner()),
            Ok(Err(status)) => Err(status),
            Err(_elapsed) => Err(tonic::Status::deadline_exceeded(format!(
                "no reply within {}s",
                timeout.as_secs()
            ))),
        };
        let outcome = PingOutcome::classify(nonce, result);
        let line = outcome.log_line(&c.settings.target.endpoint);
        if outcome.is_success() {
            self.backoff.on_success();
            log!("{line}");
            self.next_due = Instant::now() + c.settings.ping_interval;
        } else {
            let wait = self.backoff.on_failure();
            warning!(
                "{line} (retry in {}s, consecutive_failures={})",
                wait.as_secs(),
                self.backoff.consecutive_failures()
            );
            self.next_due = Instant::now() + wait;
        }
    }
}

// --- subscription streams -----------------------------------------------------------------

/// Sends one `NOTIFY axiom_events` with a JSON payload describing a change.
///
/// Callers must only invoke this for an actual change, never for an object
/// arriving in a subscription's initial listing. Each call opens its own
/// transaction (`BackgroundWorker::transaction`) and this worker runs a
/// current-thread runtime, so the SPI call blocks the single thread driving
/// every watch stream for its duration. One per object of an initial listing
/// is therefore thousands of sequential transactions during which no other
/// stream makes progress. See the guard in `apply_event`.
fn notify(spec: &SubSpec, ty: &str, namespace: &str, name: &str) {
    let payload = serde_json::json!({
        "server": spec.target.endpoint,
        "resource": spec.resource.to_string(),
        "namespace": namespace,
        "name": name,
        "type": ty,
    })
    .to_string();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        BackgroundWorker::transaction(|| {
            Spi::run_with_args(
                "SELECT pg_notify($1, $2)",
                &[NOTIFY_CHANNEL.into(), payload.as_str().into()],
            )
        })
    }));
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warning!("{WORKER_NAME}: NOTIFY failed: {e}"),
        Err(_) => warning!("{WORKER_NAME}: NOTIFY failed (transaction error)"),
    }
}

/// Applies one stream event to the cache. Updates `resume` only once the
/// stream has proven itself current (`synced`): resource versions seen during
/// a partial initial listing are never resume points. A bookmark that does not
/// fit the slot clears the stored one so a restart relists rather than resumes
/// from a corrupted token.
fn apply_event(
    spec: &SubSpec,
    ev: SubscribeResponse,
    resume: &mut Option<String>,
    synced: &mut bool,
) -> Result<EventAction, ShmemError> {
    let ty = EvType::try_from(ev.r#type).unwrap_or(EvType::Unspecified);
    let record_bookmark = |rv: &str,
                           full: bool,
                           resume: &mut Option<String>|
     -> Result<(), ShmemError> {
        match shmem::set_bookmark(spec.slot, spec.id, rv, full) {
            Ok(()) => {
                *resume = Some(rv.to_owned());
                Ok(())
            }
            Err(ShmemError::TooLarge(_)) => {
                warning!("{WORKER_NAME}: resourceVersion {rv:?} too long to store; this watch will relist after a restart");
                *resume = None;
                shmem::clear_bookmark(spec.slot, spec.id)
            }
            Err(e) => Err(e),
        }
    };
    match ty {
        EvType::Added | EvType::Modified | EvType::Deleted => {
            let Some(obj) = ev.object else {
                return Ok(EventAction::Continue);
            };
            if ty == EvType::Deleted {
                shmem::tombstone(spec.slot, spec.id, &obj.namespace, &obj.name)?;
            } else {
                shmem::upsert(
                    spec.slot,
                    spec.id,
                    &obj.namespace,
                    &obj.name,
                    &obj.resource_version,
                    &obj.json,
                )?;
            }
            if *synced && !ev.resource_version.is_empty() {
                record_bookmark(&ev.resource_version, false, resume)?;
            }
            // Only notify for a change, never for the initial listing.
            //
            // A fresh subscription replays the whole collection as ADDED
            // before SYNCED, and those are not changes: they are the cache
            // being populated. Announcing them is both wrong for the consumer
            // (nothing happened in the cluster) and the worker's worst
            // bottleneck, because every notification is its own transaction on
            // the one thread that drives every stream.
            //
            // `synced` starts true on a resumed stream (`run_stream` sets it
            // from a non-empty resume token), so changes that happened during
            // an outage and arrive on resume still notify. That is the
            // behaviour worth keeping: they are real changes the consumer
            // missed.
            if *synced {
                notify(
                    spec,
                    ty.as_str_name().trim_start_matches("TYPE_"),
                    &obj.namespace,
                    &obj.name,
                );
            }
            Ok(EventAction::Continue)
        }
        EvType::Synced => {
            *synced = true;
            record_bookmark(&ev.resource_version, true, resume)?;
            shmem::set_state(spec.slot, spec.id, SubState::Active, "watch active")?;
            Ok(EventAction::Synced)
        }
        EvType::Bookmark => {
            *synced = true;
            record_bookmark(&ev.resource_version, false, resume)?;
            // The API server only bookmarks a caught-up watcher: a resumed
            // stream is current again.
            shmem::set_state(spec.slot, spec.id, SubState::Active, "watch active")?;
            Ok(EventAction::Synced)
        }
        EvType::ResyncRequired => {
            *resume = None;
            Ok(EventAction::Relist)
        }
        EvType::Unspecified => Ok(EventAction::Continue),
    }
}

enum EventAction {
    Continue,
    Synced,
    Relist,
}

/// Records a state change, logging it; `SlotGone` means the subscription was
/// dropped and the task should end. `has_bookmark` says whether a resume point
/// exists, which decides whether a loss is `Degraded` (servable) or `Requested`.
fn transition(
    spec: &SubSpec,
    current: &mut SubState,
    ev: StreamEvent,
    has_bookmark: bool,
    reason: &str,
) -> Result<(), ShmemError> {
    let next = next_state(*current, ev, has_bookmark);
    if next != *current {
        log!(
            "{WORKER_NAME}: watch {} {} ns={:?}: {} -> {} ({reason})",
            spec.target.endpoint,
            spec.resource,
            spec.namespace,
            current,
            next
        );
    }
    *current = next;
    shmem::set_state(spec.slot, spec.id, next, reason)
}

/// One subscription's stream loop: connect, (re)list or resume, apply events,
/// back off and reconnect on loss. Ends only when the slot is gone.
async fn stream_task(spec: Rc<SubSpec>) {
    let mut backoff = Backoff::new(BACKOFF_BASE, BACKOFF_MAX);
    let mut state = spec.state;
    // Resume point: a bookmark survives a worker restart (kept in shared memory).
    let mut resume: Option<String> =
        if spec.bookmark.is_empty() || !matches!(state, SubState::Degraded) {
            None
        } else {
            Some(spec.bookmark.clone())
        };
    loop {
        let rv = resume.clone().unwrap_or_default();
        let outcome = run_stream(&spec, &rv, &mut resume, &mut state, &mut backoff).await;
        let (ev, reason, was_cache_full) = match outcome {
            Ok(EventAction::Relist) => {
                resume = None;
                if let Err(ShmemError::SlotGone) = shmem::clear(spec.slot, spec.id) {
                    return;
                }
                (
                    StreamEvent::ResyncRequired,
                    "gateway requested a full resync".to_owned(),
                    false,
                )
            }
            Ok(_) => (StreamEvent::Lost, "stream ended".to_owned(), false),
            Err(f) => (StreamEvent::Lost, f.reason, f.cache_full),
        };
        if let Err(ShmemError::SlotGone) =
            transition(&spec, &mut state, ev, resume.is_some(), &reason)
        {
            return;
        }
        if was_cache_full {
            // The longest interval straight away, rather than climbing to it
            // from a second. Nothing here is transient: until something frees
            // space, every attempt reopens a stream and walks into the same
            // wall, and doing that every second costs the gateway and the API
            // server a full listing each time for no possible progress.
            //
            // It does keep trying, though. `sweep` reclaims expired
            // tombstones, another subscription may be dropped, and the cache
            // GUC can be raised and the server restarted -- so recovery
            // happens on its own, without needing anyone to restart the
            // worker. Meanwhile a cache that had synced keeps being served
            // stale, and one that had not is not served at all.
            tokio::time::sleep(BACKOFF_MAX).await;
        } else {
            tokio::time::sleep(backoff.on_failure()).await;
        }
    }
}

/// Why a stream ended, and whether retrying can help.
struct StreamFailure {
    /// Shown in `axiom_watch_status()` and in the WARNING a stale scan raises.
    reason: String,
    /// The shared cache is full. Distinguished because nothing about
    /// reconnecting changes it: `axiom.cache_size_mb` is a standing condition,
    /// not a transient one, so the usual reconnect-and-relist is not recovery,
    /// it is churn.
    cache_full: bool,
}

impl StreamFailure {
    fn lost(reason: String) -> Self {
        Self {
            reason,
            cache_full: false,
        }
    }

    /// Classifies a cache write failure.
    ///
    /// Only an exhausted cache is treated as standing. Everything else keeps
    /// the ordinary reconnect-and-relist, because everything else plausibly
    /// clears on its own.
    fn from_cache_write(e: &ShmemError) -> Self {
        match e {
            ShmemError::OutOfMemory => Self {
                reason: "the shared cache is full (axiom.cache_size_mb); this subscription \
                         cannot take new objects until there is room"
                    .to_owned(),
                cache_full: true,
            },
            other => Self::lost(format!("cache write failed: {other}")),
        }
    }
}

/// Opens one stream and pumps it until it ends. `Ok(Relist)` means resync
/// requested; `Ok(Continue)` means the stream ended cleanly; `Err` is the
/// reason it failed. The slot is marked `Resyncing` only once the gateway has
/// accepted the subscription: a failed connect attempt must not disturb a
/// `Degraded` slot's servability or its bookmark. Clears the cache before
/// applying a full listing.
async fn run_stream(
    spec: &SubSpec,
    rv: &str,
    resume: &mut Option<String>,
    state: &mut SubState,
    backoff: &mut Backoff,
) -> Result<EventAction, StreamFailure> {
    // WhileIdle: this channel carries the watch stream, which is idle whenever
    // the cluster is quiet and has no next request to discover a dead
    // connection with. The unary channels cached per backend deliberately do
    // not do this; see transport::Keepalive.
    let channel = build_channel_with(&spec.target, spec.rpc_timeout, Keepalive::WhileIdle)
        .map_err(|e| StreamFailure::lost(e.to_string()))?;
    // A watch's initial listing arrives as individual events, but one object
    // can still be large, so the subscription client needs the same ceiling.
    let mut client = GatewayServiceClient::new(channel)
        .max_decoding_message_size(transport::MAX_MESSAGE_BYTES)
        .max_encoding_message_size(transport::MAX_MESSAGE_BYTES);
    let r = &spec.resource;
    let req = SubscribeRequest {
        gvk: Some(crate::proto::v1::GroupVersionKind {
            group: r.group.to_string(),
            version: r.version.to_string(),
            kind: r.kind.to_string(),
        }),
        namespace: spec.namespace.clone(),
        resource_version: rv.to_owned(),
    };
    let mut stream = tokio::time::timeout(spec.rpc_timeout, client.subscribe(req))
        .await
        .map_err(|_| {
            StreamFailure::lost(format!("no reply within {}s", spec.rpc_timeout.as_secs()))
        })?
        .map_err(|s| {
            StreamFailure::lost(format!("gateway returned {:?}: {}", s.code(), s.message()))
        })?
        .into_inner();
    let why = if rv.is_empty() {
        "listing"
    } else {
        "resuming from bookmark; stale until the first bookmark"
    };
    transition(spec, state, StreamEvent::Opened, resume.is_some(), why)
        .map_err(|e| StreamFailure::lost(e.to_string()))?;
    // A full listing always starts from an empty cache, so objects deleted
    // while disconnected do not linger. This holds even after a cache-full
    // failure, where keeping the partial contents looks tempting: a fresh
    // listing sends only ADDED, so an object deleted cluster-side in the
    // meantime is never mentioned, and retained entries would survive into the
    // ACTIVE cache that the eventual successful listing produces -- served as
    // authoritative, forever, with nothing to correct them. The partial
    // contents are not being served anyway (see `retention_would_not_help`).
    if rv.is_empty() {
        shmem::clear(spec.slot, spec.id).map_err(|e| StreamFailure::lost(e.to_string()))?;
    }
    // A resumed stream is already "synced" for bookmark purposes: its events
    // are live changes, not a partial listing.
    let mut synced = !rv.is_empty();
    loop {
        match stream.message().await {
            Ok(Some(ev)) => match apply_event(spec, ev, resume, &mut synced) {
                Ok(EventAction::Continue) => {}
                Ok(EventAction::Synced) => {
                    backoff.on_success();
                    *state = SubState::Active;
                }
                Ok(EventAction::Relist) => return Ok(EventAction::Relist),
                Err(ShmemError::SlotGone) => {
                    return Err(StreamFailure::lost("subscription dropped".to_owned()))
                }
                Err(e) => return Err(StreamFailure::from_cache_write(&e)),
            },
            Ok(None) => return Ok(EventAction::Continue),
            Err(s) => {
                return Err(StreamFailure::lost(format!(
                    "stream error {:?}: {}",
                    s.code(),
                    s.message()
                )))
            }
        }
    }
}

/// Spawns stream tasks for subscriptions that do not have one yet.
fn manage_subscriptions(tasks: &mut HashMap<(usize, u32), tokio::task::JoinHandle<()>>) {
    let specs = match shmem::subscriptions() {
        Ok(s) => s,
        Err(ShmemError::NotAvailable) => return,
        Err(e) => {
            warning!("{WORKER_NAME}: cannot read subscriptions: {e}");
            return;
        }
    };
    tasks.retain(|_, h| !h.is_finished());
    for spec in specs {
        let key = (spec.slot, spec.id);
        if tasks.contains_key(&key) {
            continue;
        }
        log!(
            "{WORKER_NAME}: starting watch for {} {} ns={:?}",
            spec.target.endpoint,
            spec.resource,
            spec.namespace
        );
        let spec = Rc::new(spec);
        tasks.insert(key, tokio::task::spawn_local(stream_task(spec)));
    }
}

/// Entry point of the background worker (named in [`register`]).
///
/// Loop contract: a 250 ms tick checks signals (SIGTERM ends the loop, SIGHUP
/// reloads GUCs), runs the ping when due, spawns stream tasks for new
/// subscriptions, and sweeps tombstones once a second. Stream tasks run on the
/// same thread; nothing here spawns OS threads inside the Postgres process.
#[pg_guard]
#[no_mangle]
pub extern "C-unwind" fn axiom_bgworker_main(_arg: pg_sys::Datum) {
    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM);
    let notify_db = NOTIFY_DATABASE.get().map_or_else(
        || "postgres".to_owned(),
        |c| c.to_string_lossy().into_owned(),
    );
    BackgroundWorker::connect_worker_to_spi(Some(&notify_db), None);
    log!("{WORKER_NAME}: starting (notify database {notify_db:?})");

    let cache_bytes = usize::try_from(CACHE_SIZE_MB.get())
        .unwrap_or(256)
        .saturating_mul(1024 * 1024);
    match shmem::worker_init(cache_bytes) {
        Ok(()) => log!(
            "{WORKER_NAME}: watch cache ready ({} MiB limit)",
            cache_bytes / (1024 * 1024)
        ),
        Err(e) => warning!("{WORKER_NAME}: watch cache unavailable: {e}"),
    }

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            // Startup invariant: without a runtime nothing else can work.
            // Returning lets Postgres restart us after `set_restart_time`.
            warning!("{WORKER_NAME}: cannot create tokio runtime, exiting: {e}");
            return;
        }
    };
    let local = tokio::task::LocalSet::new();
    let handle = rt.handle().clone();
    local.block_on(&rt, async move {
        let mut pinger = Pinger::new();
        let mut tasks: HashMap<(usize, u32), tokio::task::JoinHandle<()>> = HashMap::new();
        let mut last_sweep = Instant::now();
        let mut tick = tokio::time::interval(TICK);
        loop {
            tick.tick().await;
            if BackgroundWorker::sigterm_received() {
                break;
            }
            if BackgroundWorker::sighup_received() {
                // SAFETY: standard bgworker SIGHUP handling from the main loop, no transaction open.
                unsafe { pg_sys::ProcessConfigFile(pg_sys::GucContext::PGC_SIGHUP) };
            }
            pinger.tick(&handle).await;
            manage_subscriptions(&mut tasks);
            if last_sweep.elapsed() >= SWEEP_EVERY {
                last_sweep = Instant::now();
                match shmem::sweep() {
                    Ok(0) | Err(ShmemError::NotAvailable) => {}
                    Ok(n) => log!("{WORKER_NAME}: swept {n} tombstones"),
                    Err(e) => warning!("{WORKER_NAME}: sweep failed: {e}"),
                }
            }
        }
        for (_, h) in tasks.drain() {
            h.abort();
        }
    });
    log!("{WORKER_NAME}: exiting on SIGTERM");
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    use crate::cache::{cache_servable, decide_tier, CacheMode, Tier};

    /// An exhausted cache is a standing condition, not a lost stream.
    ///
    /// The distinction drives the behaviour that matters more than the label:
    /// the retry waits at the longest interval instead of climbing to it from
    /// a second. Nothing about reconnecting frees space, so a listing per
    /// second costs the gateway and the API server real work for no possible
    /// progress.
    #[test]
    fn a_full_cache_is_distinguished_from_a_lost_stream() {
        let full = StreamFailure::from_cache_write(&ShmemError::OutOfMemory);
        assert!(
            full.cache_full,
            "an exhausted cache must not be treated as transient"
        );
        assert!(
            full.reason.contains("axiom.cache_size_mb"),
            "the reason must name the setting to raise: {}",
            full.reason
        );
        // Deliberately not claiming what happens to scans. A cache that
        // filled after syncing keeps being served stale; one that filled
        // mid-listing is not served at all. One reason reaches both.
        assert!(
            full.reason.contains("cannot take new objects"),
            "the reason should say what the subscription has stopped doing: {}",
            full.reason
        );

        // Everything else keeps the ordinary reconnect-and-relist, because
        // everything else plausibly clears on its own.
        for e in [
            ShmemError::CacheNotReady,
            ShmemError::NotAvailable,
            ShmemError::TooLarge("object"),
        ] {
            let f = StreamFailure::from_cache_write(&e);
            assert!(
                !f.cache_full,
                "{e} should stay a retryable stream failure, not a standing one"
            );
            assert!(
                f.reason.contains("cache write failed"),
                "unexpected reason for {e}: {}",
                f.reason
            );
        }
    }

    /// Why the cache is cleared before relisting even when it is full.
    ///
    /// Keeping the partial contents reads like a kindness -- serve the stale
    /// rows rather than throw them away -- but on the only path where it would
    /// change anything, there are no stale rows to serve.
    ///
    /// The clear runs only before a *fresh* listing; a resume leaves the cache
    /// alone already. And a fresh listing has no bookmark, because none is
    /// recorded until SYNCED, so a cache-full failure during one sends the
    /// slot to `Requested`, whose contents are not servable. Scans fall back
    /// to the gateway. (A cache that fills later, on a live event, does have a
    /// bookmark: it goes `Degraded` and keeps being served stale. That path
    /// resumes and never reaches this clear.)
    ///
    /// Retention would therefore buy nothing and cost correctness. A fresh
    /// listing sends only ADDED, so an object deleted cluster-side while the
    /// cache was full is never mentioned by the listing that eventually
    /// succeeds. A retained entry would survive into an `ACTIVE` cache and be
    /// served as authoritative with nothing left to correct it.
    #[test]
    fn retention_would_not_help_because_a_filling_cache_is_not_served() {
        // A fill has no bookmark, so losing the stream cannot leave it
        // `Degraded` -- the one state in which a lost stream's contents stay
        // servable.
        let after = next_state(SubState::Resyncing, StreamEvent::Lost, false);
        assert_eq!(
            after,
            SubState::Requested,
            "a listing that fails without a bookmark must not stay servable"
        );
        assert!(
            !cache_servable(after),
            "{after:?} must not serve its partial contents"
        );
        assert_eq!(
            decide_tier(CacheMode::Watch, Some(after)),
            Tier::OnDemand,
            "scans must fall back to the gateway, not read a partial cache"
        );
    }
}
