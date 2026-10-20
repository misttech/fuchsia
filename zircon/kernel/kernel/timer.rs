// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_char;
use core::marker::{PhantomData, PhantomPinned};
use core::pin::Pin;
use pin_init::pin_data;
use zr::OpaqueBytes;

pub use super::percpu::TimerQueue;

#[allow(improper_ctypes)]
unsafe extern "C" {
    fn cpp_timer_init(timer: *mut core::ffi::c_void, clock_id: u32);
    fn cpp_timer_destroy(timer: *mut Timer);
    fn cpp_timer_set(
        timer: *mut Timer,
        deadline: *const Deadline,
        callback: Callback,
        arg: *mut core::ffi::c_void,
    );
    fn cpp_timer_cancel(timer: *mut Timer) -> bool;
    fn cpp_timer_trylock_or_cancel(
        timer: *mut Timer,
        lock: *const crate::kconcurrent::ChainLock,
    ) -> zx_types::zx_status_t;
    fn cpp_timer_queue_print_timer_queues(buf: *mut c_char, len: usize);
}

pub const ZX_CLOCK_MONOTONIC: u32 = 0;
pub const ZX_CLOCK_BOOT: u32 = 1;

use super::deadline::Deadline;
use crate::platform_rs::timer::InstantUnknown;

pub type Callback = unsafe extern "C" fn(timer: *mut Timer, now: i64, arg: *mut core::ffi::c_void);

const TIMER_SIZE: usize = 72;

#[pin_data(PinnedDrop)]
#[repr(C)]
#[repr(align(8))]
pub struct Timer {
    _opaque: OpaqueBytes<TIMER_SIZE>,
    _marker: PhantomData<PhantomPinned>,
}

impl Timer {
    #[inline]
    pub fn init(clock_id: u32) -> impl pin_init::PinInit<Self, core::convert::Infallible> {
        zr::pin_init_ffi!(cpp_timer_init, clock_id)
    }

    /// Returns a raw mutable pointer to this [`Timer`] via its interior-mutable `OpaqueBytes`.
    #[inline]
    fn as_raw_mut(&self) -> *mut Self {
        self._opaque.get().cast()
    }

    /// Schedules the timer to fire at the specified deadline.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `self` remains pinned in memory and is not dropped, moved, or deallocated while
    ///   scheduled; [`Self::cancel`] must be called before `self` is dropped if it may still be
    ///   scheduled or executing its callback.
    /// - `callback` is a valid FFI callback that does not cause undefined behavior when executed.
    /// - `arg` is either a valid pointer or a null pointer, and remains valid to pass to
    ///   `callback` until either `callback` completes or [`Self::cancel`] returns.
    /// - Neither [`Self::set_deadline`], [`Self::set_oneshot`], nor [`Self::cancel`] is called
    ///   concurrently on `self` (except from within `callback` itself).
    #[inline]
    pub unsafe fn set_deadline(
        self: Pin<&Self>,
        deadline: &Deadline,
        callback: Callback,
        arg: *mut core::ffi::c_void,
    ) {
        // SAFETY: `self.as_raw_mut()` and `deadline` are valid pointers, and the caller upholds
        // the pinning, lifetime, callback, and synchronization requirements.
        unsafe {
            cpp_timer_set(self.as_raw_mut(), core::ptr::from_ref(deadline), callback, arg);
        }
    }

    /// Schedules the timer to fire once at the specified deadline (in nanoseconds).
    ///
    /// # Safety
    ///
    /// Same as [`Self::set_deadline`].
    #[inline]
    pub unsafe fn set_oneshot(
        self: Pin<&Self>,
        deadline: i64,
        callback: Callback,
        arg: *mut core::ffi::c_void,
    ) {
        let dl = Deadline::no_slack(InstantUnknown(deadline));
        // SAFETY: The caller guarantees the safety preconditions of `set_deadline`.
        unsafe {
            self.set_deadline(&dl, callback, arg);
        }
    }

