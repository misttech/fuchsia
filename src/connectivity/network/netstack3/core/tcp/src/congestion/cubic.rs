// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The CUBIC congestion control algorithm as described in
//! [RFC 9438](https://tools.ietf.org/html/rfc9438).
//!
//! Note: This module uses fixed point arithmetic, avoiding floating point
//! operations which may be expensive or outright unavailable depending on
//! where the stack is running. Two properties make that possible without
//! losing much precision:
//!
//! - All of the CUBIC constants are rational numbers, so they're expressed as
//!   a [`Ratio`] and applied with integer multiplications and divisions.
//! - The equations in the RFC are defined in terms of *segments*, so all the
//!   window sizes kept by this module are in segments as well. Conversions
//!   to and from the byte-denominated congestion window happen only at the
//!   edges.
//!
//! The only quantity that requires fractional precision is time, which is
//! represented by [`FixedSeconds`].

use core::num::NonZeroU32;
use core::time::Duration;

use netstack3_base::Instant;

use crate::internal::congestion::{CongestionControlParams, CongestionEvent};

/// A rational number.
///
/// All of the CUBIC constants are rational numbers, this type allows them
/// to be applied to window sizes without resorting to floating point
/// arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ratio {
    numerator: u32,
    denominator: NonZeroU32,
}

impl Ratio {
    /// The identity ratio.
    const ONE: Self = Self::new(1, 1).unwrap();

    const fn new(numerator: u32, denominator: u32) -> Option<Self> {
        match NonZeroU32::new(denominator) {
            Some(denominator) => Some(Self { numerator, denominator }),
            None => None,
        }
    }

    /// Returns this ratio with its numerator and denominator swapped.
    const fn inverse(self) -> Option<Self> {
        let Self { numerator, denominator } = self;
        Self::new(denominator.get(), numerator)
    }

    /// Multiplies `value` by this ratio, rounding down and saturating at
    /// `u32::MAX`.
    fn multiply(self, value: u32) -> u32 {
        let Self { numerator, denominator } = self;
        let result = u64::from(value) * u64::from(numerator) / u64::from(denominator.get());
        u32::try_from(result).unwrap_or(u32::MAX)
    }
}

/// Per RFC 9438 (https://tools.ietf.org/html/rfc9438#section-4.6):
///  Parameter beta_cubic SHOULD be set to 0.7.
const CUBIC_BETA: Ratio = Ratio::new(7, 10).unwrap();

/// Per RFC 9438 (https://tools.ietf.org/html/rfc9438#section-5.1):
///  Therefore, C SHOULD be set to 0.4.
///
/// In segments per second cubed.
const CUBIC_C: Ratio = Ratio::new(2, 5).unwrap();

/// Per RFC 9438 (https://tools.ietf.org/html/rfc9438#section-4.3):
///   alpha_cubic must be equal to 3 * ((1-beta_cubic)/(1+beta_cubic))
///
/// With beta_cubic equal to 7/10, that is exactly 9/17.
const CUBIC_ALPHA: Ratio = Ratio::new(
    3 * (CUBIC_BETA.denominator.get() - CUBIC_BETA.numerator),
    CUBIC_BETA.denominator.get() + CUBIC_BETA.numerator,
)
.unwrap();

/// Per RFC 9438 (https://www.rfc-editor.org/rfc/rfc9438#section-4.7), with
/// fast convergence enabled W_max is reduced to `cwnd * ((1 + beta_cubic)/2)`
/// on a congestion event.
const FAST_CONVERGENCE_W_MAX: Ratio = Ratio::new(
    CUBIC_BETA.denominator.get() + CUBIC_BETA.numerator,
    2 * CUBIC_BETA.denominator.get(),
)
.unwrap();

/// Per RFC 9438 (https://www.rfc-editor.org/rfc/rfc9438#section-4.2), the
/// congestion window is never allowed to grow to more than 1.5 times its
/// current value in a single RTT.
const TARGET_LIMIT: Ratio = Ratio::new(3, 2).unwrap();

/// Hosts [`FixedSeconds`] so that its invariant is upheld by construction.
mod fixed_seconds {
    use super::*;

    /// A number of seconds represented as a fixed point number with
    /// [`FixedSeconds::FRACTION_BITS`] fractional bits.
    ///
    /// The contained value is never larger than [`FixedSeconds::MAX`], which
    /// guarantees that [`FixedSeconds::cubed`] can't overflow. The only ways
    /// to create a value are [`Default`], [`From<Duration>`], and
    /// [`FixedSeconds::cube_root_of_segments`], all of which observe that
    /// bound, and the inner value is not reachable from outside this module.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
    pub(super) struct FixedSeconds(u64);

    impl FixedSeconds {
        /// The divisor that converts a cubed [`FixedSeconds`] value into a number of
        /// segments.
        ///
        /// It folds the CUBIC constant `C` and the fixed point scaling into a single
        /// constant, i.e. for a [`FixedSeconds`] value `x`:
        ///
        ///   C * (x / 2^FRACTION_BITS)^3 == x^3 / CUBIC_C_CUBE_DIVISOR
        const CUBIC_C_CUBE_DIVISOR: u64 = ((CUBIC_C.denominator.get() as u64)
            << (3 * FixedSeconds::FRACTION_BITS))
            / CUBIC_C.numerator as u64;

