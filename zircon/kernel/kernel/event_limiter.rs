// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Thread-safe rate limiter for periodic actions, mirroring `<kernel/event_limiter.h>`.

use crate::platform_rs::timer::current_mono_time;
use core::sync::atomic::{AtomicI64, Ordering};
use zx_types::{zx_duration_mono_t, zx_instant_mono_t};

/// An `EventLimiter` allows an action to only be taken every `DURATION` nanoseconds in a
/// thread-safe manner.
///
/// Use as follows:
///
/// ```ignore
/// static LIMITER: EventLimiter<{ DurationMono::from_seconds(1).into_nanos() }> =
///     EventLimiter::new();
/// while ... {
///     if LIMITER.ready() {
///         kprintln!("...");
///     }
///     // ...
/// }
/// ```
pub struct EventLimiter<const DURATION: zx_duration_mono_t> {
    last_event: AtomicI64,
}

impl<const DURATION: zx_duration_mono_t> EventLimiter<DURATION> {
    /// Creates a new `EventLimiter` with no prior event recorded.
    pub const fn new() -> Self {
        Self { last_event: AtomicI64::new(0) }
    }

    /// Returns `true` if at least `DURATION` nanoseconds have elapsed since the last ready event
    /// and this caller won the race to record the new timestamp.
    pub fn ready(&self) -> bool {
        let now: zx_instant_mono_t = current_mono_time().into_nanos();

        // If we have recently taken action, we don't need to do it again.
        let last_event = self.last_event.load(Ordering::Relaxed);
        if last_event != 0 && now < last_event.saturating_add(DURATION) {
            return false;
        }

        // Otherwise, record that we have acted. If we race with another thread, assume it has
        // taken action and we don't need to.
        self.last_event
            .compare_exchange(last_event, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }
}

impl<const DURATION: zx_duration_mono_t> Default for EventLimiter<DURATION> {
    fn default() -> Self {
        Self::new()
    }
}

/// Unit tests for [`EventLimiter`].
#[cfg(ktest)]
#[unittest::suite(name = "event_limiter_rust")]
mod tests {
    use super::EventLimiter;
    use crate::kernel::thread::sleep_relative;
    use crate::platform_rs::timer::DurationMono;
    use unittest::{expect_false, expect_true};

    /// Verifies that a zero-duration `EventLimiter` is always ready.
    #[test]
    fn zero_duration_always_ready() {
        let limiter: EventLimiter<0> = EventLimiter::new();
        expect_true!(limiter.ready());
        expect_true!(limiter.ready());
        expect_true!(limiter.ready());
    }

    /// Verifies that `EventLimiter` throttles calls until `DURATION` has elapsed.
    #[test]
    fn throttles_until_duration_elapses() {
        let long_limiter: EventLimiter<{ DurationMono::from_hours(1).into_nanos() }> =
            EventLimiter::default();
        expect_true!(long_limiter.ready());
        expect_false!(long_limiter.ready());

        let short_limiter: EventLimiter<{ DurationMono::from_millis(1).into_nanos() }> =
            EventLimiter::new();
        expect_true!(short_limiter.ready());
        expect_false!(short_limiter.ready());
        let _ = sleep_relative(DurationMono::from_millis(2));
        expect_true!(short_limiter.ready());
    }
}
