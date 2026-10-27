// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! RAII helpers for managing preemption, eager rescheduling, and timeslice extensions.

use core::marker::PhantomData;

use super::thread::{
    PreemptDisabledToken, eager_resched_disable, eager_resched_reenable,
    preempt_clear_timeslice_extension, preempt_disable, preempt_reenable,
    preempt_set_timeslice_extension,
};
use crate::platform_rs::timer::DurationMono;

/// `AutoPreemptDisabler` is a RAII helper that automatically manages disabling and
/// re-enabling preemption. When the object goes out of scope, it automatically
/// re-enables preemption if it had been previously disabled by the instance.
///
/// This guard is `!Send` and `!Sync` because preemption state is CPU- and thread-local.
///
/// Example usage:
///
/// ```ignore
/// // Immediately disable preemption, then obtain the list lock and append an
/// // element to the list.
/// {
///     let _preempt_disabler = AutoPreemptDisabler::new();
///     let mut guard = lock.lock();
///     guard.push_back(element);
/// }
///
/// // Reserve the option to disable preemption, but do not do so right now.
/// {
///     let mut preempt_disabler = AutoPreemptDisabler::new_deferred();
///     let mut guard = lock.lock();
///
///     // Do some work.
///
///     if predicate() {
///         preempt_disabler.disable();
///         // Do some more work with preemption disabled.
///     }
/// } // `guard` is dropped first, then (if `predicate()` was true), preemption is re-enabled.
/// ```
pub struct AutoPreemptDisabler {
    disabled: bool,
    _marker: PhantomData<*mut ()>,
}

impl AutoPreemptDisabler {
    /// Creates a new guard and immediately disables preemption.
    #[inline]
    pub fn new() -> Self {
        preempt_disable();
        Self { disabled: true, _marker: PhantomData }
    }

    /// Creates a new guard without preemption initially disabled.
    #[inline]
    pub fn new_deferred() -> Self {
        Self { disabled: false, _marker: PhantomData }
    }

    /// Disables preemption if it was not disabled by this instance already.
    #[inline]
    pub fn disable(&mut self) {
        if !self.disabled {
            preempt_disable();
            self.disabled = true;
        }
    }

    /// Enables preemption if it was previously disabled by this instance.
    #[inline]
    pub fn enable(&mut self) {
        if self.disabled {
            // SAFETY: `self.disabled` records that this guard's own `preempt_disable()` is still
            // unmatched, and this guard type never mints a `PreemptDisabledToken`.
            unsafe { preempt_reenable() };
            self.disabled = false;
        }
    }

    /// Asserts in debug builds that preemption is currently disabled by this guard instance.
    #[inline]
    pub fn assert_disabled(&self) {
        debug_assert!(self.disabled);
    }
}

impl Default for AutoPreemptDisabler {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AutoPreemptDisabler {
    #[inline]
    fn drop(&mut self) {
        self.enable();
    }
}

/// `AnnotatedAutoPreemptDisabler` is an RAII helper which is almost identical in
/// functionality to [`AutoPreemptDisabler`]. In C++, this variant carries a `TA_SCOPED_CAP`
/// thread-safety annotation (`preempt_disabled_token`), which has no direct effect on the Rust
/// struct itself; in Rust, callers can borrow a [`PreemptDisabledToken`] via [`Self::token`]
/// where proof that preemption has been disabled is required.
///
/// This guard is `!Send` and `!Sync` because preemption state is CPU- and thread-local.
pub struct AnnotatedAutoPreemptDisabler {
    disabled: bool,
    token: PreemptDisabledToken,
}

impl AnnotatedAutoPreemptDisabler {
    /// Creates a new guard and immediately disables preemption.
    #[inline]
    pub fn new() -> Self {
        preempt_disable();
        // SAFETY: `preempt_disable()` was just called above, and `Self::token` only hands out
        // a shared reference while `self.disabled` is true.
        Self { disabled: true, token: unsafe { PreemptDisabledToken::new() } }
    }

