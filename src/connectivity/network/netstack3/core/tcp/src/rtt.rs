// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! TCP RTT estimation per [RFC 6298](https://tools.ietf.org/html/rfc6298).
use core::num::NonZeroU32;
use core::ops::Range;
use core::time::Duration;

use netstack3_base::{Instant, SeqNum};

const ONE: NonZeroU32 = NonZeroU32::new(1).unwrap();

#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(super) enum Estimator {
    NoSample,
    Measured {
        /// The smoothed round-trip time.
        srtt: Duration,
        /// The round-trip time variation.
        rtt_var: Duration,
    },
}

impl Default for Estimator {
    fn default() -> Self {
        Self::NoSample
    }
}

impl Estimator {
    /// The following constants are defined in [RFC 6298 Section 2]:
    ///
    /// [RFC 6298 Section 2]: https://tools.ietf.org/html/rfc6298#section-2
    const K: u32 = 4;
    const G: Duration = Duration::from_millis(100);

    /// Updates the estimates with a newly sampled RTT.
    pub(super) fn sample(&mut self, rtt: Duration, samples_per_round_trip: NonZeroU32) {
        match self {
            Self::NoSample => {
                // Per RFC 6298 section 2,
                //   When the first RTT measurement R is made, the host MUST set
                //   SRTT <- R
                //   RTTVAR <- R/2
                *self = Self::Measured { srtt: rtt, rtt_var: rtt / 2 }
            }
            Self::Measured { srtt, rtt_var } => {
                // Per RFC 6298 section 2,
                //   When a subsequent RTT measurement R' is made, a host MUST set
                //     RTTVAR <- (1 - beta) * RTTVAR + beta * |SRTT - R'|
                //     SRTT <- (1 - alpha) * SRTT + alpha * R'
                //   ...
                //   The above SHOULD be computed using alpha=1/8 and beta=1/4.
                //
                // Per RFC 7323 Appendix G, when taking N RTT samples per round trip,
                // the weights alpha and beta should be scaled by 1/N to maintain
                // roughly the same historical smoothing across one RTT. This
                // scaling yields alpha' and beta'.
                let alpha_prime_reciprocal = 8 * samples_per_round_trip.get();
                let beta_prime_reciprocal = 4 * samples_per_round_trip.get();
                let diff = srtt.checked_sub(rtt).unwrap_or_else(|| rtt - *srtt);
                // Using fixed point integer division below rather than using
                // floating points just to define the exact constants.
                *rtt_var =
                    ((*rtt_var * (beta_prime_reciprocal - 1)) + diff) / beta_prime_reciprocal;
                *srtt = ((*srtt * (alpha_prime_reciprocal - 1)) + rtt) / alpha_prime_reciprocal;
            }
        }
    }

    /// Returns the current retransmission timeout.
    pub(super) fn rto(&self) -> Rto {
        //   Until a round-trip time (RTT) measurement has been made for a
        //   segment sent between the sender and receiver, the sender SHOULD
        //   set RTO <- 1 second;
        //   ...
        //   RTO <- SRTT + max (G, K*RTTVAR)
        match *self {
            Estimator::NoSample => Rto::DEFAULT,
            Estimator::Measured { srtt, rtt_var } => {
                // `Duration::MAX` is 2^64 seconds which is about 6 * 10^11
                // years. If the following expression panics due to overflow,
                // we must have some serious errors in the estimator itself.
                Rto::new(srtt + Self::G.max(rtt_var * Self::K))
            }
        }
    }

    pub(super) fn srtt(&self) -> Option<Duration> {
        match self {
            Self::NoSample => None,
            Self::Measured { srtt, rtt_var: _ } => Some(*srtt),
        }
    }

    pub(super) fn rtt_var(&self) -> Option<Duration> {
        match self {
            Self::NoSample => None,
            Self::Measured { srtt: _, rtt_var } => Some(*rtt_var),
        }
    }
}

/// A retransmit timeout value.
///
/// This type serves as a witness for a valid retransmit timeout value that is
/// clamped to the interval `[Rto::MIN, Rto::MAX]`. It can be transformed into a
/// [`Duration`].
#[derive(Debug, Eq, PartialEq, PartialOrd, Ord, Copy, Clone)]
pub(super) struct Rto(Duration);

