// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// Monotonic timeline instant in nanoseconds.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstantMono(pub i64);

impl core::ops::Add<i64> for InstantMono {
    type Output = Self;

    #[inline]
    fn add(self, rhs: i64) -> Self {
        Self(self.0 + rhs)
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

/// Boot timeline instant in ticks.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstantBootTicks(pub i64);

/// Monotonic timeline duration in nanoseconds.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DurationMono(pub i64);

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

/// Duration in nanoseconds on an unknown timeline.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DurationUnknown(pub i64);

unsafe extern "C" {
    fn cpp_timer_current_mono_ticks() -> InstantMonoTicks;
    fn cpp_timer_current_boot_ticks() -> InstantBootTicks;
    fn cpp_current_mono_time() -> InstantMono;
    fn cpp_current_boot_time() -> InstantBoot;
}

/// Returns the current monotonic time in ticks.
#[inline]
pub fn timer_current_mono_ticks() -> InstantMonoTicks {
    // SAFETY: Calling this FFI function has no preconditions and safely returns the platform timer
    // ticks.
    unsafe { cpp_timer_current_mono_ticks() }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_time_ordering_and_equality() {
        assert!(DurationMono(10) < DurationMono(20));
        assert_eq!(DurationMono(15), DurationMono(15));
        assert!(InstantMono(100) < InstantMono(200));
        assert_eq!(InstantMono(100) + 50, InstantMono(150));
    }
}