    /// Returns a capability token proving that preemption is disabled while this guard remains
    /// borrowed and has not re-enabled preemption.
    ///
    /// # Panics
    ///
    /// Panics if [`Self::enable`] has already been called on this guard.
    #[inline]
    pub fn token(&self) -> &PreemptDisabledToken {
        assert!(self.disabled);
        &self.token
    }

    /// Enables preemption if it was previously disabled by this instance.
    #[inline]
    pub fn enable(&mut self) {
        if self.disabled {
            // SAFETY: `self.disabled` records that this guard's own `preempt_disable()` is still
            // unmatched, and the `&mut self` borrow guarantees that no `&PreemptDisabledToken`
            // handed out by `Self::token(&self)` is still live.
            unsafe { preempt_reenable() };
            self.disabled = false;
        }
    }
}

impl Default for AnnotatedAutoPreemptDisabler {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AnnotatedAutoPreemptDisabler {
    #[inline]
    fn drop(&mut self) {
        self.enable();
    }
}

/// `AutoEagerReschedDisabler` is a RAII helper that automatically manages
/// disabling and re-enabling eager reschedules, including both local and remote
/// CPUs. This type works the same as [`AutoPreemptDisabler`], except that it also
/// prevents sending reschedule IPIs until eager reschedules are re-enabled.
///
/// This guard is `!Send` and `!Sync` because preemption state is CPU- and thread-local.
pub struct AutoEagerReschedDisabler {
    disabled: bool,
    _marker: PhantomData<*mut ()>,
}

impl AutoEagerReschedDisabler {
    /// Creates a new guard and immediately disables eager rescheduling.
    #[inline]
    pub fn new() -> Self {
        eager_resched_disable();
        Self { disabled: true, _marker: PhantomData }
    }

    /// Creates a new guard without eager reschedules initially disabled.
    #[inline]
    pub fn new_deferred() -> Self {
        Self { disabled: false, _marker: PhantomData }
    }

    /// Disables eager rescheduling if it was not disabled by this instance already.
    #[inline]
    pub fn disable(&mut self) {
        if !self.disabled {
            eager_resched_disable();
            self.disabled = true;
        }
    }

    /// Enables eager rescheduling if it was previously disabled by this instance.
    #[inline]
    pub fn enable(&mut self) {
        if self.disabled {
            // SAFETY: `self.disabled` records that this guard's own `eager_resched_disable()` is
            // still unmatched, and this guard type never mints a `PreemptDisabledToken`.
            unsafe { eager_resched_reenable() };
            self.disabled = false;
        }
    }
}

impl Default for AutoEagerReschedDisabler {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AutoEagerReschedDisabler {
    #[inline]
    fn drop(&mut self) {
        self.enable();
    }
}

/// `AnnotatedAutoEagerReschedDisabler` is an RAII helper which is almost identical in
/// functionality to [`AutoEagerReschedDisabler`]. In C++, this variant carries a `TA_SCOPED_CAP`
/// thread-safety annotation (`preempt_disabled_token`), which has no direct effect on the Rust
/// struct itself; in Rust, callers can borrow a [`PreemptDisabledToken`] via [`Self::token`]
/// where proof that preemption has been disabled is required.
///
/// This guard is `!Send` and `!Sync` because preemption state is CPU- and thread-local.
pub struct AnnotatedAutoEagerReschedDisabler {
    disabled: bool,
    token: PreemptDisabledToken,
}

impl AnnotatedAutoEagerReschedDisabler {
    /// Creates a new guard and immediately disables eager rescheduling.
    #[inline]
    pub fn new() -> Self {
        eager_resched_disable();
        // SAFETY: `eager_resched_disable()` disables preemption, and `Self::token` only hands out
        // a shared reference while `self.disabled` is true.
        Self { disabled: true, token: unsafe { PreemptDisabledToken::new() } }
    }

    /// Returns a capability token proving that preemption is disabled while this guard remains
    /// borrowed and has not re-enabled eager rescheduling.
    ///
    /// # Panics
    ///
    /// Panics if [`Self::enable`] has already been called on this guard.
    #[inline]
    pub fn token(&self) -> &PreemptDisabledToken {
        assert!(self.disabled);
        &self.token
    }