    #[inline]
    pub fn cancel(&self) -> bool {
        // SAFETY: `self.as_raw_mut()` is a valid pointer to an initialized `Timer`, and C++
        // `Timer::Cancel()` is internally synchronized via `TimerLock`.
        unsafe { cpp_timer_cancel(self.as_raw_mut()) }
    }

    /// Special helper routine to simultaneously try to acquire a
    /// [`ChainLock`](crate::kconcurrent::ChainLock) and check for timer cancel, which is needed in
    /// a few special cases. Returns `Ok(())` if `lock` was acquired, or `Err(Status::TIMED_OUT)`
    /// if the timer was canceled.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - This method is called from within `self`'s own currently executing timer callback.
    /// - A [`ChainLockTransaction`](crate::kconcurrent::ChainLockTransaction) is currently active
    ///   on the current CPU and has not yet been finalized.
    /// - `lock` is not already held by the current transaction.
    /// - If `Ok(())` is returned, the caller assumes responsibility for releasing `lock`.
    #[inline]
    pub unsafe fn trylock_or_cancel(
        &self,
        lock: &crate::kconcurrent::ChainLock,
    ) -> Result<(), zx_status::Status> {
        // SAFETY: `self.as_raw_mut()` and `lock` are valid pointers, and the caller guarantees
        // this is called from `self`'s timer callback within an active, unfinalized
        // `ChainLockTransaction` that does not already hold `lock`.
        unsafe { zx_status::Status::ok(cpp_timer_trylock_or_cancel(self.as_raw_mut(), lock)) }
    }
}

zr::unsafe_pinned_drop_ffi!(Timer, cpp_timer_destroy);

// SAFETY: A Timer can be sent across threads as its underlying C++ object is thread-safe
// (protected by TimerLock internally).
unsafe impl Send for Timer {}

zr::static_assert!(core::mem::size_of::<Timer>() == 72);
zr::static_assert!(core::mem::align_of::<Timer>() == 8);

impl TimerQueue {
    /// Prints the contents of all timer queues into `buf` and null-terminates `buf`.
    pub fn print_timer_queues(buf: &mut [u8]) {
        // SAFETY: `buf.as_mut_ptr()` is valid for `buf.len()` bytes.
        unsafe { cpp_timer_queue_print_timer_queues(buf.as_mut_ptr().cast(), buf.len()) };
    }
}

/// # Safety
///
/// Nothing: `argv` is never dereferenced. The function is `unsafe` only to match the console
/// command signature.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_timers(
    _argc: core::ffi::c_int,
    _argv: *const crate::console::CmdArgs,
    _flags: u32,
) -> core::ffi::c_int {
    use core::ffi::CStr;
    use kprint::kprint;
    use page::SIZE as PAGE_SIZE;
    use zx_status::Status;

    let timer_buffer_size = PAGE_SIZE;

    // allocate a buffer to dump the timer queue into to avoid reentrancy issues with the
    // timer spinlock
    let Ok(mut buf) = kalloc::Box::<[u8]>::try_new_zeroed_slice(timer_buffer_size) else {
        return Status::NO_MEMORY.into_raw();
    };

    TimerQueue::print_timer_queues(&mut buf);

    // SAFETY: `TimerQueue::print_timer_queues` null-terminates `buf`.
    kprint!("{:s}", unsafe { CStr::from_ptr(buf.as_ptr().cast()) });

    drop(buf);

    0
}

#[cfg(console_enabled)]
crate::console::static_command!(
    CMD_TIMERS,
    c"timers".as_ptr(),
    c"dump the current kernel timer queues".as_ptr(),
    cmd_timers,
    crate::console::CMD_AVAIL_NORMAL
);

