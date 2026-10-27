// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ops;

use affine::Ratio;
use zx_types::{ZX_TIME_INFINITE, ZX_TIME_INFINITE_PAST};

#[inline]
pub const fn zx_nsec(n: i64) -> i64 {
    n
}

#[inline]
pub const fn zx_usec(n: i64) -> i64 {
    n.saturating_mul(1_000)
}

#[inline]
pub const fn zx_msec(n: i64) -> i64 {
    n.saturating_mul(1_000_000)
}

#[inline]
pub const fn zx_sec(n: i64) -> i64 {
    n.saturating_mul(1_000_000_000)
}

#[inline]
pub const fn zx_min(n: i64) -> i64 {
    n.saturating_mul(60_000_000_000)
}

#[inline]
pub const fn zx_hour(n: i64) -> i64 {
    n.saturating_mul(3_600_000_000_000)
}

/// Monotonic timeline instant in nanoseconds.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstantMono(pub i64);

impl core::ops::Add<i64> for InstantMono {
    type Output = Self;

    #[inline]
    fn add(self, rhs: i64) -> Self {
        Self(self.0.saturating_add(rhs))
    }
}

impl core::ops::Add<DurationMono> for InstantMono {
    type Output = Self;

    #[inline]
    fn add(self, rhs: DurationMono) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }
}

impl core::ops::Sub<DurationMono> for InstantMono {
    type Output = Self;

    #[inline]
    fn sub(self, rhs: DurationMono) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }
}

impl core::ops::Sub<InstantMono> for InstantMono {
    type Output = DurationMono;

    #[inline]
    fn sub(self, rhs: InstantMono) -> DurationMono {
        DurationMono(self.0.saturating_sub(rhs.0))
    }
}

impl InstantMono {
    pub const ZERO: Self = Self(0);
    pub const INFINITE: Self = Self(ZX_TIME_INFINITE);
    pub const INFINITE_PAST: Self = Self(ZX_TIME_INFINITE_PAST);

    pub const fn from_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    pub const fn into_nanos(self) -> i64 {
        self.0
    }
}

/// Monotonic timeline instant in ticks.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstantMonoTicks(pub i64);

/// Boot timeline instant in nanoseconds.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstantBoot(pub i64);

impl ops::Sub<InstantBoot> for InstantBoot {
    type Output = DurationBoot;

    #[inline]
    fn sub(self, rhs: InstantBoot) -> DurationBoot {
        DurationBoot(self.0.saturating_sub(rhs.0))
    }
}

/// Boot timeline instant in ticks.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstantBootTicks(pub i64);

/// Monotonic timeline duration in nanoseconds.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DurationMono(pub i64);

impl core::ops::Add<DurationMono> for DurationMono {
    type Output = Self;

    #[inline]
    fn add(self, rhs: DurationMono) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }
}

impl core::ops::Sub<DurationMono> for DurationMono {
    type Output = Self;

    #[inline]
    fn sub(self, rhs: DurationMono) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }
}

impl core::ops::Mul<i64> for DurationMono {
    type Output = Self;

    #[inline]
    fn mul(self, rhs: i64) -> Self {
        Self(self.0.saturating_mul(rhs))
    }
}

impl DurationMono {
    pub const ZERO: Self = Self(0);
    pub const INFINITE: Self = Self(ZX_TIME_INFINITE);
    pub const INFINITE_PAST: Self = Self(ZX_TIME_INFINITE_PAST);

    /// Returns the number of nanoseconds contained by this `Duration`.
    pub const fn into_nanos(self) -> i64 {
        self.0
    }

    /// Returns the total number of whole microseconds contained by this `Duration`.
    pub const fn into_micros(self) -> i64 {
        self.0 / 1_000
    }

    /// Returns the total number of whole milliseconds contained by this `Duration`.
    pub const fn into_millis(self) -> i64 {
        self.into_micros() / 1_000
    }

    /// Returns the total number of whole seconds contained by this `Duration`.
    pub const fn into_seconds(self) -> i64 {
        self.into_millis() / 1_000
    }