    /// Enables eager rescheduling if it was previously disabled by this instance.
    #[inline]
    pub fn enable(&mut self) {
        if self.disabled {
            // SAFETY: `self.disabled` records that this guard's own `eager_resched_disable()` is
            // still unmatched, and the `&mut self` borrow guarantees that no
            // `&PreemptDisabledToken` handed out by `Self::token(&self)` is still live.
            unsafe { eager_resched_reenable() };
            self.disabled = false;
        }
    }
}

impl Default for AnnotatedAutoEagerReschedDisabler {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AnnotatedAutoEagerReschedDisabler {
    #[inline]
    fn drop(&mut self) {
        self.enable();
    }
}

/// `AutoExpiringPreemptDisabler` is an RAII helper that defers preemption of the
/// current thread until either `max_deferral_duration` nanoseconds after
/// preemption is requested or the object is destroyed, whichever comes first.
///
/// This guard is `!Send` and `!Sync` because timeslice extensions modify CPU- and thread-local
/// state.
pub struct AutoExpiringPreemptDisabler {
    should_clear: bool,
    _marker: PhantomData<*mut ()>,
}

impl AutoExpiringPreemptDisabler {
    /// Default timeslice extension duration (150us).
    pub const DEFAULT_TIMESLICE_EXTENSION: DurationMono = DurationMono::from_micros(150);

    /// Creates a new guard and attempts to set a timeslice extension for `max_deferral_duration`.
    #[inline]
    pub fn new(max_deferral_duration: DurationMono) -> Self {
        let should_clear = preempt_set_timeslice_extension(max_deferral_duration);
        Self { should_clear, _marker: PhantomData }
    }

    /// Creates a new guard with the default timeslice extension duration.
    #[inline]
    pub fn with_default_timeslice_extension() -> Self {
        Self::new(Self::DEFAULT_TIMESLICE_EXTENSION)
    }
}

impl Drop for AutoExpiringPreemptDisabler {
    #[inline]
    fn drop(&mut self) {
        if self.should_clear {
            preempt_clear_timeslice_extension();
        }
    }
}

/// Unit tests for preemption and eager-reschedule disabler RAII guards.
#[cfg(ktest)]
#[unittest::suite(name = "auto_preempt_disabler_rust")]
mod tests {
    use crate::kernel::thread::{
        eager_resched_disable_count, preempt_disable_count, preempt_is_enabled,
    };
    use unittest::{assert_eq, assert_false, assert_true};