        /// The number of fractional bits in the representation.
        ///
        /// This gives time a resolution of 2^-10 seconds (a little under a
        /// millisecond), which is the same resolution used by Linux's CUBIC
        /// implementation. It's fine enough to follow the cubic function at
        /// RTT timescales, while coarse enough that cubing a [`FixedSeconds`]
        /// value comfortably fits in a `u64`.
        pub(super) const FRACTION_BITS: u32 = 10;

        /// The largest representable value.
        ///
        /// Values are capped at the cube root of `u64::MAX` so that
        /// cubing the value in [`Self::cubed_diff_segments`] never overflows.
        ///
        /// The value works out to around 43 hours in this representation, which
        /// is far outside the range of latency values we're regularly working
        /// with in TCP.
        pub(super) const MAX: Self = Self(cube_root(u64::MAX));

        /// Returns the value of time `x` at which the cubic function grows the
        /// window by `segments`, i.e. the solution to `C * x^3 == segments`.
        ///
        /// This is the cube root from Figure 2 in RFC 9438.
        pub(super) fn cube_root_of_segments(segments: u32) -> Self {
            // NB: The largest possible result here is
            // `cube_root(u32::MAX * CUBIC_C_CUBE_DIVISOR)`, which is smaller
            // than `Self::MAX`, so the invariant is upheld. See
            // `fixed_seconds_max_fits_largest_k`.
            Self(cube_root(u64::from(segments) * Self::CUBIC_C_CUBE_DIVISOR))
        }

        /// Calculates C*(self-other)^3 as a [`WindowDelta`].
        ///
        /// Typically `self` is `t` and `other` is `K` to calculate the window
        /// difference from the cubic RFC. This function calculates the cubed
        /// difference and applies the [`Self::CUBIC_C_CUBE_DIVISOR`] scaling
        /// that undoes the inner representation of time in `FixedSeconds`.
        pub(super) fn cubed_diff_segments(self, Self(other): Self) -> WindowDelta {
            let Self(this) = self;
            if this < other {
                // NB: Saturation is not reachable because values are capped at
                // `Self::MAX`, this is just defense in depth.
                let cubed_offset = (other - this).saturating_pow(3);
                // In the concave region the delta is subtracted from W_max, so it
                // must be rounded up for the resulting window to be rounded down.
                let delta = u32::try_from(cubed_offset.div_ceil(Self::CUBIC_C_CUBE_DIVISOR))
                    .unwrap_or(u32::MAX);
                WindowDelta::Negative(delta)
            } else {
                // NB: Saturation is not reachable because values are capped at
                // `Self::MAX`, this is just defense in depth.
                let cubed_offset = (this - other).saturating_pow(3);
                let delta =
                    u32::try_from(cubed_offset / Self::CUBIC_C_CUBE_DIVISOR).unwrap_or(u32::MAX);
                WindowDelta::Positive(delta)
            }
        }

        /// Returns the raw fixed point value.
        #[cfg(test)]
        pub(super) fn get(self) -> u64 {
            let Self(seconds) = self;
            seconds
        }
    }

    /// The window delta calculated from [`FixedSeconds::cubed_diff_segments`]
    /// in segments.
    pub(super) enum WindowDelta {
        Positive(u32),
        Negative(u32),
    }

    impl From<Duration> for FixedSeconds {
        fn from(duration: Duration) -> Self {
            const NANOS_PER_SECOND: u64 = 1_000_000_000;
            let Self(max) = Self::MAX;
            let seconds = duration.as_secs().saturating_mul(1 << Self::FRACTION_BITS);
            let fraction =
                (u64::from(duration.subsec_nanos()) << Self::FRACTION_BITS) / NANOS_PER_SECOND;
            Self(seconds.saturating_add(fraction).min(max))
        }
    }
}
use fixed_seconds::{FixedSeconds, WindowDelta};

/// Returns the cube root of `value`, rounded down.
///
/// Like other integer CUBIC implementations, this uses Newton-Raphson
/// iteration to find the root of `x^3 - value`:
///
///   x[n+1] = (2 * x[n] + value / x[n]^2) / 3
const fn cube_root(value: u64) -> u64 {
    if value == 0 {
        return 0;
    }
    // Any value smaller than 2^bits has a cube root smaller than
    // 2^ceil(bits/3), so this is an over-estimate of the root. Newton-Raphson
    // decreases monotonically towards the root from an over-estimate, which
    // guarantees that the loop below terminates.
    let mut estimate = 1u64 << (u64::BITS - value.leading_zeros()).div_ceil(3);
    loop {
        // NB: `estimate` is at most 2^22, so squaring it can't overflow.
        let next = (2 * estimate + value / (estimate * estimate)) / 3;
        if next >= estimate {
            return estimate;
        }
        estimate = next;
    }
}