    /// Returns the duration as a floating-point value in seconds.
    pub fn into_seconds_f64(self) -> f64 {
        self.into_nanos() as f64 / 1_000_000_000f64
    }

    /// Returns the total number of whole minutes contained by this `Duration`.
    pub const fn into_minutes(self) -> i64 {
        self.into_seconds() / 60
    }

    /// Returns the total number of whole hours contained by this `Duration`.
    pub const fn into_hours(self) -> i64 {
        self.into_minutes() / 60
    }

    pub const fn from_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    pub const fn from_micros(micros: i64) -> Self {
        Self(micros.saturating_mul(1_000))
    }

    pub const fn from_millis(millis: i64) -> Self {
        Self::from_micros(millis.saturating_mul(1_000))
    }

    pub const fn from_seconds(secs: i64) -> Self {
        Self::from_millis(secs.saturating_mul(1_000))
    }

    pub const fn from_minutes(min: i64) -> Self {
        Self::from_seconds(min.saturating_mul(60))
    }

    pub const fn from_hours(hours: i64) -> Self {
        Self::from_minutes(hours.saturating_mul(60))
    }
}

/// Monotonic timeline duration in ticks.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DurationMonoTicks(pub i64);

/// Boot timeline duration in nanoseconds.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DurationBoot(pub i64);

/// Boot timeline duration in ticks.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DurationBootTicks(pub i64);

// TODO(https://fxbug.dev/319935985): Not all locations are migrated to using specific timeline
// types, and some types (such as Deadline) are dangerously allowed to be instantiated on different
// timelines without capturing it in the type. For the moment we allow such behavior with these
// explicitly untyped types.

/// Instant in nanoseconds on an unknown timeline.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstantUnknown(pub i64);

impl From<InstantMono> for InstantUnknown {
    #[inline]
    fn from(val: InstantMono) -> Self {
        Self(val.0)
    }
}

impl From<InstantBoot> for InstantUnknown {
    #[inline]
    fn from(val: InstantBoot) -> Self {
        Self(val.0)
    }
}

impl core::ops::Sub<InstantUnknown> for InstantUnknown {
    type Output = DurationUnknown;

    /// Saturating difference between two instants, matching `zx_time_sub_time`.
    #[inline]
    fn sub(self, rhs: InstantUnknown) -> DurationUnknown {
        DurationUnknown(self.0.saturating_sub(rhs.0))
    }
}

/// Duration in nanoseconds on an unknown timeline.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DurationUnknown(pub i64);

impl From<DurationMono> for DurationUnknown {
    #[inline]
    fn from(val: DurationMono) -> Self {
        Self(val.0)
    }
}

impl From<DurationBoot> for DurationUnknown {
    #[inline]
    fn from(val: DurationBoot) -> Self {
        Self(val.0)
    }
}

/// CurrentTicksObservation contains a coherent read of the current ticks on both the monotonic and
/// boot timeline.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct CurrentTicksObservation {
    pub mono_now: i64,
    pub boot_now: i64,
}

zr::static_assert_size_and_align!(CurrentTicksObservation, 16, 8);

/// The `GetTicksSyncFlag` bits of `<platform/timer.h>`: which of the surrounding
/// memory accesses a raw ticks observation must be ordered against (`kNone`).
pub const GET_TICKS_SYNC_FLAG_NONE: u8 = 0;
/// `GetTicksSyncFlag::kAfterPreviousLoads`: order tick observation after previous loads.
pub const GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS: u8 = 1 << 0;
/// `GetTicksSyncFlag::kAfterPreviousStores`: order tick observation after previous stores.
pub const GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES: u8 = 1 << 1;
/// `GetTicksSyncFlag::kBeforeSubsequentLoads`: order tick observation before subsequent loads.
pub const GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS: u8 = 1 << 2;
/// `GetTicksSyncFlag::kBeforeSubsequentStores`: order tick observation before subsequent stores.
pub const GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES: u8 = 1 << 3;