impl Rto {
    /// The minimum retransmit timeout value.
    ///
    /// [RFC 6298 Section 2] states:
    ///
    /// > Whenever RTO is computed, if it is less than 1 second, then the RTO
    /// > SHOULD be rounded up to 1 second. [...] Therefore, this specification
    /// > requires a large minimum RTO as a conservative approach, while at the
    /// > same time acknowledging that at some future point, research may show
    /// > that a smaller minimum RTO is acceptable or superior.
    ///
    /// We hard code the default value used by [Linux] here.
    ///
    /// [RFC 6298 Section 2]: https://datatracker.ietf.org/doc/html/rfc6298#section-2
    /// [Linux]: https://github.com/torvalds/linux/blob/4701f33a10702d5fc577c32434eb62adde0a1ae1/include/net/tcp.h#L148
    pub(super) const MIN: Rto = Rto(Duration::from_millis(200));

    /// The maximum retransmit timeout value.
    ///
    /// [RFC 67298 Section 2] states:
    ///
    /// > (2.5) A maximum value MAY be placed on RTO provided it is at least 60
    /// > seconds.
    ///
    /// We hard code the default value used by [Linux] here.
    ///
    /// [RFC 6298 Section 2]: https://datatracker.ietf.org/doc/html/rfc6298#section-2
    /// [Linux]: https://github.com/torvalds/linux/blob/4701f33a10702d5fc577c32434eb62adde0a1ae1/include/net/tcp.h#L147
    pub(super) const MAX: Rto = Rto(Duration::from_secs(120));

    /// The default RTO value.
    pub(super) const DEFAULT: Rto = Rto(Duration::from_secs(1));

    /// Creates a new [`Rto`] by clamping `duration` to the allowed range.
    pub(super) fn new(duration: Duration) -> Self {
        Self(duration).clamp(Self::MIN, Self::MAX)
    }

    pub(super) fn get(&self) -> Duration {
        let Self(inner) = self;
        *inner
    }

    /// Returns the result of doubling this RTO value and saturating to the
    /// valid range.
    pub(super) fn double(&self) -> Self {
        let Self(d) = self;
        Self(d.saturating_mul(2)).min(Self::MAX)
    }
}

impl From<Rto> for Duration {
    fn from(Rto(value): Rto) -> Self {
        value
    }
}

impl Default for Rto {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A RTT sampler that collects samples by measuring the time between sending
/// a segment and receiving the ACK for that segment.
#[derive(Debug, Default, Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(super) enum MeasuredSampler<I> {
    #[default]
    NotTracking,
    Tracking {
        range: Range<SeqNum>,
        timestamp: I,
    },
}

impl<I> MeasuredSampler<I> {
    /// Returns the number of samples per round trip collected by this sampler.
    #[inline(always)]
    fn samples_per_round_trip() -> NonZeroU32 {
        ONE
    }
}

impl<I: Instant> MeasuredSampler<I> {
    /// Updates the `MeasuredSampler` with a new segment that is about to be sent.
    ///
    /// - `now` is the current timestamp.
    /// - `range` is the sequence number range in the newly produced segment.
    /// - `snd_max` is the SND.MAX value *not considering* the new segment in `range`.
    pub(super) fn on_will_send_segment(&mut self, now: I, range: Range<SeqNum>, snd_max: SeqNum) {
        match self {
            Self::NotTracking => {
                // If we're currently not tracking any segments, we can consider
                // this segment for RTT IFF at least part of `range` is new
                // bytes.
                if !range.end.after(snd_max) {
                    return;
                }
                // The segment could be partially retransmitting some data, so
                // the left edge of our tracking must be the latest between the
                // start and snd_max.
                let start = if range.start.before(snd_max) { snd_max } else { range.start };
                *self = Self::Tracking { range: start..range.end, timestamp: now }
            }
            Self::Tracking { range: tracking, timestamp: _ } => {
                // We need to discard this tracking segment if we retransmit
                // anything prior to the right edge of the tracked segment.
                if range.start.before(tracking.end) {
                    *self = Self::NotTracking;
                }
            }
        }
    }

    /// Updates the `MeasuredSampler` with a new ack that arrived for the connection.
    ///
    /// - `now` is the current timestamp.
    /// - `ack` is the acknowledgement number in the ACK segment.
    ///
    /// If the sampler was able to produce a new RTT sample, `Some` is returned.
    ///
    /// This function assumes that `ack` is a valid ACK number and is within the
    /// window the sender is expecting to receive (i.e. it's not an ACK for data
    /// we did not send).
    fn on_ack(&mut self, now: I, ack: SeqNum) -> Option<Duration> {
        match self {
            Self::NotTracking => None,
            Self::Tracking { range, timestamp } => {
                if ack.after(range.start) {
                    // Any acknowledgement that is at or after the tracked range
                    // is a valid rtt sample.
                    let rtt = now.saturating_duration_since(*timestamp);
                    // Segment has been acked, we're not going to be tracking it
                    // anymore.
                    *self = Self::NotTracking;
                    Some(rtt)
                } else {
                    None
                }
            }
        }
    }
}