/// The CUBIC algorithm state variables.
#[derive(Debug, Clone, Copy, PartialEq, derivative::Derivative)]
#[derivative(Default(bound = ""))]
pub(super) struct Cubic<I, const FAST_CONVERGENCE: bool> {
    /// The start of the current congestion avoidance epoch.
    epoch_start: Option<I>,
    /// The time it takes for the cubic growth function to increase the window
    /// back to `w_max` in the current congestion avoidance epoch.
    k: FixedSeconds,
    /// The window size when the last congestion event occurred, in segments.
    w_max: u32,
    /// The window size when `ssthresh` was most recently set (either upon
    /// exiting the first slow start or just before cwnd was reduced in the last
    /// congestion event), in segments.
    cwnd_prior: u32,
    /// An estimate for the congestion window, in segments, in the Reno friendly
    /// region.
    reno_w_est: u32,
    /// The running count of ACKed bytes during congestion avoidance that have
    /// not yet been accounted for by the Cubic congestion window. Effectively,
    /// it can be thought of as a remainder on the Cubic window, since our
    /// implementation uses integer arithmetic rather than floating point.
    remaining_cubic_bytes_acked: u32,
    /// The running count of ACKed bytes during congestion avoidance that have
    /// not yet been accounted for by the Reno friendly region. Effectively,
    /// it can be thought of as a remainder on the Reno window estimate, since
    /// our implementation uses integer arithmetic rather than floating point.
    remaining_reno_bytes_acked: u32,
}

impl<I: Instant, const FAST_CONVERGENCE: bool> Cubic<I, FAST_CONVERGENCE> {
    /// Returns the window size, in segments, governed by the cubic growth
    /// function.
    ///
    /// This function is responsible for the concave/convex regions described
    /// in the RFC.
    fn cubic_window(&self, t: Duration) -> u32 {
        // Per RFC 9438 (https://www.rfc-editor.org/rfc/rfc9438#section-4.2):
        //       W_cubic(t) = C*(t-K)^3 + W_max (Fig. 1)
        let t = FixedSeconds::from(t);
        match t.cubed_diff_segments(self.k) {
            WindowDelta::Negative(delta) => self.w_max.saturating_sub(delta),
            WindowDelta::Positive(delta) => self.w_max.saturating_add(delta),
        }
    }

    /// Updates the estimated Reno window upon the reception of an ACK.
    fn reno_friendly_window(&mut self, bytes_acked: u32, cwnd: u32) {
        // Per RFC 9438 (https://tools.ietf.org/html/rfc9438#section-4.3):
        //   Once [...] W_est >= cwnd_prior, the sender SHOULD set alpha_cubic
        //   to 1 to ensure that it can achieve the same congestion window
        //   increment rate as Reno.
        let one_over_alpha = if self.reno_w_est >= self.cwnd_prior {
            Ratio::ONE
        } else {
            CUBIC_ALPHA.inverse().unwrap()
        };

        // Per RFC 9438 (https://tools.ietf.org/html/rfc9438#section-4.3):
        //   W_est = W_est + alpha_cubic * (segments_acked / cwnd)
        //
        // Note: Here we use a similar approach as in appropriate byte counting
        // (RFC 3465) - We count how many bytes are now acked, then we use
        // Figure 4 to calculate how many acked bytes are needed to increase our
        // cwnd by an even multiple of MSS, which is cwnd/alpha_cubic.
        self.remaining_reno_bytes_acked =
            self.remaining_reno_bytes_acked.saturating_add(bytes_acked);
        let required_bytes = one_over_alpha.multiply(cwnd).max(1);
        let num_increments = self.remaining_reno_bytes_acked / required_bytes;
        self.reno_w_est = self.reno_w_est.saturating_add(num_increments);
        self.remaining_reno_bytes_acked %= required_bytes;
    }

