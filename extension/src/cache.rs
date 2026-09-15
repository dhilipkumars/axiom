//! Pure logic for the watch-driven cache: consistency-tier decisions,
//! subscription state transitions, key hashing, and tombstone sweeping. No
//! Postgres, no shared memory here; `shmem.rs` is the impure side.

use std::fmt;

/// Lifecycle of one subscription `(server, kind, namespace)` as seen by scans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SubState {
    /// Slot is unused.
    Free = 0,
    /// A backend asked for a watch; the worker has not started it yet.
    Requested = 1,
    /// Stream open, initial listing in progress (or resume after a resync).
    Resyncing = 2,
    /// Stream open and caught up: the cache is authoritative.
    Active = 3,
    /// Stream lost; the cache is served as stale until the worker resyncs.
    Degraded = 4,
}

impl SubState {
    /// Decodes the shared-memory byte.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Requested,
            2 => Self::Resyncing,
            3 => Self::Active,
            4 => Self::Degraded,
            _ => Self::Free,
        }
    }

    /// SQL-visible name, as returned by `axiom_watch_status()`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Free => "FREE",
            Self::Requested => "REQUESTED",
            Self::Resyncing => "RESYNCING",
            Self::Active => "ACTIVE",
            Self::Degraded => "DEGRADED",
        }
    }
}

impl fmt::Display for SubState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// `cache_mode` foreign-table option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheMode {
    /// Every scan is a gateway RPC (the default).
    #[default]
    OnDemand,
    /// Scans are served from the watch-driven cache once it is active.
    Watch,
}

impl CacheMode {
    /// Parses the option value.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "on_demand" => Some(Self::OnDemand),
            "watch" => Some(Self::Watch),
            _ => None,
        }
    }
}

/// Where a scan's rows come from (docs/DESIGN.md §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Direct unary RPC.
    OnDemand,
    /// Cache is authoritative (`LIVE`).
    Live,
    /// Cache served with a staleness warning (`STALE-BUT-SERVEABLE`).
    Stale,
}

/// Decides the tier for a scan given the table's mode and the subscription
/// state (if any). Never lies about freshness: a degraded subscription is
/// `Stale`, a subscription that is not yet caught up falls through to an RPC.
pub fn decide_tier(mode: CacheMode, state: Option<SubState>) -> Tier {
    match (mode, state) {
        (CacheMode::Watch, Some(SubState::Active)) => Tier::Live,
        (CacheMode::Watch, Some(SubState::Degraded)) => Tier::Stale,
        // On-demand tables, and watch tables whose subscription is absent or
        // not yet caught up, always go to the gateway.
        (CacheMode::OnDemand, _)
        | (
            CacheMode::Watch,
            None | Some(SubState::Free | SubState::Requested | SubState::Resyncing),
        ) => Tier::OnDemand,
    }
}

/// Recorded as a subscription's reason when a cache write fails for want of
/// room.
///
/// Single-sourced because two places depend on the exact text: the worker
/// writes it, and a scan falling back to the gateway matches on it to tell the
/// person running the query why caching stopped. Deliberately says nothing
/// about what happens to scans -- that differs by when the cache filled, and
/// one reason reaches both paths.
pub const CACHE_FULL_REASON: &str = "the shared cache is full (axiom.cache_size_mb); \
     this subscription cannot take new objects until there is room";

/// Events the worker feeds into a subscription's state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEvent {
    /// Stream opened (with or without a resume point).
    Opened,
    /// Initial listing complete.
    Synced,
    /// API server bookmark: the watcher is caught up (activates a resumed stream).
    Bookmark,
    /// Stream ended with an error / disconnect.
    Lost,
    /// Gateway said the resume point is gone; cache must be rebuilt.
    ResyncRequired,
}

/// Next state for a subscription. Pure and total.
///
/// `has_bookmark` says whether a resume point exists (the stream synced at
/// least once). Opening a stream *with* a bookmark is a resume: the cache stays
/// servable-but-stale (`Degraded`) until the API server's first BOOKMARK
/// proves the backlog is delivered, because there is no other honest signal
/// (a resume gets no SYNCED). Opening without one is a fresh listing
/// (`Resyncing`, cache not served). Losing the stream with a bookmark keeps
/// the cache servable as `Degraded`; without one there is nothing to serve, so
/// the slot goes back to `Requested`.
pub fn next_state(current: SubState, event: StreamEvent, has_bookmark: bool) -> SubState {
    match (current, event) {
        (SubState::Free, _) => SubState::Free,
        // With a bookmark the cache stays servable-but-stale, whether the stream
        // was just lost or is being resumed (until the first bookmark arrives).
        (_, StreamEvent::Opened | StreamEvent::Lost) if has_bookmark => SubState::Degraded,
        (_, StreamEvent::Opened | StreamEvent::ResyncRequired) => SubState::Resyncing,
        (_, StreamEvent::Synced | StreamEvent::Bookmark) => SubState::Active,
        (_, StreamEvent::Lost) => SubState::Requested,
    }
}

/// Whether the cache contents may be served in `state`.
pub fn cache_servable(state: SubState) -> bool {
    matches!(state, SubState::Active | SubState::Degraded)
}