    /// Verifies preemption and eager-reschedule disabler RAII guards.
    #[test]
    fn test_auto_preempt_disabler() {
        // Make sure that nothing funny is going on with our preempt disable count as
        // it stands now.
        assert_eq!(0, preempt_disable_count());
        assert_eq!(0, eager_resched_disable_count());

        {
            // Create a disabler inside of a scope, but do not have it immediately
            // request that preemption be disabled. Our count should still be zero.
            let mut ap_disabler = AutoPreemptDisabler::new_deferred();
            assert_eq!(0, preempt_disable_count());

            // Now explicitly disable. Our count should go to 1.
            ap_disabler.disable();
            ap_disabler.assert_disabled();
            assert_eq!(1, preempt_disable_count());

            // Do it again, our count should remain at 1.
            ap_disabler.disable();
            assert_eq!(1, preempt_disable_count());

            {
                // Make another inside of a new scope. Our count should remain at 1 until
                // we explicitly use the new instance to disable preemption.
                let mut ap_disabler2 = AutoPreemptDisabler::new_deferred();
                assert_eq!(1, preempt_disable_count());

                ap_disabler2.disable();
                assert_eq!(2, preempt_disable_count());
            } // Let it go out of scope, we should drop down to a count of 1.

            assert_eq!(1, preempt_disable_count());
        } // Allow the original to go out of scope. This should get us back down to a count of 0.

        assert_eq!(0, preempt_disable_count());

        // Next, do a similar test, but this time with the version which automatically
        // begins life with preemption disabled.
        {
            let mut ap_disabler = AutoPreemptDisabler::new();
            ap_disabler.assert_disabled();
            assert_eq!(1, preempt_disable_count());

            // Attempting to call disable should do nothing.
            ap_disabler.disable();
            assert_eq!(1, preempt_disable_count());

            {
                // Add a second. Watch the count go up as it comes into scope, and back
                // down again when it goes out.
                let _ap_disabler2 = AutoPreemptDisabler::default();
                assert_eq!(2, preempt_disable_count());
            }

            assert_eq!(1, preempt_disable_count());
        } // Allow the original to go out of scope. This should get us back down to a count of 0.

        assert_eq!(0, preempt_disable_count());

        // Test an explicit Enable
        {
            let mut ap_disabler = AutoPreemptDisabler::new();
            assert_eq!(1, preempt_disable_count());

            {
                // Create a deferred disabler, and test that Enabling it before it is disabled
                // does nothing.
                let mut ap_disabler2 = AutoPreemptDisabler::new_deferred();
                assert_eq!(1, preempt_disable_count());

                ap_disabler2.enable();
                assert_eq!(1, preempt_disable_count());
            }
            assert_eq!(1, preempt_disable_count());

            // Should be able to toggle enable and disable
            ap_disabler.enable();
            assert_eq!(0, preempt_disable_count());
            ap_disabler.disable();
            assert_eq!(1, preempt_disable_count());
            // Ending on Enable should result in no change after the disabler goes out of scope.
            ap_disabler.enable();
            assert_eq!(0, preempt_disable_count());
        }

        assert_eq!(0, preempt_disable_count());

        // Verify AnnotatedAutoPreemptDisabler.
        {
            let mut aap_disabler = AnnotatedAutoPreemptDisabler::default();
            assert_eq!(1, preempt_disable_count());
            aap_disabler.enable();
            assert_eq!(0, preempt_disable_count());
            aap_disabler.enable();
            assert_eq!(0, preempt_disable_count());
        }
        assert_eq!(0, preempt_disable_count());

        // Verify AutoEagerReschedDisabler and AnnotatedAutoEagerReschedDisabler.
        {
            let mut er_disabler = AutoEagerReschedDisabler::new_deferred();
            assert_eq!(0, eager_resched_disable_count());
            er_disabler.enable();
            assert_eq!(0, eager_resched_disable_count());

            er_disabler.disable();
            assert_eq!(1, eager_resched_disable_count());
            er_disabler.disable();
            assert_eq!(1, eager_resched_disable_count());

            {
                let _er_disabler2 = AutoEagerReschedDisabler::default();
                assert_eq!(2, eager_resched_disable_count());

                let mut aer_disabler = AnnotatedAutoEagerReschedDisabler::default();
                assert_eq!(3, eager_resched_disable_count());
                aer_disabler.enable();
                assert_eq!(2, eager_resched_disable_count());
                aer_disabler.enable();
                assert_eq!(2, eager_resched_disable_count());
            }
            assert_eq!(1, eager_resched_disable_count());
        }
        assert_eq!(0, eager_resched_disable_count());
    }

    /// Verifies `AutoExpiringPreemptDisabler` basic and nested timeslice extensions.
    #[test]
    fn test_auto_timeslice_extension() {
        // Basic.
        {
            assert_true!(preempt_is_enabled());
            {
                let _guard = AutoExpiringPreemptDisabler::new(DurationMono::INFINITE);
                assert_false!(preempt_is_enabled());
            }
            assert_true!(preempt_is_enabled());
        }

        // Nested. Only the outermost guard matters.
        {
            assert_true!(preempt_is_enabled());
            {
                let _guard1 = AutoExpiringPreemptDisabler::new(DurationMono::INFINITE);
                assert_false!(preempt_is_enabled());
                {
                    let _guard2 = AutoExpiringPreemptDisabler::new(DurationMono::from_nanos(0));
                    // Even though guard2's duration is 0, preemption should still be disabled
                    // because of guard1's extension.
                    assert_false!(preempt_is_enabled());
                }
                assert_false!(preempt_is_enabled());
            }
            assert_true!(preempt_is_enabled());
        }
    }
}