    pub(super) fn on_ack(
        &mut self,
        CongestionControlParams { cwnd, ssthresh, mss }: &mut CongestionControlParams,
        mut bytes_acked: NonZeroU32,
        now: I,
        rtt: Duration,
    ) {
        if *cwnd < *ssthresh {
            // TODO(https://fxbug.dev/513208004): Implement the HyStart++ slow
            // start algorithm.

            // Slow start, Per RFC 5681 (https://www.rfc-editor.org/rfc/rfc5681#page-6):
            // we RECOMMEND that TCP implementations increase cwnd, per:
            //   cwnd += min (N, SMSS)                      (2)
            *cwnd = cwnd.saturating_add(u32::min(bytes_acked.get(), u32::from(*mss)));
            if *cwnd <= *ssthresh {
                return;
            }
            // Now that we are moving out of slow start, we need to treat the
            // extra bytes differently, set the cwnd back to ssthresh and then
            // backtrack the portion of bytes that should be processed in
            // congestion avoidance.
            match cwnd.checked_sub(*ssthresh).and_then(NonZeroU32::new) {
                None => return,
                Some(diff) => bytes_acked = diff,
            }
            *cwnd = *ssthresh;
        }

        // Congestion avoidance. The cubic equations are all expressed in
        // segments, so the congestion window is converted here and the
        // resulting increments are converted back to bytes below.
        let mss_bytes = u32::from(*mss);
        let cwnd_segments = *cwnd / mss_bytes;

        let epoch_start = match self.epoch_start {
            Some(epoch_start) => epoch_start,
            None => {
                // Setup the parameters for the current congestion avoidance epoch.
                if let Some(w_max_diff_cwnd) = self.w_max.checked_sub(cwnd_segments) {
                    // Per RFC 9438 (https://www.rfc-editor.org/rfc/rfc9438#section-4.2):
                    //   K is calculated using the following equation:
                    //       K = cube_root((w_max - cwnd_epoch) / C) (Fig. 2)
                    self.k = FixedSeconds::cube_root_of_segments(w_max_diff_cwnd);
                } else {
                    // Per RFC 9438 (https://www.rfc-editor.org/rfc/rfc9438#section-4.10):
                    //   When CUBIC uses HyStart++ [RFC9406], it may exit the
                    //   the first slow start without incurring any packet loss
                    //   and thus w_max is undefined. In this special case,
                    //   CUBIC sets cwnd_prior = cwnd and switches to congestion
                    //   avoidance. It then increases its congestion window
                    //   size using Figure 1, where t is the elapsed time since
                    //   the beginning of the current congestion avoidance
                    //   stage, K is set to 0, and w_max is set to the
                    //   congestion window size at the beginning of the current
                    //   congestion avoidance stage.
                    self.k = FixedSeconds::default();
                    self.w_max = cwnd_segments;
                    self.cwnd_prior = cwnd_segments;
                }
                self.epoch_start = Some(now);
                // Per RFC 9438 (https://www.rfc-editor.org/rfc/rfc9438#section-4.3):
                //   W_est is set equal to cwnd_epoch at the start of the
                //   congestion avoidance stage.
                self.reno_w_est = cwnd_segments;
                now
            }
        };

        // Per RFC 9438 (https://www.rfc-editor.org/rfc/rfc9438#section-4.2):
        //   Upon receiving a new ACK during congestion avoidance, CUBIC
        //   computes the target congestion window size after the next RTT
        //   using Figure 1 as follows [...]
        //       target = cwnd if W_cubic(t + RTT) < cwnd
        //       target = 1.5 * cwnd if W_cubic(t+ RTT) > 1.5 * cwnd
        //       target = W_cubic(t + RTT) otherwise
        // where earlier, t was defined as:
        //   t is the elapsed time in seconds from the beginning of the current
        //   congestion avoidance stage -- that is,
        //       t = t_current - t_epoch
        let t = now.saturating_duration_since(epoch_start);
        let target = self.cubic_window(t + rtt);
        let target = target.clamp(cwnd_segments, TARGET_LIMIT.multiply(cwnd_segments));

        // In a *very* rare case, we might overflow the counter if the acks
        // keep coming in and we can't increase our congestion window. Use
        // saturating add here as a defense so that we don't lost ack counts
        // by accident.
        self.remaining_cubic_bytes_acked =
            self.remaining_cubic_bytes_acked.saturating_add(bytes_acked.get());

        // Per RFC 9438 (https://www.rfc-editor.org/rfc/rfc9438#section-4.4):
        //   cwnd MUST be incremented by (target - cwnd)/cwnd for each
        //   received ACK.
        // Note: Here we use a similar approach as in appropriate byte counting
        // (RFC 3465) - We count how many bytes are now acked, then we use Eq. 1
        // to calculate how many acked bytes are needed to increase our cwnd
        // by an even multiple of MSS. The increase rate is (target - cwnd)/cwnd
        // segments per ACK, so the number of bytes that must be acknowledged
        // for the window to grow by a full segment is cwnd/(target - cwnd).
        // Because our cubic function is a monotonically increasing function,
        // this method is slightly more aggressive - if we need N acks to
        // increase our window by 1 MSS, then it would take the RFC method at
        // least N acks to increase the same amount. This method is used in the
        // original CUBIC paper[1], and it eliminates the need for a fractional
        // cwnd.
        // [1]: (https://www.cs.princeton.edu/courses/archive/fall16/cos561/papers/Cubic08.pdf)
        let mut cubic_cwnd_segments = cwnd_segments;
        let target_diff_cwnd = target - cwnd_segments;
        if target_diff_cwnd > 0 {
            let required_bytes = (cwnd_segments * mss_bytes / target_diff_cwnd).max(1);
            // Limit the increase to ensure we don't exceed the cubic target.
            let num_increments =
                (self.remaining_cubic_bytes_acked / required_bytes).min(target_diff_cwnd);
            if num_increments > 0 {
                // `saturating_add` avoids overflow in `cwnd`. See https://fxbug.dev/327628809.
                cubic_cwnd_segments = cwnd_segments.saturating_add(num_increments);
                self.remaining_cubic_bytes_acked = self
                    .remaining_cubic_bytes_acked
                    .saturating_sub(num_increments.saturating_mul(required_bytes));
            }
        }

        self.reno_friendly_window(bytes_acked.get(), *cwnd);

        // Per RFC 9438 (https://www.rfc-editor.org/rfc/rfc9438#section-4.3):
        //   CUBIC checks whether W_cubic(t) is less than W_est(t). If so,
        //   CUBIC is in the Reno-friendly region and cwnd SHOULD be set to
        //   W_est(t) at each reception of a new ACK.
        *cwnd = u32::max(cubic_cwnd_segments, self.reno_w_est).saturating_mul(mss_bytes);
    }