/// The strategy used for sampling RTT measurements on a connection.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(super) enum SamplingStrategy<I> {
    /// Sample the RTT once per round trip by tracking when an individual
    /// segment is sent & ACKed.
    Measured(MeasuredSampler<I>),
}

impl<I> Default for SamplingStrategy<I> {
    fn default() -> Self {
        Self::Measured(MeasuredSampler::default())
    }
}

impl<I> SamplingStrategy<I> {
    /// Returns the number of samples collected per round trip by this strategy.
    pub(super) fn samples_per_round_trip(&self) -> NonZeroU32 {
        match self {
            Self::Measured(_) => MeasuredSampler::<I>::samples_per_round_trip(),
        }
    }
}

impl<I: Instant> SamplingStrategy<I> {
    /// Updates the sampler with a new segment that is about to be sent.
    pub(super) fn on_will_send_segment(&mut self, now: I, range: Range<SeqNum>, snd_max: SeqNum) {
        match self {
            Self::Measured(sampler) => sampler.on_will_send_segment(now, range, snd_max),
        }
    }

    /// Updates the sampler with a new ack that arrived for the connection.
    pub(super) fn on_ack(&mut self, now: I, ack: SeqNum) -> Option<Duration> {
        match self {
            Self::Measured(sampler) => sampler.on_ack(now, ack),
        }
    }
}