unsafe extern "C" {
    fn cpp_timer_current_mono_ticks() -> InstantMonoTicks;
    fn cpp_timer_current_boot_ticks() -> InstantBootTicks;
    fn cpp_current_mono_time() -> InstantMono;
    fn cpp_current_boot_time() -> InstantBoot;
    fn cpp_ticks_per_second() -> i64;
    fn cpp_timer_get_ticks_to_time_ratio() -> Ratio;
    fn cpp_timer_get_mono_ticks_offset() -> i64;
    fn cpp_timer_get_boot_ticks_offset() -> i64;
    fn cpp_timer_current_mono_and_boot_ticks() -> CurrentTicksObservation;
    fn cpp_timer_set_ticks_to_time_ratio(ticks_to_time: Ratio);
    fn cpp_timer_set_initial_ticks(ticks: u64);
    fn cpp_timer_convert_mono_to_raw_ticks(
        mono_ticks: InstantMonoTicks,
        out_raw_ticks: *mut i64,
    ) -> bool;
    fn cpp_platform_set_oneshot_timer(deadline: i64) -> zx_types::zx_status_t;
    fn cpp_platform_stop_timer();
}

unsafe extern "C" {
    /// Runs the expired timers of the current CPU.
    ///
    /// # Safety
    ///
    /// Must be called from the timer interrupt, with interrupts disabled.
    pub fn timer_tick();
}

/// Returns the current monotonic time in ticks.
#[inline]
pub fn timer_current_mono_ticks() -> InstantMonoTicks {
    // SAFETY: Calling this FFI function has no preconditions and safely returns the platform timer
    // ticks.
    unsafe { cpp_timer_current_mono_ticks() }
}

/// High-precision timer ticks per second.
#[inline]
pub fn ticks_per_second() -> i64 {
    // SAFETY: Calling this FFI function has no preconditions and returns the configured platform
    // ticks per second.
    unsafe { cpp_ticks_per_second() }
}

/// Returns the current boot time in ticks.
#[inline]
pub fn timer_current_boot_ticks() -> InstantBootTicks {
    // SAFETY: Calling this FFI function has no preconditions and safely returns the platform timer
    // ticks.
    unsafe { cpp_timer_current_boot_ticks() }
}

/// Current monotonic time in nanoseconds.
#[inline]
pub fn current_mono_time() -> InstantMono {
    // SAFETY: Calling this FFI function has no preconditions and safely returns the platform
    // monotonic time.
    unsafe { cpp_current_mono_time() }
}

/// Current boot time in nanoseconds.
#[inline]
pub fn current_boot_time() -> InstantBoot {
    // SAFETY: Calling this FFI function has no preconditions and safely returns the platform boot
    // time.
    unsafe { cpp_current_boot_time() }
}

/// Returns the ratio between timer ticks and nanoseconds.
#[inline]
pub fn timer_get_ticks_to_time_ratio() -> Ratio {
    // SAFETY: Calling this FFI function has no preconditions and returns the globally configured
    // timer ticks to time ratio.
    unsafe { cpp_timer_get_ticks_to_time_ratio() }
}

/// The offset added to raw platform ticks to put them on the monotonic ticks timeline.
#[inline]
pub fn timer_get_mono_ticks_offset() -> i64 {
    // SAFETY: Calling this FFI function has no preconditions and safely returns the offset.
    unsafe { cpp_timer_get_mono_ticks_offset() }
}

/// Access the offset from the raw ticks timeline to the boot ticks timeline.
#[inline]
pub fn timer_get_boot_ticks_offset() -> i64 {
    // SAFETY: Calling this FFI function has no preconditions; it just loads an atomic.
    unsafe { cpp_timer_get_boot_ticks_offset() }
}

/// Performs a coherent read of both the mono and boot timelines and returns a tuple with the
/// current value of both timelines.
#[inline]
pub fn timer_current_mono_and_boot_ticks() -> CurrentTicksObservation {
    // SAFETY: Calling this FFI function has no preconditions and safely returns the current ticks
    // observation.
    unsafe { cpp_timer_current_mono_and_boot_ticks() }
}

