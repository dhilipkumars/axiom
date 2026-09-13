//! Exponential backoff for gateway reconnects. Pure; no I/O.

use std::time::Duration;

/// Capped exponential backoff with a reset-on-success state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backoff {
    base: Duration,
    max: Duration,
    consecutive_failures: u32,
}

impl Backoff {
    /// Creates a backoff that starts at `base` and doubles up to `max`.
    /// If `max < base`, `max` is raised to `base` so the schedule is monotonic.
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max: max.max(base),
            consecutive_failures: 0,
        }
    }

    /// Records a failure and returns how long to wait before the next attempt.
    /// The delay is `base * 2^(n-1)` for the n-th consecutive failure, capped
    /// at `max`; the exponent saturates so it never overflows.
    pub fn on_failure(&mut self) -> Duration {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let shift = self.consecutive_failures.saturating_sub(1).min(31);
        self.base.saturating_mul(1u32 << shift).min(self.max)
    }

    /// Records a success, resetting the schedule.
    pub fn on_success(&mut self) {
        self.consecutive_failures = 0;
    }

    /// Number of failures since the last success.
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doubles_then_caps() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(10));
        assert_eq!(b.on_failure(), Duration::from_secs(1));
        assert_eq!(b.on_failure(), Duration::from_secs(2));
        assert_eq!(b.on_failure(), Duration::from_secs(4));
        assert_eq!(b.on_failure(), Duration::from_secs(8));
        assert_eq!(b.on_failure(), Duration::from_secs(10));
        assert_eq!(b.on_failure(), Duration::from_secs(10));
        assert_eq!(b.consecutive_failures(), 6);
    }

    #[test]
    fn success_resets() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(10));
        b.on_failure();
        b.on_failure();
        b.on_success();
        assert_eq!(b.consecutive_failures(), 0);
        assert_eq!(b.on_failure(), Duration::from_secs(1));
    }

    #[test]
    fn never_overflows() {
        let mut b = Backoff::new(Duration::from_hours(1), Duration::MAX);
        for _ in 0..100 {
            let _ = b.on_failure();
        }
        assert_eq!(b.consecutive_failures(), 100);
        assert!(b.on_failure() > Duration::from_hours(1));
    }

    #[test]
    fn max_below_base_is_raised() {
        let mut b = Backoff::new(Duration::from_secs(5), Duration::from_secs(1));
        assert_eq!(b.on_failure(), Duration::from_secs(5));
    }
}