impl<I> From<MeasuredSampler<I>> for SamplingStrategy<I> {
    fn from(sampler: MeasuredSampler<I>) -> Self {
        Self::Measured(sampler)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use netstack3_base::testutil::FakeInstant;
    use test_case::test_case;

    impl MeasuredSampler<FakeInstant> {
        fn from_range(Range { start, end }: Range<u32>) -> Self {
            Self::Tracking {
                range: SeqNum::new(start)..SeqNum::new(end),
                timestamp: FakeInstant::default(),
            }
        }
    }

    #[test_case(
        Estimator::NoSample,
        Duration::from_secs(2),
        NonZeroU32::new(1).unwrap()
        => Estimator::Measured {
            srtt: Duration::from_secs(2),
            rtt_var: Duration::from_secs(1)
        };
        "first_sample"
    )]
    #[test_case(
        Estimator::Measured {
            srtt: Duration::from_secs(1),
            rtt_var: Duration::from_secs(1)
        },
        Duration::from_secs(2),
        NonZeroU32::new(1).unwrap()
        => Estimator::Measured {
            srtt: Duration::from_millis(1125),
            rtt_var: Duration::from_secs(1)
        };
        "different_sample_changes_srtt"
    )]
    #[test_case(
        Estimator::Measured {
            srtt: Duration::from_secs(1),
            rtt_var: Duration::from_secs(2)
        },
        Duration::from_secs(1),
        NonZeroU32::new(1).unwrap()
        => Estimator::Measured {
            srtt: Duration::from_secs(1),
            rtt_var: Duration::from_millis(1500)
        };
        "same_sample_changes_rtt_var"
    )]
    #[test_case(
        Estimator::Measured {
            srtt: Duration::from_secs(1),
            rtt_var: Duration::from_secs(1)
        },
        Duration::from_secs(2),
        NonZeroU32::new(2).unwrap()
        => Estimator::Measured {
            srtt: Duration::from_micros(1062500),
            rtt_var: Duration::from_millis(1000)
        };
        "multiple_samples_per_round_trip_scales_decay"
    )]

    fn sample_rtt(
        mut estimator: Estimator,
        rtt: Duration,
        samples_per_round_trip: NonZeroU32,
    ) -> Estimator {
        estimator.sample(rtt, samples_per_round_trip);
        estimator
    }

    #[test_case(Estimator::NoSample => Rto::DEFAULT.get())]
    #[test_case(Estimator::Measured {
        srtt: Duration::from_secs(1),
        rtt_var: Duration::from_secs(2),
    } => Duration::from_secs(9))]
    fn calculate_rto(estimator: Estimator) -> Duration {
        estimator.rto().get()
    }

    // Useful for representing wrapping-around TCP seqnum ranges.
    #[allow(clippy::reversed_empty_ranges)]
    #[test_case(
        MeasuredSampler::NotTracking, 1..10, 1 => MeasuredSampler::from_range(1..10)
        ; "segment after SND.MAX"
    )]
    #[test_case(
        MeasuredSampler::NotTracking, 1..10, 10 => MeasuredSampler::NotTracking
        ; "segment before SND.MAX"
    )]
    #[test_case(
        MeasuredSampler::NotTracking, 1..10, 5 => MeasuredSampler::from_range(5..10)
        ; "segment contains SND.MAX"
    )]
    #[test_case(
        MeasuredSampler::from_range(1..10), 10..20, 10 => MeasuredSampler::from_range(1..10)
        ; "send further segments"
    )]
    #[test_case(
        MeasuredSampler::from_range(10..20), 1..10, 20 => MeasuredSampler::NotTracking
        ; "retransmit prior segments"
    )]
    #[test_case(
        MeasuredSampler::from_range(1..10), 1..10, 10 => MeasuredSampler::NotTracking
        ; "retransmit same segment"
    )]
    #[test_case(
        MeasuredSampler::from_range(1..10), 5..15, 15 => MeasuredSampler::NotTracking
        ; "retransmit same partial 1"
    )]
    #[test_case(
        MeasuredSampler::from_range(10..20), 5..15, 20 => MeasuredSampler::NotTracking
        ; "retransmit same partial 2"
    )]
    #[test_case(
        MeasuredSampler::NotTracking, (u32::MAX - 5)..5,
        u32::MAX - 5 => MeasuredSampler::from_range((u32::MAX - 5)..5)
        ; "SND.MAX wraparound good"
    )]
    #[test_case(
        MeasuredSampler::NotTracking, (u32::MAX - 5)..5,
        5 => MeasuredSampler::NotTracking
        ; "SND.MAX wraparound retransmit not tracking"
    )]
    #[test_case(
        MeasuredSampler::from_range(u32::MAX - 5..5), (u32::MAX - 5)..5,
        5 => MeasuredSampler::NotTracking
        ; "SND.MAX wraparound retransmit tracking"
    )]
    #[test_case(
        MeasuredSampler::NotTracking, (u32::MAX - 5)..5, u32::MAX => MeasuredSampler::from_range(u32::MAX..5)
        ; "SND.MAX wraparound partial 1"
    )]
    #[test_case(
        MeasuredSampler::NotTracking, (u32::MAX - 5)..5, 1 => MeasuredSampler::from_range(1..5)
        ; "SND.MAX wraparound partial 2"
    )]
    fn measured_sampler_on_segment(
        mut sampler: MeasuredSampler<FakeInstant>,
        range: Range<u32>,
        snd_max: u32,
    ) -> MeasuredSampler<FakeInstant> {
        sampler.on_will_send_segment(
            FakeInstant::default(),
            SeqNum::new(range.start)..SeqNum::new(range.end),
            SeqNum::new(snd_max),
        );
        sampler
    }

    const ACK_DELAY: Duration = Duration::from_millis(10);

    #[test_case(
        MeasuredSampler::NotTracking, 10 => (None, MeasuredSampler::NotTracking)
        ; "not tracking"
    )]
    #[test_case(
        MeasuredSampler::from_range(1..10), 10 => (Some(ACK_DELAY), MeasuredSampler::NotTracking)
        ; "ack segment"
    )]
    #[test_case(
        MeasuredSampler::from_range(1..10), 20 => (Some(ACK_DELAY), MeasuredSampler::NotTracking)
        ; "ack after"
    )]
    #[test_case(
        MeasuredSampler::from_range(10..20), 9 => (None, MeasuredSampler::from_range(10..20))
        ; "ack before 1"
    )]
    #[test_case(
        MeasuredSampler::from_range(10..20), 10 => (None, MeasuredSampler::from_range(10..20))
        ; "ack before 2"
    )]
    #[test_case(
        MeasuredSampler::from_range(10..20), 11 => (Some(ACK_DELAY), MeasuredSampler::NotTracking)
        ; "ack single"
    )]
    #[test_case(
        MeasuredSampler::from_range(10..20), 15 => (Some(ACK_DELAY), MeasuredSampler::NotTracking)
        ; "ack partial"
    )]
    fn measured_sampler_on_ack(
        mut sampler: MeasuredSampler<FakeInstant>,
        ack: u32,
    ) -> (Option<Duration>, MeasuredSampler<FakeInstant>) {
        let res = sampler.on_ack(FakeInstant::default() + ACK_DELAY, SeqNum::new(ack));
        (res, sampler)
    }
}