/// Sets the ratio between timer ticks and nanoseconds.
///
/// # Safety
///
/// Must be called exactly once, on the boot CPU with interrupts disabled,
/// before secondary CPUs are started and before anything converts ticks to
/// time.
#[inline]
pub unsafe fn timer_set_ticks_to_time_ratio(ticks_to_time: Ratio) {
    // SAFETY: the caller upholds the one-time, single-CPU contract above, so
    // nothing reads the ratio while it is written.
    unsafe { cpp_timer_set_ticks_to_time_ratio(ticks_to_time) }
}

/// Sets the raw ticks value at which the boot and monotonic timelines start.
///
/// # Safety
///
/// Must be called on the boot CPU with interrupts disabled, before secondary
/// CPUs are started.
#[inline]
pub unsafe fn timer_set_initial_ticks(ticks: u64) {
    // SAFETY: the caller upholds the single-CPU contract above.
    unsafe { cpp_timer_set_initial_ticks(ticks) }
}

/// Converts a ticks value on the monotonic timeline to a raw hardware ticks value.
///
/// Returns the raw ticks value if the monotonic clock is not paused, and `None` if it is.
#[inline]
pub fn timer_convert_mono_to_raw_ticks(mono_ticks: InstantMonoTicks) -> Option<i64> {
    let mut raw_ticks: i64 = 0;
    // SAFETY: `raw_ticks` is a valid, writable `i64`, and the function otherwise only reads the
    // atomic monotonic ticks modifier.
    let has_value = unsafe { cpp_timer_convert_mono_to_raw_ticks(mono_ticks, &raw mut raw_ticks) };
    has_value.then_some(raw_ticks)
}

/// Sets the calling CPU's hardware oneshot timer, which is responsible for calling `timer_tick()`
/// when it fires, to the raw ticks value `deadline`.
#[inline]
pub fn platform_set_oneshot_timer(deadline: i64) -> Result<(), zx_status::Status> {
    // SAFETY: Calling this FFI function has no memory-safety preconditions; it only programs the
    // calling CPU's platform timer.
    zx_status::Status::ok(unsafe { cpp_platform_set_oneshot_timer(deadline) })
}

/// Clears the calling CPU's hardware oneshot timer.
#[inline]
pub fn platform_stop_timer() {
    // SAFETY: Calling this FFI function has no memory-safety preconditions; it only stops the
    // calling CPU's platform timer.
    unsafe { cpp_platform_stop_timer() }
}

/// Platform timer tests.
#[cfg(ktest)]
#[unittest::suite(name = "platform_timer")]
mod tests {
    use super::{DurationMono, InstantMono};
    use unittest::assert_true;

    /// Test time ordering and equality.
    #[test]
    fn test_time_ordering_and_equality() {
        assert_true!(DurationMono(10) < DurationMono(20));
        assert_true!(DurationMono(15) == DurationMono(15));
        assert_true!(InstantMono(100) < InstantMono(200));
        assert_true!(InstantMono(100) + 50 == InstantMono(150));
        assert_true!(InstantMono(100) + DurationMono(50) == InstantMono(150));
    }

    /// Test saturating addition overflow on DurationMono.
    #[test]
    fn test_duration_mono_add_overflow() {
        assert_true!(DurationMono::INFINITE + DurationMono(1) == DurationMono::INFINITE);
        assert_true!(DurationMono(i64::MAX - 10) + DurationMono(20) == DurationMono::INFINITE);
        assert_true!(DurationMono(i64::MAX - 1) + DurationMono(1) == DurationMono::INFINITE);

        assert_true!(DurationMono::INFINITE_PAST + DurationMono(-1) == DurationMono::INFINITE_PAST);
        assert_true!(
            DurationMono(i64::MIN + 10) + DurationMono(-20) == DurationMono::INFINITE_PAST
        );
        assert_true!(DurationMono(i64::MIN + 1) + DurationMono(-1) == DurationMono::INFINITE_PAST);
    }