/// Kernel timer tests.
#[cfg(ktest)]
#[unittest::suite(name = "timer_rust")]
mod tests {
    use crate::kernel::deadline::{SlackMode, TimerSlack};
    use crate::platform_rs::timer::{
        CurrentTicksObservation, DurationMono, DurationUnknown, current_boot_time,
        current_mono_time, timer_current_boot_ticks, timer_current_mono_and_boot_ticks,
        timer_current_mono_ticks, zx_hour, zx_msec, zx_usec,
    };
    use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    use pin_init::stack_pin_init;
    use unittest::{assert_eq, assert_false, assert_ge, assert_le, assert_true};
    use zx_types::ZX_TIME_INFINITE;

    #[derive(Default)]
    struct TimerArgs {
        result: AtomicBool,
        timer_fired: AtomicBool,
        remaining: AtomicI32,
    }

    unsafe extern "C" fn oneshot_cb(_timer: *mut Timer, _now: i64, arg: *mut core::ffi::c_void) {
        // SAFETY: arg is a valid pointer to AtomicBool.
        let fired = unsafe { &*(arg as *const AtomicBool) };
        fired.store(true, Ordering::Release);
    }

    unsafe extern "C" fn timer_cb(_timer: *mut Timer, _now: i64, void_arg: *mut core::ffi::c_void) {
        // SAFETY: `void_arg` points to a valid `TimerArgs` for the duration of the timer.
        let arg = unsafe { &*(void_arg as *const TimerArgs) };
        arg.timer_fired.store(true, Ordering::SeqCst);
    }

    unsafe extern "C" fn timer_cancel_cb(
        t: *mut Timer,
        _now: i64,
        void_arg: *mut core::ffi::c_void,
    ) {
        // SAFETY: `void_arg` points to a valid `TimerArgs` and `t` points to a valid pinned
        // `Timer`.
        let arg = unsafe { &*(void_arg as *const TimerArgs) };
        // SAFETY: `t` is a valid pinned `Timer` for the duration of the callback.
        let t = unsafe { &*t };
        arg.result.store(t.cancel(), Ordering::SeqCst);
        arg.timer_fired.store(true, Ordering::SeqCst);
    }

    unsafe extern "C" fn timer_set_cb(t: *mut Timer, _now: i64, void_arg: *mut core::ffi::c_void) {
        // SAFETY: `void_arg` points to a valid `TimerArgs` for the duration of the timer.
        let arg = unsafe { &*(void_arg as *const TimerArgs) };
        if arg.remaining.fetch_sub(1, Ordering::SeqCst) >= 1 {
            let deadline = Deadline::after_mono(DurationMono(zx_usec(10)), TimerSlack::none());
            // SAFETY: `t` is a valid pinned `Timer` and `void_arg` remains valid until `t` is
            // cancelled at the end of `set_from_callback`.
            unsafe {
                let t = core::pin::Pin::new_unchecked(&*t);
                t.set_deadline(&deadline, timer_set_cb, void_arg);
            }
        }
    }

    /// Verifies that a scheduled oneshot timer fires and executes its callback.
    #[test]
    fn test_timer_oneshot() {
        stack_pin_init!(let timer = Timer::init(ZX_CLOCK_MONOTONIC));
        let fired = AtomicBool::new(false);
        let arg = &fired as *const AtomicBool as *mut core::ffi::c_void;

        // Schedule timer to fire in 1ms (1,000,000 ns) from now.
        let now = current_mono_time().0;
        // SAFETY: oneshot_cb and arg are valid.
        unsafe {
            timer.as_ref().set_oneshot(now + 1_000_000, oneshot_cb, arg);
        }

        // Poll up to 100ms (100 iterations of 1ms sleep) for the timer to fire.
        let mut success = false;
        for _ in 0..100 {
            if fired.load(Ordering::Acquire) {
                success = true;
                break;
            }
            let _ = crate::kernel::thread::sleep_relative(DurationMono(1_000_000)); // 1ms
        }

        unittest::expect_true!(success);
    }

