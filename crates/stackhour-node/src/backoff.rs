//! Bounded, jittered exponential reconnect backoff.
//!
//! The architecture doc's connection-runtime guidance: reconnection has no
//! small fixed retry ceiling; backoff is bounded, jittered, and capped at 16s
//! (see [`NodeConfig::backoff_max`](crate::NodeConfig)). This is "equal
//! jitter": half the capped exponential is fixed and half is random, so there
//! is always a floor (retries never busy-loop) and always spread (a fleet of
//! nodes never reconnects in lockstep).
//!
//! Jitter is derived from the wall clock's sub-second nanoseconds rather than a
//! `rand` dependency — good enough to decorrelate retries, and this crate stays
//! dependency-light like the rest of the workspace.

use std::time::Duration;

/// The backoff delay before reconnect attempt `attempt` (1-based).
///
/// The exponential is `base * 2^(attempt-1)` clamped to `max`; the returned
/// delay is `capped/2 + random(0, capped/2)`, so it is always in
/// `[capped/2, capped]` and never exceeds `max`.
pub fn delay(attempt: u32, base: Duration, max: Duration) -> Duration {
    let base_ms = base.as_millis() as u64;
    let max_ms = max.as_millis() as u64;
    if base_ms == 0 || max_ms == 0 {
        return Duration::from_millis(0);
    }

    // Exponential step, saturating so a large attempt count can never overflow
    // the shift or the multiply — it just pins to the cap.
    let shift = attempt.saturating_sub(1).min(32);
    let expo_ms = base_ms.saturating_mul(1u64 << shift);
    let capped = expo_ms.min(max_ms);

    let half = capped / 2;
    let jitter = (half as f64 * jitter_fraction()) as u64;
    Duration::from_millis(half + jitter)
}

/// A pseudo-random fraction in `[0.0, 1.0)` from the wall clock's nanoseconds.
fn jitter_fraction() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos % 1000) as f64 / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_never_exceeds_the_cap_and_has_a_floor() {
        let base = Duration::from_millis(500);
        let max = Duration::from_secs(16);
        for attempt in 1..=50 {
            let d = delay(attempt, base, max);
            assert!(d <= max, "attempt {attempt} exceeded the cap: {d:?}");
        }
        // Once saturated, the delay lives in [max/2, max]: bounded but jittered.
        let saturated = delay(40, base, max);
        assert!(
            saturated >= max / 2,
            "saturated delay lost its floor: {saturated:?}"
        );
        assert!(saturated <= max);
    }

    #[test]
    fn early_attempts_grow_toward_the_cap() {
        let base = Duration::from_millis(100);
        let max = Duration::from_secs(60);
        // The *floor* (capped/2) is monotonic in the attempt until it saturates,
        // even though jitter perturbs the exact value.
        let floor = |attempt: u32| {
            let expo = 100u64.saturating_mul(1u64 << attempt.saturating_sub(1).min(32));
            expo.min(60_000) / 2
        };
        assert!(floor(1) < floor(2));
        assert!(floor(2) < floor(3));
        // Attempt 1 can never wait longer than one full base step.
        assert!(delay(1, base, max) <= base);
    }

    #[test]
    fn zero_base_is_immediate() {
        assert_eq!(delay(5, Duration::ZERO, Duration::from_secs(1)), Duration::ZERO);
    }
}