    /// Test saturating subtraction overflow on DurationMono.
    #[test]
    fn test_duration_mono_sub_overflow() {
        assert_true!(DurationMono::INFINITE - DurationMono(-1) == DurationMono::INFINITE);
        assert_true!(DurationMono(i64::MAX - 10) - DurationMono(-20) == DurationMono::INFINITE);
        assert_true!(DurationMono(i64::MAX - 1) - DurationMono(-1) == DurationMono::INFINITE);

        assert_true!(DurationMono::INFINITE_PAST - DurationMono(1) == DurationMono::INFINITE_PAST);
        assert_true!(DurationMono(i64::MIN + 10) - DurationMono(20) == DurationMono::INFINITE_PAST);
        assert_true!(DurationMono(i64::MIN + 1) - DurationMono(1) == DurationMono::INFINITE_PAST);
    }

    /// Test saturating multiplication overflow on DurationMono.
    #[test]
    fn test_duration_mono_mul_overflow() {
        // Positive * positive overflow
        assert_true!(DurationMono::INFINITE * 2 == DurationMono::INFINITE);
        assert_true!(DurationMono(i64::MAX / 2 + 1) * 2 == DurationMono::INFINITE);

        // Negative * negative overflow
        assert_true!(DurationMono::INFINITE_PAST * -1 == DurationMono::INFINITE);
        assert_true!(DurationMono::INFINITE_PAST * -2 == DurationMono::INFINITE);

        // Positive * negative underflow
        assert_true!(DurationMono::INFINITE * -2 == DurationMono::INFINITE_PAST);
        assert_true!(DurationMono(i64::MAX / 2 + 2) * -2 == DurationMono::INFINITE_PAST);

        // Negative * positive underflow
        assert_true!(DurationMono::INFINITE_PAST * 2 == DurationMono::INFINITE_PAST);
        assert_true!(DurationMono(i64::MIN / 2 - 1) * 2 == DurationMono::INFINITE_PAST);
    }

    /// Test saturating conversion from microseconds.
    #[test]
    fn test_duration_mono_from_micros_overflow() {
        let max_valid = i64::MAX / 1_000;
        assert_true!(DurationMono::from_micros(max_valid) == DurationMono(max_valid * 1_000));
        assert_true!(DurationMono::from_micros(max_valid + 1) == DurationMono::INFINITE);
        assert_true!(DurationMono::from_micros(i64::MAX) == DurationMono::INFINITE);

        let min_valid = i64::MIN / 1_000;
        assert_true!(DurationMono::from_micros(min_valid) == DurationMono(min_valid * 1_000));
        assert_true!(DurationMono::from_micros(min_valid - 1) == DurationMono::INFINITE_PAST);
        assert_true!(DurationMono::from_micros(i64::MIN) == DurationMono::INFINITE_PAST);
    }

    /// Test saturating conversion from milliseconds.
    #[test]
    fn test_duration_mono_from_millis_overflow() {
        let max_valid = i64::MAX / 1_000_000;
        assert_true!(DurationMono::from_millis(max_valid) == DurationMono(max_valid * 1_000_000));
        assert_true!(DurationMono::from_millis(max_valid + 1) == DurationMono::INFINITE);
        assert_true!(DurationMono::from_millis(i64::MAX / 1_000 + 1) == DurationMono::INFINITE);
        assert_true!(DurationMono::from_millis(i64::MAX) == DurationMono::INFINITE);

        let min_valid = i64::MIN / 1_000_000;
        assert_true!(DurationMono::from_millis(min_valid) == DurationMono(min_valid * 1_000_000));
        assert_true!(DurationMono::from_millis(min_valid - 1) == DurationMono::INFINITE_PAST);
        assert_true!(
            DurationMono::from_millis(i64::MIN / 1_000 - 1) == DurationMono::INFINITE_PAST
        );
        assert_true!(DurationMono::from_millis(i64::MIN) == DurationMono::INFINITE_PAST);
    }

