//! The gateway pinger background worker: impure glue between Postgres
//! (GUCs, latch, elog) and the pure modules (`config`, `backoff`, `ping`).
//!
//! Lifecycle: one static worker registered from `_PG_init` under
//! `shared_preload_libraries`. It runs until SIGTERM. It never panics on a
//! failed RPC or bad configuration: failures are logged and retried with
//! exponential backoff, so a gateway outage never takes the worker down.

use std::ffi::CStr;
use std::time::Duration;

use pgrx::bgworkers::{
    BackgroundWorker, BackgroundWorkerBuilder, BgWorkerStartTime, SignalWakeFlags,
};
use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use pgrx::prelude::*;
use tonic::transport::Channel;

use crate::backoff::Backoff;
use crate::config::Settings;
use crate::ping::PingOutcome;
use crate::proto::v1::gateway_service_client::GatewayServiceClient;
use crate::proto::v1::PingRequest;
use crate::transport::{build_channel, ChannelError};

/// Human-readable worker name; also its `backend_type` in `pg_stat_activity`.
pub const WORKER_NAME: &str = "axiom gateway pinger";

static GATEWAY_ENDPOINT: GucSetting<Option<&'static CStr>> =
    GucSetting::<Option<&'static CStr>>::new(None);
static GATEWAY_CA_CERT: GucSetting<Option<&'static CStr>> =
    GucSetting::<Option<&'static CStr>>::new(None);
static PING_INTERVAL_SECS: GucSetting<i32> = GucSetting::<i32>::new(10);
static RPC_TIMEOUT_SECS: GucSetting<i32> = GucSetting::<i32>::new(5);

const BACKOFF_BASE: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Defines the `axiom.*` GUCs. Must be called from `_PG_init`.
///
/// All GUCs are `SIGHUP` context: the worker re-reads them on each tick after
/// a reload, so an operator can repoint the gateway without a restart.
pub fn define_gucs() {
    GucRegistry::define_string_guc(
        "axiom.gateway_endpoint",
        "URL of the Axiom gateway, e.g. https://gateway:8443",
        "Must use https. Credentials are never embedded here.",
        &GATEWAY_ENDPOINT,
        GucContext::Sighup,
        GucFlags::SUPERUSER_ONLY,
    );
    GucRegistry::define_string_guc(
        "axiom.gateway_ca_cert",
        "Path to a PEM CA bundle used to verify the gateway's TLS certificate",
        "Leave unset to use the Mozilla webpki root store.",
        &GATEWAY_CA_CERT,
        GucContext::Sighup,
        GucFlags::SUPERUSER_ONLY,
    );
    GucRegistry::define_int_guc(
        "axiom.ping_interval_secs",
        "Seconds between gateway Ping round-trips",
        "Must be greater than axiom.rpc_timeout_secs.",
        &PING_INTERVAL_SECS,
        1,
        3600,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        "axiom.rpc_timeout_secs",
        "Per-RPC deadline in seconds",
        "Must be smaller than axiom.ping_interval_secs.",
        &RPC_TIMEOUT_SECS,
        1,
        3600,
        GucContext::Sighup,
        GucFlags::default(),
    );
}

/// Registers the static background worker. Only valid while Postgres is
/// processing `shared_preload_libraries`; the caller checks that.
pub fn register() {
    BackgroundWorkerBuilder::new(WORKER_NAME)
        .set_function("axiom_bgworker_main")
        .set_library("axiom")
        // A database-less backend connection registers the worker in
        // `pg_stat_activity` (observable by tests) and is what Phase 3 needs
        // to issue NOTIFY. It also implies shared-memory access, which the
        // process latch (`WaitLatch`) requires. Database connections are only
        // permitted for workers starting at or after ConsistentState.
        .set_start_time(BgWorkerStartTime::RecoveryFinished)
        .enable_shmem_access(None)
        .enable_spi_access()
        .set_restart_time(Some(Duration::from_secs(5)))
        .load();
}

/// Reads the current GUC values and validates them.
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

/// Connection state carried across ticks so the channel is reused while the
/// settings are unchanged.
struct Conn {
    settings: Settings,
    client: GatewayServiceClient<Channel>,
}

/// Performs one `Ping` with the configured deadline and classifies it.
fn ping_once(rt: &tokio::runtime::Runtime, conn: &mut Conn, nonce: u64) -> PingOutcome {
    let timeout = conn.settings.rpc_timeout;
    let result = rt.block_on(async {
        match tokio::time::timeout(timeout, conn.client.ping(PingRequest { nonce })).await {
            Ok(Ok(resp)) => Ok(resp.into_inner()),
            Ok(Err(status)) => Err(status),
            Err(_elapsed) => Err(tonic::Status::deadline_exceeded(format!(
                "no reply within {}s",
                timeout.as_secs()
            ))),
        }
    });
    PingOutcome::classify(nonce, result)
}

/// Entry point of the background worker (named in [`register`]).
///
/// Loop contract: each tick re-reads settings (so SIGHUP reloads take effect),
/// (re)builds the channel if settings changed, pings once, and logs the
/// outcome with a stable `axiom bgworker: ping ok|failed` prefix. Failures
/// back off exponentially (1s..60s); successes wait `ping_interval`. Returns
/// only on SIGTERM.
#[pg_guard]
#[no_mangle]
pub extern "C" fn axiom_bgworker_main(_arg: pg_sys::Datum) {
    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM);
    // No database: Phase 0 never runs SQL from the worker. See `register`.
    BackgroundWorker::connect_worker_to_spi(None, None);
    log!("{WORKER_NAME}: starting");

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

    let mut backoff = Backoff::new(BACKOFF_BASE, BACKOFF_MAX);
    let mut conn: Option<Conn> = None;
    let mut nonce: u64 = 0;
    let mut wait = Duration::ZERO;

    while BackgroundWorker::wait_latch(Some(wait)) {
        if BackgroundWorker::sighup_received() {
            // SAFETY: standard bgworker SIGHUP handling; we are in the main
            // loop, not a signal handler, with no transaction open.
            unsafe { pg_sys::ProcessConfigFile(pg_sys::GucContext::PGC_SIGHUP) };
        }

        let settings = match read_settings() {
            Ok(s) => s,
            Err(e) => {
                conn = None;
                warning!("{WORKER_NAME}: invalid configuration, not pinging: {e}");
                wait = backoff.on_failure();
                continue;
            }
        };

        if conn.as_ref().is_none_or(|c| c.settings != settings) {
            // `connect_lazy` spawns its connector task, so it must run inside
            // the runtime context even though no I/O happens yet.
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
                    conn = Some(Conn {
                        client: GatewayServiceClient::new(ch),
                        settings,
                    });
                }
                Err(e) => {
                    conn = None;
                    warning!("{WORKER_NAME}: {}", ChannelError::to_string(&e));
                    wait = backoff.on_failure();
                    continue;
                }
            }
        }
        let Some(c) = conn.as_mut() else {
            // Unreachable by construction (set just above), but never panic.
            wait = backoff.on_failure();
            continue;
        };

        nonce = nonce.wrapping_add(1).max(1);
        let outcome = ping_once(&rt, c, nonce);
        let line = outcome.log_line(&c.settings.target.endpoint);
        if outcome.is_success() {
            backoff.on_success();
            log!("{line}");
            wait = c.settings.ping_interval;
        } else {
            wait = backoff.on_failure();
            warning!(
                "{line} (retry in {}s, consecutive_failures={})",
                wait.as_secs(),
                backoff.consecutive_failures()
            );
        }
    }

    log!("{WORKER_NAME}: exiting on SIGTERM");
}