    pub(super) fn on_congestion_event(
        &mut self,
        CongestionControlParams { cwnd, ssthresh, mss }: &mut CongestionControlParams,
        event: CongestionEvent,
        flight_size: u32,
    ) {
        // End the current congestion avoidance epoch.
        self.epoch_start = None;
        let mss_bytes = u32::from(*mss);
        let cwnd_segments = *cwnd / mss_bytes;
        // Per RFC 9438 (https://www.rfc-editor.org/rfc/rfc9438#section-4.7):
        //   With fast convergence, when a congestion event occurs, W_max is
        //   updated as follows, before the window reduction described in
        //   Section 4.6:
        //       W_max = cwnd * ((1 + beta_cubic) / 2) if cwnd < W_max and fast
        //               convergence is enabled, further reduce W_max.
        //       W_max = cwnd otherwise.
        if FAST_CONVERGENCE && cwnd_segments < self.w_max {
            self.w_max = FAST_CONVERGENCE_W_MAX.multiply(cwnd_segments);
        } else {
            self.w_max = cwnd_segments;
        }

        // Per RFC 9438 (https://www.rfc-editor.org/rfc/rfc9438#section-4.6):
        //   When a congestion event is detected by the mechanisms described in
        //   Section 3.1, CUBIC updates W_max and reduces cwnd and ssthresh
        //   immediately, as described below.
        //       ssthresh = flight_size * beta_cubic
        //       cwnd_prior = cwnd
        //       cwnd = max(ssthresh, 2), if reduction on loss
        //       cwnd = max(ssthresh, 1), if reduction on ECE
        //       ssthresh = max(ssthresh, 2)
        self.cwnd_prior = cwnd_segments;
        let ssthresh_segments = CUBIC_BETA.multiply(flight_size) / mss_bytes;
        *ssthresh = u32::max(ssthresh_segments, 2) * mss_bytes;
        match event {
            CongestionEvent::PacketLoss => {
                *cwnd = *ssthresh;
            }
            CongestionEvent::Timeout => {
                // Per RFC 9438 (https://www.rfc-editor.org/rfc/rfc9438#section-4.8):
                //   In case of timeout, CUBIC follows Reno to reduce cwnd [RFC5681].
                // The Reno cwnd reduction strategy is described in RFC 5681
                // (https://www.rfc-editor.org/rfc/rfc5681#page-8):
                //   Furthermore, upon a timeout (as specified in [RFC2988]) cwnd MUST be
                //   set to no more than the loss window, LW, which equals 1 full-sized
                //   segment (regardless of the value of IW).
                *cwnd = mss_bytes
            }
        }

        // Reset our running count of the acked bytes.
        self.remaining_cubic_bytes_acked = 0;
        self.remaining_reno_bytes_acked = 0;
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use netstack3_base::testutil::{FakeInstant, FakeInstantCtx};
    use netstack3_base::{EffectiveMss, InstantContext as _, Mss, MssSizeLimiters};
    use proptest::test_runner::{Config, TestCaseError};
    use proptest::{prop_assert, proptest};
    use proptest_support::failed_seeds_no_std;
    use test_case::test_case;

    use super::*;

    const DEFAULT_MSS: EffectiveMss =
        EffectiveMss::from_mss(Mss::DEFAULT_IPV4, MssSizeLimiters { timestamp_enabled: false });
    impl<I: Instant, const FAST_CONVERGENCE: bool> Cubic<I, FAST_CONVERGENCE> {
        // Helper function in test that takes a u32 instead of a NonZeroU32
        // as we know we never pass 0 in the test and it's a bit clumsy to
        // convert a u32 into a NonZeroU32 every time.
        fn on_ack_u32(
            &mut self,
            params: &mut CongestionControlParams,
            bytes_acked: u32,
            now: I,
            rtt: Duration,
        ) {
            self.on_ack(params, NonZeroU32::new(bytes_acked).unwrap(), now, rtt)
        }
    }

    #[test]
    fn cube_root_of_zero() {
        assert_eq!(cube_root(0), 0);
    }

    #[test]
    fn cube_root_of_max() {
        let root = cube_root(u64::MAX);
        // The root of the largest `u64` is the largest value whose cube still
        // fits in a `u64`.
        assert_matches!(root.checked_pow(3), Some(_));
        assert_matches!((root + 1).checked_pow(3), None);
    }

    #[test]
    fn cube_root_rounds_down() {
        for root in 1..10_000u64 {
            let cube = root.pow(3);
            assert_eq!(cube_root(cube), root);
            assert_eq!(cube_root(cube - 1), root - 1);
            assert_eq!(cube_root(cube + 1), root);
        }
    }

    #[test]
    fn fixed_seconds_max_fits_largest_k() {
        // The largest K the implementation can calculate must be representable
        // without saturating, otherwise `FixedSeconds` would silently clamp it.
        assert!(FixedSeconds::cube_root_of_segments(u32::MAX) < FixedSeconds::MAX);
    }

    #[test_case(Duration::ZERO => 0)]
    #[test_case(Duration::from_secs(1) => 1024)]
    #[test_case(Duration::from_millis(100) => 102)]
    #[test_case(Duration::from_secs(10) => 10240)]
    #[test_case(Duration::MAX => FixedSeconds::MAX.get())]
    fn fixed_seconds_from_duration(duration: Duration) -> u64 {
        FixedSeconds::from(duration).get()
    }

    #[test_case(CUBIC_BETA, 100 => 70; "beta")]
    #[test_case(CUBIC_ALPHA, 17 => 9; "alpha")]
    #[test_case(CUBIC_ALPHA.inverse().unwrap(), 9 => 17; "inverse_alpha")]
    #[test_case(Ratio::ONE, 12345 => 12345; "one")]
    #[test_case(Ratio::new(1, 3).unwrap(), 10 => 3; "rounds_down")]
    #[test_case(TARGET_LIMIT, u32::MAX => u32::MAX; "saturates")]
    fn ratio_apply(ratio: Ratio, value: u32) -> u32 {
        ratio.multiply(value)
    }

    proptest! {
        #![proptest_config(Config {
            // Add all failed seeds here.
            failure_persistence: failed_seeds_no_std!(),
            ..Config::default()
        })]

        #[test]
        fn cubic_equations_match_floating_point(
            (cwnd_segments, w_max) in strategy::windows(),
            t in strategy::fixed_seconds(),
        ) {
            cubic_equations_match_floating_point_proptest(cwnd_segments, w_max, t)?
        }
    }

    // Verifies that the fixed point implementation of Figures 1 and 2 in
    // RFC 9438 matches their floating point counterparts.
    fn cubic_equations_match_floating_point_proptest(
        cwnd_segments: u32,
        w_max: u32,
        t: Duration,
    ) -> Result<(), TestCaseError> {
        // The value of a single unit of `FixedSeconds`, in seconds. Both `t`
        // and `K` are rounded down to a multiple of this.
        let resolution = 1.0 / f64::from(1u32 << FixedSeconds::FRACTION_BITS);
        let cubic_c = f64::from(CUBIC_C.numerator) / f64::from(CUBIC_C.denominator.get());

        let cubic = Cubic::<FakeInstant, false /* FAST_CONVERGENCE */> {
            w_max,
            k: FixedSeconds::cube_root_of_segments(w_max - cwnd_segments),
            ..Default::default()
        };

        // K = cube_root((w_max - cwnd_epoch) / C) (Fig. 2), truncated to the
        // fixed point resolution. `f64::cbrt` isn't guaranteed to be correctly
        // rounded, even for perfect cubes (e.g. glibc computes the cube root
        // of 27000 as 29.999999999999996 instead of 30), so allow the reference
        // value to be off by several ULPs. One ULP (unit in the last place) is
        // the gap between a floating point number and the next representable
        // one. The gap doubles at every power of two, so
        // `f64::EPSILON * expected_k` is one to two ULPs of `expected_k`.
        let expected_k = (f64::from(w_max - cwnd_segments) / cubic_c).cbrt();
        let tolerance = 8.0 * f64::EPSILON * expected_k;
        let k = cubic.k.get() as f64 * resolution;
        prop_assert!(
            (-tolerance..=resolution + tolerance).contains(&(expected_k - k)),
            "K: {k} is not within one tick below {expected_k}"
        );

        // W_cubic(t) = C*(t-K)^3 + W_max (Fig. 1).
        let w_cubic = |offset: f64| {
            (cubic_c * offset.powi(3) + f64::from(w_max)).clamp(0.0, f64::from(u32::MAX))
        };
        // `t` and `K` are each truncated to the fixed point resolution, so the
        // result must sit between the values the equation takes at the
        // extremes of that interval. The extra segment on either side accounts
        // for the result being a whole number of segments.
        let offset = t.as_secs_f64() - expected_k;
        let bounds = (w_cubic(offset - resolution) - 1.0)..=(w_cubic(offset + resolution) + 1.0);
        let got = f64::from(cubic.cubic_window(t));
        prop_assert!(bounds.contains(&got), "W_cubic({t:?}): {got} is not in {bounds:?}");
        Ok(())
    }

    // The following expectations are extracted from table. 1 and table. 2 in
    // RFC 9438 (https://www.rfc-editor.org/rfc/rfc9438#section-5.1). Note that
    // some numbers do not match as-is, but the error rate is acceptable (~2%),
    // this can be attributed to a few things, e.g., the way we simulate is
    // slightly different from the the ideal process, as we start the first
    // congestion avoidance with the convex region which grows pretty fast, also
    // the theoretical estimation is an approximation already. The theoretical
    // value is included in the name for each case.
    //
    // NB: Skip the tests with a loss_rate_reciprocal of 100_000_000 as they
    // take too long to run.
    #[test_case(Duration::from_millis(100), 100 => 11; "rtt=0.1 p=0.01 Wavg=12")]
    #[test_case(Duration::from_millis(100), 1_000 => 38; "rtt=0.1 p=0.001 Wavg=38")]
    #[test_case(Duration::from_millis(100), 10_000 => 187; "rtt=0.1 p=0.0001 Wavg=187")]
    #[test_case(Duration::from_millis(100), 100_000 => 1057; "rtt=0.1 p=0.00001 Wavg=1054")]
    #[test_case(Duration::from_millis(100), 1_000_000 => 5926; "rtt=0.1 p=0.000001 Wavg=5926")]
    #[test_case(Duration::from_millis(100), 10_000_000 => 33310; "rtt=0.1 p=0.0000001 Wavg=33325")]
    #[test_case(Duration::from_millis(10), 100 => 11; "rtt=0.01 p=0.01 Wavg=12")]
    #[test_case(Duration::from_millis(10), 1_000 => 37; "rtt=0.01 p=0.001 Wavg=38")]
    #[test_case(Duration::from_millis(10), 10_000 => 121; "rtt=0.01 p=0.0001 Wavg=120")]
    #[test_case(Duration::from_millis(10), 100_000 => 386; "rtt=0.01 p=0.00001 Wavg=379")]
    #[test_case(Duration::from_millis(10), 1_000_000 => 1261; "rtt=0.01 p=0.000001 Wavg=1200")]
    #[test_case(Duration::from_millis(10), 10_000_000 => 5931; "rtt=0.01 p=0.0000001 Wavg=5926")]
    fn average_window_size(rtt: Duration, loss_rate_reciprocal: u32) -> u32 {
        // Run the test long enough to experience 5 loss events.
        let round_trips = loss_rate_reciprocal * 5;

        // The theoretical predictions do not consider fast convergence,
        // disable it.
        let mut cubic = Cubic::<_, false /* FAST_CONVERGENCE */>::default();
        let mut params = CongestionControlParams::with_mss(DEFAULT_MSS);
        // The theoretical value is a prediction for the congestion avoidance
        // only, set ssthresh to 1 so that we skip slow start. Slow start can
        // grow the window size very quickly.
        params.ssthresh = 1;

        let mut clock = FakeInstantCtx::default();

        let mut avg_pkts = 0.0f64;
        let mut ack_cnt = 0;

        // We simulate a deterministic loss model, i.e., for loss_rate p, we
        // drop one packet for every 1/p packets.
        for _ in 0..round_trips {
            let cwnd = params.rounded_cwnd().cwnd();
            if ack_cnt >= loss_rate_reciprocal {
                ack_cnt -= loss_rate_reciprocal;
                let flight_size = params.cwnd;
                cubic.on_congestion_event(&mut params, CongestionEvent::PacketLoss, flight_size);
            } else {
                ack_cnt += cwnd / u32::from(params.mss);
                // On a true TCP connection, we'd get at least one ack for every
                // two segments. However, for the purpose of our simulation, we
                // pretend that a singular ACK arrives that ACKs all the sent
                // bytes (i.e. the whole `cwnd`). This allows us to speed up the
                // simulation, without changing the underlying math.
                cubic.on_ack_u32(&mut params, cwnd, clock.now(), rtt);
            }
            clock.sleep(rtt);
            // NB: Use f64, as f32 looses precision on the test cases with a
            // large number of round trips.
            avg_pkts += (cwnd as f64 / u32::from(params.mss) as f64) as f64 / round_trips as f64;
        }
        avg_pkts as u32
    }

    #[test]
    fn cubic_example() {
        let mut clock = FakeInstantCtx::default();
        let mut cubic = Cubic::<_, true /* FAST_CONVERGENCE */>::default();
        let mut params = CongestionControlParams::with_mss(DEFAULT_MSS);
        const RTT: Duration = Duration::from_millis(100);

        // Assert we have the correct initial window.
        assert_eq!(params.cwnd, 10 * u32::from(DEFAULT_MSS));

        // Slow start.
        clock.sleep(RTT);
        for _seg in 0..params.cwnd / u32::from(DEFAULT_MSS) {
            cubic.on_ack_u32(&mut params, u32::from(DEFAULT_MSS), clock.now(), RTT);
        }
        assert_eq!(params.cwnd, 20 * u32::from(DEFAULT_MSS));

        clock.sleep(RTT);
        let flight_size = params.cwnd;
        cubic.on_congestion_event(&mut params, CongestionEvent::Timeout, flight_size);
        assert_eq!(params.cwnd, u32::from(DEFAULT_MSS));

        // We are now back in slow start.
        for exp in 1..=3 {
            clock.sleep(RTT);
            for _ in 0..params.cwnd / u32::from(DEFAULT_MSS) {
                cubic.on_ack_u32(&mut params, u32::from(DEFAULT_MSS), clock.now(), RTT);
            }
            assert_eq!(params.cwnd, (1 << exp) * u32::from(DEFAULT_MSS));
        }

        // In this roundtrip, we enter a new congestion epoch from slow start,
        // in this round trip, both cubic and the reno-friendly window equations
        // will be reset, so the cwnd in this round trip will be ssthresh, which
        // is 7504 bytes, or 14 full sized segments.
        clock.sleep(RTT);
        for _seg in 0..params.cwnd / u32::from(DEFAULT_MSS) {
            cubic.on_ack_u32(&mut params, u32::from(DEFAULT_MSS), clock.now(), RTT);
        }
        assert_eq!(params.cwnd, 14 * u32::from(DEFAULT_MSS));

        // In the Reno-Friendly region, the cwnd is increased by alpha_cubic MSS
        // per cwnd of acked data. Since alpha_cubic is approximately 0.53, in
        // practice it takes 2 full RTT to observe this increase.
        for _ in 0..2 {
            clock.sleep(RTT);
            for _seg in 0..params.cwnd / u32::from(DEFAULT_MSS) {
                cubic.on_ack_u32(&mut params, u32::from(DEFAULT_MSS), clock.now(), RTT);
            }
        }
        assert_eq!(params.cwnd, 15 * u32::from(DEFAULT_MSS));
    }

    // This is a regression test for https://fxbug.dev/327628809.
    #[test_case(u32::MAX ; "cwnd is u32::MAX")]
    #[test_case(u32::MAX - 1; "cwnd is u32::MAX - 1")]
    fn repro_overflow_b327628809(cwnd: u32) {
        let clock = FakeInstantCtx::default();
        let mut cubic = Cubic::<_, true /* FAST_CONVERGENCE */>::default();
        let mut params = CongestionControlParams { ssthresh: 0, cwnd, mss: DEFAULT_MSS };
        const RTT: Duration = Duration::from_millis(100);

        cubic.on_ack(&mut params, NonZeroU32::MIN, clock.now(), RTT);
    }

    // This is a regression test for https://fxbug.dev/412748465.
    #[test]
    fn repro_overflow_b412748465() {
        let clock = FakeInstantCtx::default();
        let mut cubic = Cubic::<_, true /* FAST_CONVERGENCE */>::default();
        // Setup the params in slow start with `cwnd` close to overflow.
        let mut params =
            CongestionControlParams { ssthresh: u32::MAX, cwnd: u32::MAX - 1, mss: DEFAULT_MSS };
        const RTT: Duration = Duration::from_millis(100);
        // Ack enough bytes to push cwnd over u32::MAX.
        cubic.on_ack(
            &mut params,
            NonZeroU32::new(2).unwrap(), /*bytes_acked*/
            clock.now(),
            RTT,
        );
    }

    // Verify that the `flight_size` is used when updating congestion parameters
    // after a congestion event, rather than the `cwnd`.
    #[test_case(20, 20, 14; "same_as_cwnd")]
    #[test_case(20, 10, 7; "half_of_cwnd")]
    #[test_case(20, 0, 2; "saturates_to_min")]
    fn congestion_events_account_for_flight_size(cwnd: u32, flight_size: u32, expected_cwnd: u32) {
        let mut cubic = Cubic::<FakeInstant, true /* FAST_CONVERGENCE */>::default();
        let mss = u32::from(DEFAULT_MSS);

        let cwnd = cwnd * mss;
        let flight_size = flight_size * mss;
        let expected_cwnd = expected_cwnd * mss;

        let mut params = CongestionControlParams { ssthresh: 0, cwnd, mss: DEFAULT_MSS };
        cubic.on_congestion_event(&mut params, CongestionEvent::PacketLoss, flight_size);

        assert_eq!(params.ssthresh, expected_cwnd);
        assert_eq!(params.cwnd, expected_cwnd);
    }

    mod strategy {
        use core::time::Duration;

        use proptest::strategy::{Just, Strategy};

        use super::FixedSeconds;

        /// Generates values in `0..=max` spread evenly across orders of
        /// magnitude rather than across the range, so that small values are
        /// exercised as often as large ones.
        fn log_uniform(max: u64) -> impl Strategy<Value = u64> {
            (1..=u64::BITS - max.leading_zeros())
                .prop_flat_map(move |bits| 0..=max.min(u64::MAX >> (u64::BITS - bits)))
        }

        /// Generates `(cwnd_segments, w_max)` pairs where `cwnd_segments` is
        /// never larger than `w_max`.
        pub(super) fn windows() -> impl Strategy<Value = (u32, u32)> {
            log_uniform(u32::MAX.into()).prop_flat_map(|w_max| {
                let w_max = u32::try_from(w_max).unwrap();
                (0..=w_max, Just(w_max))
            })
        }

        /// Generates durations that [`FixedSeconds`] can represent.
        ///
        /// Durations are capped at [`FixedSeconds::MAX`], past which the fixed
        /// point implementation saturates and stops following the cubic
        /// function.
        pub(super) fn fixed_seconds() -> impl Strategy<Value = Duration> {
            const MILLIS_PER_SECOND: u64 = 1_000;
            let max_millis =
                (FixedSeconds::MAX.get() * MILLIS_PER_SECOND) >> FixedSeconds::FRACTION_BITS;
            log_uniform(max_millis).prop_map(Duration::from_millis)
        }
    }
}