    /// Verifies that a scheduled timer can be successfully cancelled.
    #[test]
    fn test_timer_cancel() {
        stack_pin_init!(let timer = Timer::init(ZX_CLOCK_MONOTONIC));
        let fired = AtomicBool::new(false);
        let arg = &fired as *const AtomicBool as *mut core::ffi::c_void;

        // Schedule timer to fire in 10 seconds (in the future).
        let now = current_mono_time().0;
        // SAFETY: oneshot_cb and arg are valid.
        unsafe {
            timer.as_ref().set_oneshot(now + 10_000_000_000, oneshot_cb, arg);
        }

        // Cancel it immediately.
        let cancelled = timer.cancel();
        // Cancel should return true.
        unittest::expect_true!(cancelled);

        // Sleep for a short duration to ensure no callback is running.
        let _ = crate::kernel::thread::sleep_relative(DurationMono(1_000_000)); // 1ms

        // Verify that it did not fire.
        unittest::expect_false!(fired.load(Ordering::Acquire));
    }

    // Set a timer and cancel it before the deadline has elapsed.
    /// cancel_before_deadline
    #[test]
    fn cancel_before_deadline() {
        let arg = TimerArgs::default();
        stack_pin_init!(let t = Timer::init(ZX_CLOCK_MONOTONIC));
        let deadline = Deadline::after_mono(DurationMono(zx_hour(5)), TimerSlack::none());
        // SAFETY: `timer_cb` and `&arg` are valid while `t` is active.
        unsafe {
            t.as_ref().set_deadline(&deadline, timer_cb, (&raw const arg).cast_mut().cast());
        }
        assert_true!(t.cancel());
        assert_false!(arg.timer_fired.load(Ordering::SeqCst));
    }

    // Set a timer and cancel it after it has fired.
    /// cancel_after_fired
    #[test]
    fn cancel_after_fired() {
        let arg = TimerArgs::default();
        stack_pin_init!(let t = Timer::init(ZX_CLOCK_MONOTONIC));
        let deadline = Deadline::no_slack(current_mono_time().into());
        // SAFETY: `timer_cb` and `&arg` are valid while `t` is active.
        unsafe {
            t.as_ref().set_deadline(&deadline, timer_cb, (&raw const arg).cast_mut().cast());
        }
        while !arg.timer_fired.load(Ordering::SeqCst) {
            core::hint::spin_loop();
        }
        assert_false!(t.cancel());
    }

    // Set a timer and cancel it from its own callback.
    /// cancel_from_callback
    #[test]
    fn cancel_from_callback() {
        let arg = TimerArgs { result: AtomicBool::new(true), ..Default::default() };
        stack_pin_init!(let t = Timer::init(ZX_CLOCK_MONOTONIC));
        let deadline = Deadline::no_slack(current_mono_time().into());
        // SAFETY: `timer_cancel_cb` and `&arg` are valid while `t` is active.
        unsafe {
            t.as_ref().set_deadline(&deadline, timer_cancel_cb, (&raw const arg).cast_mut().cast());
        }
        while !arg.timer_fired.load(Ordering::SeqCst) {
            core::hint::spin_loop();
        }
        assert_false!(t.cancel());
        assert_false!(arg.result.load(Ordering::SeqCst));
    }

    // Set a timer that re-sets itself from its own callback.
    /// set_from_callback
    #[test]
    fn set_from_callback() {
        let arg = TimerArgs { remaining: AtomicI32::new(5), ..Default::default() };
        stack_pin_init!(let t = Timer::init(ZX_CLOCK_MONOTONIC));
        let deadline = Deadline::no_slack(current_mono_time().into());
        // SAFETY: `timer_set_cb` and `&arg` are valid while `t` is active.
        unsafe {
            t.as_ref().set_deadline(&deadline, timer_set_cb, (&raw const arg).cast_mut().cast());
        }
        while arg.remaining.load(Ordering::SeqCst) > 0 {
            core::hint::spin_loop();
        }

        // We cannot assert the return value below because we don't know if the last timer has
        // fired.
        t.cancel();
    }