    /// Test saturating conversion from seconds.
    #[test]
    fn test_duration_mono_from_seconds_overflow() {
        let max_valid = i64::MAX / 1_000_000_000;
        assert_true!(
            DurationMono::from_seconds(max_valid) == DurationMono(max_valid * 1_000_000_000)
        );
        assert_true!(DurationMono::from_seconds(max_valid + 1) == DurationMono::INFINITE);
        assert_true!(DurationMono::from_seconds(i64::MAX) == DurationMono::INFINITE);

        let min_valid = i64::MIN / 1_000_000_000;
        assert_true!(
            DurationMono::from_seconds(min_valid) == DurationMono(min_valid * 1_000_000_000)
        );
        assert_true!(DurationMono::from_seconds(min_valid - 1) == DurationMono::INFINITE_PAST);
        assert_true!(DurationMono::from_seconds(i64::MIN) == DurationMono::INFINITE_PAST);
    }

    /// Test saturating conversion from minutes.
    #[test]
    fn test_duration_mono_from_minutes_overflow() {
        const NANOS_PER_MIN: i64 = 60_000_000_000;
        let max_valid = i64::MAX / NANOS_PER_MIN;
        assert_true!(
            DurationMono::from_minutes(max_valid) == DurationMono(max_valid * NANOS_PER_MIN)
        );
        assert_true!(DurationMono::from_minutes(max_valid + 1) == DurationMono::INFINITE);
        assert_true!(DurationMono::from_minutes(i64::MAX) == DurationMono::INFINITE);

        let min_valid = i64::MIN / NANOS_PER_MIN;
        assert_true!(
            DurationMono::from_minutes(min_valid) == DurationMono(min_valid * NANOS_PER_MIN)
        );
        assert_true!(DurationMono::from_minutes(min_valid - 1) == DurationMono::INFINITE_PAST);
        assert_true!(DurationMono::from_minutes(i64::MIN) == DurationMono::INFINITE_PAST);
    }

    /// Test saturating conversion from hours.
    #[test]
    fn test_duration_mono_from_hours_overflow() {
        const NANOS_PER_HOUR: i64 = 3_600_000_000_000;
        let max_valid = i64::MAX / NANOS_PER_HOUR;
        assert_true!(
            DurationMono::from_hours(max_valid) == DurationMono(max_valid * NANOS_PER_HOUR)
        );
        assert_true!(DurationMono::from_hours(max_valid + 1) == DurationMono::INFINITE);
        assert_true!(DurationMono::from_hours(i64::MAX) == DurationMono::INFINITE);

        let min_valid = i64::MIN / NANOS_PER_HOUR;
        assert_true!(
            DurationMono::from_hours(min_valid) == DurationMono(min_valid * NANOS_PER_HOUR)
        );
        assert_true!(DurationMono::from_hours(min_valid - 1) == DurationMono::INFINITE_PAST);
        assert_true!(DurationMono::from_hours(i64::MIN) == DurationMono::INFINITE_PAST);
    }

    /// Test saturating arithmetic on InstantMono with DurationMono and InstantMono.
    #[test]
    fn test_instant_mono_overflow() {
        // InstantMono + i64
        assert_true!(InstantMono::INFINITE + 1 == InstantMono::INFINITE);
        assert_true!(InstantMono::INFINITE_PAST + (-1) == InstantMono::INFINITE_PAST);

        // InstantMono + DurationMono
        assert_true!(InstantMono::INFINITE + DurationMono(1) == InstantMono::INFINITE);
        assert_true!(InstantMono::INFINITE_PAST + DurationMono(-1) == InstantMono::INFINITE_PAST);

        // InstantMono - DurationMono
        assert_true!(InstantMono::INFINITE - DurationMono(-1) == InstantMono::INFINITE);
        assert_true!(InstantMono::INFINITE_PAST - DurationMono(1) == InstantMono::INFINITE_PAST);

        // InstantMono - InstantMono -> DurationMono
        assert_true!(InstantMono::INFINITE - InstantMono(-1) == DurationMono::INFINITE);
        assert_true!(InstantMono::INFINITE_PAST - InstantMono(1) == DurationMono::INFINITE_PAST);
    }
}