/// FNV-1a over `namespace\0name`; stable across processes and versions, which
/// is all a shared-memory hash index needs.
pub fn key_hash(namespace: &str, name: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in namespace
        .bytes()
        .chain(std::iter::once(0))
        .chain(name.bytes())
    {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Grace period a deleted object stays visible as a tombstone so a scan that
/// started before the delete does not see it vanish and reappear.
pub const TOMBSTONE_GRACE_US: i64 = 2_000_000;

/// Whether a tombstone written at `deleted_at_us` should be swept at `now_us`.
pub fn tombstone_expired(deleted_at_us: i64, now_us: i64) -> bool {
    now_us.saturating_sub(deleted_at_us) >= TOMBSTONE_GRACE_US
}

/// Bucket count to use for `live` entries: grow by doubling past 75 % load.
pub fn buckets_for(current: u32, live: u32) -> u32 {
    let mut n = current.max(64);
    while n < u32::MAX && u64::from(live) * 4 > u64::from(n) * 3 {
        n = n.saturating_mul(2);
    }
    n
}

/// Whether a `List`-style filter matches an object key. Empty filter = all.
pub fn key_matches(ns_filter: &str, name_filter: &str, namespace: &str, name: &str) -> bool {
    (ns_filter.is_empty() || ns_filter == namespace)
        && (name_filter.is_empty() || name_filter == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_decision_never_lies() {
        assert_eq!(
            decide_tier(CacheMode::OnDemand, Some(SubState::Active)),
            Tier::OnDemand
        );
        assert_eq!(decide_tier(CacheMode::Watch, None), Tier::OnDemand);
        assert_eq!(
            decide_tier(CacheMode::Watch, Some(SubState::Requested)),
            Tier::OnDemand
        );
        assert_eq!(
            decide_tier(CacheMode::Watch, Some(SubState::Resyncing)),
            Tier::OnDemand
        );
        assert_eq!(
            decide_tier(CacheMode::Watch, Some(SubState::Active)),
            Tier::Live
        );
        assert_eq!(
            decide_tier(CacheMode::Watch, Some(SubState::Degraded)),
            Tier::Stale
        );
    }

    #[test]
    fn state_machine() {
        use StreamEvent as E;
        assert_eq!(
            next_state(SubState::Requested, E::Opened, false),
            SubState::Resyncing
        );
        assert_eq!(
            next_state(SubState::Resyncing, E::Synced, false),
            SubState::Active
        );
        assert_eq!(
            next_state(SubState::Active, E::Lost, true),
            SubState::Degraded
        );
        // A resume keeps serving stale until a bookmark proves the backlog is in.
        assert_eq!(
            next_state(SubState::Degraded, E::Opened, true),
            SubState::Degraded
        );
        assert_eq!(
            next_state(SubState::Degraded, E::Bookmark, true),
            SubState::Active
        );
        assert_eq!(
            next_state(SubState::Active, E::Bookmark, true),
            SubState::Active
        );
        // A failed reconnect attempt must not lose the bookmark's servability.
        assert_eq!(
            next_state(SubState::Degraded, E::Lost, true),
            SubState::Degraded
        );
        // Never synced: nothing to serve, list again.
        assert_eq!(
            next_state(SubState::Resyncing, E::Lost, false),
            SubState::Requested
        );
        assert_eq!(
            next_state(SubState::Active, E::ResyncRequired, true),
            SubState::Resyncing
        );
        assert_eq!(next_state(SubState::Free, E::Synced, false), SubState::Free);
        for s in [
            SubState::Free,
            SubState::Requested,
            SubState::Resyncing,
            SubState::Active,
            SubState::Degraded,
        ] {
            assert_eq!(SubState::from_u8(s as u8), s);
            assert_eq!(
                cache_servable(s),
                matches!(s, SubState::Active | SubState::Degraded)
            );
        }
        assert_eq!(SubState::from_u8(99), SubState::Free);
    }

    #[test]
    fn cache_mode_parse() {
        assert_eq!(CacheMode::parse("watch"), Some(CacheMode::Watch));
        assert_eq!(CacheMode::parse(" on_demand "), Some(CacheMode::OnDemand));
        assert_eq!(CacheMode::parse("live"), None);
        assert_eq!(CacheMode::default(), CacheMode::OnDemand);
    }

    #[test]
    fn hashing_and_matching() {
        assert_ne!(key_hash("a", "b"), key_hash("ab", ""));
        assert_ne!(key_hash("a", "b"), key_hash("b", "a"));
        assert_eq!(key_hash("ns", "n"), key_hash("ns", "n"));
        assert!(key_matches("", "", "x", "y"));
        assert!(key_matches("x", "", "x", "y"));
        assert!(!key_matches("z", "", "x", "y"));
        assert!(key_matches("x", "y", "x", "y"));
        assert!(!key_matches("x", "q", "x", "y"));
    }

    #[test]
    fn tombstones_and_buckets() {
        assert!(!tombstone_expired(1_000_000, 1_500_000));
        assert!(tombstone_expired(1_000_000, 3_000_000));
        assert!(tombstone_expired(0, i64::MAX));
        assert_eq!(buckets_for(0, 0), 64);
        assert_eq!(buckets_for(64, 48), 64);
        assert_eq!(buckets_for(64, 49), 128);
        assert_eq!(buckets_for(128, 1000), 2048);
        assert_eq!(buckets_for(u32::MAX, u32::MAX), u32::MAX);
    }
}