    /// Deadline::after
    #[test]
    fn deadline_after() {
        let slack_modes: [Option<TimerSlack>; 5] = [
            None,                     // None is used for testing the default mode ("none").
            Some(TimerSlack::none()), // an explicit test of "none"
            Some(TimerSlack::new(DurationUnknown(zx_usec(100)), SlackMode::Center)),
            Some(TimerSlack::new(DurationUnknown(zx_usec(200)), SlackMode::Early)),
            Some(TimerSlack::new(DurationUnknown(zx_usec(200)), SlackMode::Late)),
        ];

        // Test to make sure that a relative timeout which is an infinite amount of
        // time from now produces an infinite deadline.
        for slack in &slack_modes {
            let deadline = match slack {
                Some(s) => Deadline::after_mono(DurationMono(ZX_TIME_INFINITE), *s),
                None => Deadline::after_mono(DurationMono(ZX_TIME_INFINITE), TimerSlack::none()),
            };
            assert_eq!(ZX_TIME_INFINITE, deadline.when().0);

            // Default slack should be "none"
            let expected = slack.unwrap_or_else(TimerSlack::none);
            assert_eq!(expected.amount().0, deadline.slack().amount().0);
            assert_eq!(expected.mode(), deadline.slack().mode());
        }

        // While we cannot control the precise deadline which will be produced from
        // our call to Deadline::after, we _can_ bound the range it might exist in.
        // Test for this as well.
        for slack in &slack_modes {
            const TIMEOUT: DurationMono = DurationMono(zx_msec(10));
            let before = current_mono_time() + TIMEOUT;
            let deadline = match slack {
                Some(s) => Deadline::after_mono(TIMEOUT, *s),
                None => Deadline::after_mono(TIMEOUT, TimerSlack::none()),
            };
            let after = current_mono_time() + TIMEOUT;
            assert_le!(before.0, deadline.when().0);
            assert_ge!(after.0, deadline.when().0);

            // Default slack should be "none"
            let expected = slack.unwrap_or_else(TimerSlack::none);
            assert_eq!(expected.amount().0, deadline.slack().amount().0);
            assert_eq!(expected.mode(), deadline.slack().mode());
        }
    }

    /// test_timer_current_mono_and_boot_ticks
    #[test]
    fn test_timer_current_mono_and_boot_ticks() {
        // Get the current monotonic and boot ticks. This should occur prior to our observation of
        // both below, providing us with a lower bound on those values.
        let boot_before = timer_current_boot_ticks();
        let mono_before = timer_current_mono_ticks();

        // Perform a synchronized read of the monotonic and boot ticks.
        let obs: CurrentTicksObservation = timer_current_mono_and_boot_ticks();

        // Get the current monotonic and boot ticks. This should occur after our observation of
        // both above, providing us with an upper bound on those values.
        let mono_after = timer_current_mono_ticks();
        let boot_after = timer_current_boot_ticks();

        // Ensure that the monotonic ticks are less than or equal to the boot ticks.
        assert_le!(obs.mono_now, obs.boot_now);

        // Ensure that our observations are monotonic, meaning that they are greater than or equal
        // to our before observations and less than or equal to our after observations.
        assert_ge!(obs.mono_now, mono_before.0);
        assert_ge!(obs.boot_now, boot_before.0);
        assert_le!(obs.mono_now, mono_after.0);
        assert_le!(obs.boot_now, boot_after.0);
    }

    // Ensure that a boot timer fires.
    /// boot_timer
    #[test]
    fn boot_timer() {
        let arg = TimerArgs::default();
        stack_pin_init!(let t = Timer::init(ZX_CLOCK_BOOT));
        let deadline = Deadline::no_slack(current_boot_time().into());
        // SAFETY: `timer_cb` and `&arg` are valid while `t` is active.
        unsafe {
            t.as_ref().set_deadline(&deadline, timer_cb, (&raw const arg).cast_mut().cast());
        }
        while !arg.timer_fired.load(Ordering::SeqCst) {
            core::hint::spin_loop();
        }
    }
}
