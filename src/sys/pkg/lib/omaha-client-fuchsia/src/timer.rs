// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_async as fasync;
use futures::future::BoxFuture;
use futures::prelude::*;
use omaha_client::time::{PartialComplexTime, Timer};
use std::cmp::min;
use std::time::{Duration, Instant, SystemTime};

pub struct FuchsiaTimer;
impl FuchsiaTimer {
    // Return the duration until the given SystemTime, or a 0-length duration if it's in the past.
    // w.r.t clippy, Duration::from_secs is a const fn, and is more readable than unwrap_or_default
    #[allow(clippy::or_fun_call)]
    fn duration_until_system_time(system: SystemTime) -> Duration {
        system.duration_since(SystemTime::now()).ok().unwrap_or(Duration::from_secs(0))
    }

    // Return the duration until the given Instant, or a 0-length duration if it's in the past.
    // w.r.t clippy, Duration::from_secs is a const fn, and is more readable than unwrap_or_default
    #[allow(clippy::or_fun_call)]
    fn duration_until_instant(instant: Instant) -> Duration {
        instant.checked_duration_since(Instant::now()).unwrap_or(Duration::from_secs(0))
    }

    fn determine_wait_until(time: PartialComplexTime) -> Duration {
        match time {
            PartialComplexTime::Wall(w) => Self::duration_until_system_time(w),
            PartialComplexTime::Monotonic(m) => Self::duration_until_instant(m),
            PartialComplexTime::Complex(c) => {
                min(Self::duration_until_system_time(c.wall), Self::duration_until_instant(c.mono))
            }
        }
    }
}
impl Timer for FuchsiaTimer {
    /// Wait until at least one of the given time bounds has been reached.
    fn wait_until(&mut self, time: impl Into<PartialComplexTime>) -> BoxFuture<'static, ()> {
        fasync::Timer::new(Self::determine_wait_until(time.into())).boxed()
    }

    /// Wait for the given duration (from now).
    fn wait_for(&mut self, duration: Duration) -> BoxFuture<'static, ()> {
        fasync::Timer::new(duration).boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_async::TestExecutor;
    use std::task::Poll;

    #[fuchsia::test(allow_stalls = false)]
    async fn test_timer() {
        let start_time = fasync::MonotonicInstant::now();

        let mut timer = FuchsiaTimer;
        let mut future = timer.wait_for(Duration::from_secs(1234));
        assert_eq!(Poll::Pending, TestExecutor::poll_until_stalled(&mut future).await);

        let future_time = TestExecutor::next_timer().unwrap();
        TestExecutor::advance_to(future_time).await;
        assert_eq!(1234, (future_time - start_time).into_seconds());

        let () = future.await;
    }
}
