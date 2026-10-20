// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::{Callback, SuspendWakeupTimer};
use crate::platform_rs::timer::InstantBoot;
use core::mem::MaybeUninit;
use core::ptr::NonNull;

#[cfg(target_arch = "aarch64")]
use crate::kernel::types::cpu_mask_t;
#[cfg(target_arch = "aarch64")]
use crate::platform_rs::timer::DurationBoot;
#[cfg(target_arch = "aarch64")]
use core::ffi::c_void;
#[cfg(target_arch = "aarch64")]
use core::pin::Pin;
#[cfg(target_arch = "aarch64")]
use zx_status::Status;

/// Wrapper around a C++ `SuspendWakeupTimer::Callback`
/// (`fit::inline_function<void(zx_instant_boot_t, zx_instant_boot_t), sizeof(void*)>`).
///
/// Note: `CppCallback` is moved by value into [`Callback`] (`zr::SendSyncInlineFn`), which
/// bitwise-relocates its storage. This relies on `fit::inline_function` and its inline
/// pointer-sized target (such as a function pointer or stateless/pointer-capturing lambda) being
/// trivially relocatable.
#[repr(C, align(8))]
pub struct CppCallback(zr::OpaqueBytes<16>);

zr::static_assert_size_and_align!(CppCallback, 16, 8);

unsafe extern "C" {
    fn cpp_suspend_wakeup_timer_callback_move(
        dst: &mut MaybeUninit<CppCallback>,
        src: NonNull<CppCallback>,
    );
    fn cpp_suspend_wakeup_timer_callback_call(
        cb: &CppCallback,
        now: InstantBoot,
        resume_at: InstantBoot,
    );
    fn cpp_suspend_wakeup_timer_callback_destroy(cb: &mut CppCallback);
}

// SAFETY: `CppCallback` wraps a thread-safe IRQ callback moved from C++.
unsafe impl Send for CppCallback {}
// SAFETY: `CppCallback` is only invoked via shared reference at IRQ time or dropped once.
unsafe impl Sync for CppCallback {}

impl CppCallback {
    /// Moves a C++ `SuspendWakeupTimer::Callback` into inline [`CppCallback`] storage.
    ///
    /// # Safety
    ///
    /// `src` must point to a valid, initialized C++ `SuspendWakeupTimer::Callback` that may be
    /// moved from.
    unsafe fn from_cpp(src: NonNull<Self>) -> Self {
        let mut dst = MaybeUninit::uninit();
        // SAFETY: `dst` has the exact size and alignment of `SuspendWakeupTimer::Callback`,
        // and the caller guarantees `src` points to a valid `SuspendWakeupTimer::Callback`.
        unsafe {
            cpp_suspend_wakeup_timer_callback_move(&mut dst, src);
            dst.assume_init()
        }
    }

    fn call(&self, now: InstantBoot, resume_at: InstantBoot) {
        // SAFETY: `self` was initialized by `cpp_suspend_wakeup_timer_callback_move`.
        unsafe {
            cpp_suspend_wakeup_timer_callback_call(self, now, resume_at);
        }
    }
}

impl Drop for CppCallback {
    fn drop(&mut self) {
        // SAFETY: `self` was initialized by `cpp_suspend_wakeup_timer_callback_move` and is
        // destroyed at most once on drop.
        unsafe {
            cpp_suspend_wakeup_timer_callback_destroy(self);
        }
    }
}

/// Opaque facade representing a C++ `Armv7MmioTimer`.
#[cfg(target_arch = "aarch64")]
#[repr(C)]
pub(crate) struct Armv7MmioTimer {
    _facade: zr::OpaqueFacade,
}

#[cfg(target_arch = "aarch64")]
zr::static_assert!(core::mem::size_of::<Armv7MmioTimer>() == 0);

/// Opaque facade representing a C++ `Armv7MmioTimer::Timer`.
#[cfg(target_arch = "aarch64")]
#[repr(C)]
pub(crate) struct Armv7MmioTimerSubtimer {
    _facade: zr::OpaqueFacade,
}

#[cfg(target_arch = "aarch64")]
zr::static_assert!(core::mem::size_of::<Armv7MmioTimerSubtimer>() == 0);

// SAFETY: `Armv7MmioTimer::Timer` is internally synchronized by its spinlock (`irq_lock_`).
#[cfg(target_arch = "aarch64")]
unsafe impl Sync for Armv7MmioTimerSubtimer {}

#[cfg(target_arch = "aarch64")]
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqHandlerResult {
    RemainRegistered = 0,
    Unregister = 1,
}

#[cfg(target_arch = "aarch64")]
unsafe extern "C" {
    fn cpp_armv7_mmio_timer_get(ndx: usize) -> Option<&'static Armv7MmioTimer>;
    fn cpp_armv7_mmio_timer_vct_timer(
        timer_block: &'static Armv7MmioTimer,
    ) -> &'static Armv7MmioTimerSubtimer;
    fn cpp_armv7_mmio_timer_pct_timer(
        timer_block: &'static Armv7MmioTimer,
    ) -> &'static Armv7MmioTimerSubtimer;
    fn cpp_armv7_mmio_timer_timer_supported(timer: &Armv7MmioTimerSubtimer) -> bool;
    fn cpp_armv7_mmio_timer_timer_enabled(timer: &Armv7MmioTimerSubtimer) -> bool;
    fn cpp_armv7_mmio_timer_timer_cancel(timer: &Armv7MmioTimerSubtimer) -> Result<(), Status>;
    fn cpp_armv7_mmio_timer_timer_set_handler(
        timer: &Armv7MmioTimerSubtimer,
        context: NonNull<c_void>,
        mask: cpu_mask_t,
    ) -> Result<(), Status>;
    fn cpp_armv7_mmio_timer_timer_reset_handler(
        timer: &Armv7MmioTimerSubtimer,
    ) -> Result<(), Status>;
    fn cpp_armv7_mmio_timer_timer_set_relative_timer(
        timer: &Armv7MmioTimerSubtimer,
        relative_timeout: DurationBoot,
    ) -> Result<(), Status>;
}

#[cfg(target_arch = "aarch64")]
impl Armv7MmioTimer {
    pub(crate) const MAX_TIMERS: usize = 8;

    /// Returns a reference to the `ndx`-th `Armv7MmioTimer` block, if present.
    ///
    /// # Safety
    ///
    /// Must only be called after platform timer initialization has completed (no earlier than
    /// `LK_INIT_LEVEL_PLATFORM`).
    pub(crate) unsafe fn get(ndx: usize) -> Option<&'static Self> {
        // SAFETY: The caller guarantees platform timer initialization has completed, and
        // `cpp_armv7_mmio_timer_get` bounds-checks `ndx` and returns either a valid pointer to a
        // static `Armv7MmioTimer` or null.
        unsafe { cpp_armv7_mmio_timer_get(ndx) }
    }

    pub(crate) fn vct_timer(&'static self) -> &'static Armv7MmioTimerSubtimer {
        // SAFETY: `self` is a valid static `Armv7MmioTimer` and `vct_timer()` returns a valid
        // C++ reference to its embedded VCT subtimer.
        unsafe { cpp_armv7_mmio_timer_vct_timer(self) }
    }

    pub(crate) fn pct_timer(&'static self) -> &'static Armv7MmioTimerSubtimer {
        // SAFETY: `self` is a valid static `Armv7MmioTimer` and `pct_timer()` returns a valid
        // C++ reference to its embedded PCT subtimer.
        unsafe { cpp_armv7_mmio_timer_pct_timer(self) }
    }
}

#[cfg(target_arch = "aarch64")]
impl Armv7MmioTimerSubtimer {
    pub(crate) fn supported(&self) -> bool {
        // SAFETY: `self` is a valid static `Armv7MmioTimer::Timer`.
        unsafe { cpp_armv7_mmio_timer_timer_supported(self) }
    }

    pub(crate) fn enabled(&self) -> bool {
        // SAFETY: `self` is a valid static `Armv7MmioTimer::Timer`.
        unsafe { cpp_armv7_mmio_timer_timer_enabled(self) }
    }

    pub(crate) fn cancel_timer(&self) -> Result<(), Status> {
        // SAFETY: `self` is a valid static `Armv7MmioTimer::Timer` (internally synchronized by
        // its spinlock).
        unsafe { cpp_armv7_mmio_timer_timer_cancel(self) }
    }

    /// Registers `outer` as the interrupt handler context on `self`.
    ///
    /// C++ treats the context as an opaque pointer and passes it back verbatim to
    /// [`rust_armv7_mmio_suspend_wakeup_timer_irq_handler`].
    ///
    /// # Safety
    ///
    /// `outer` must remain valid and pinned until [`Self::reset_handler`] completes.
    pub(crate) unsafe fn set_handler(
        &self,
        outer: Pin<&SuspendWakeupTimer>,
        mask: cpu_mask_t,
    ) -> Result<(), Status> {
        // SAFETY: `self` is a valid static `Armv7MmioTimer::Timer` and the caller guarantees
        // `outer` remains valid while the handler is registered.
        unsafe {
            cpp_armv7_mmio_timer_timer_set_handler(
                self,
                NonNull::from_ref(outer.get_ref()).cast::<c_void>(),
                mask,
            )
        }
    }

    pub(crate) fn reset_handler(&self) -> Result<(), Status> {
        // SAFETY: `self` is a valid static `Armv7MmioTimer::Timer` (internally synchronized by
        // its spinlock).
        unsafe { cpp_armv7_mmio_timer_timer_reset_handler(self) }
    }

    pub(crate) fn set_relative_timer(&self, relative_timeout: DurationBoot) -> Result<(), Status> {
        // SAFETY: `self` is a valid static `Armv7MmioTimer::Timer`.
        unsafe { cpp_armv7_mmio_timer_timer_set_relative_timer(self, relative_timeout) }
    }
}

/// Interrupt handler callback invoked by `Armv7MmioTimer::Timer`.
#[cfg(target_arch = "aarch64")]
#[unsafe(no_mangle)]
pub extern "C" fn rust_armv7_mmio_suspend_wakeup_timer_irq_handler(
    timer: &SuspendWakeupTimer,
) -> IrqHandlerResult {
    super::Armv7MmioSuspendWakeupTimer::irq_handler(timer)
}

/// Initializes a [`SuspendWakeupTimer`] in-place from C++.
///
/// # Safety
///
/// - `timer` must point to valid, properly aligned, uninitialized storage for a
///   [`SuspendWakeupTimer`], and the initialized object must remain pinned in memory until
///   dropped by [`rust_suspend_wakeup_timer_destroy`].
/// - `callback` must be a non-null pointer to a valid, initialized C++
///   `SuspendWakeupTimer::Callback` that may be moved from.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_suspend_wakeup_timer_init(
    timer: *mut SuspendWakeupTimer,
    callback: NonNull<CppCallback>,
) {
    // SAFETY: The caller guarantees `callback` points to a valid `SuspendWakeupTimer::Callback`.
    let cpp_callback = unsafe { CppCallback::from_cpp(callback) };
    let callback = Callback::new(move |now, resume_at| {
        cpp_callback.call(now, resume_at);
    });
    // SAFETY: The caller guarantees `timer` points to valid, aligned, uninitialized storage that
    // remains pinned until `rust_suspend_wakeup_timer_destroy`.
    unsafe {
        let _ = pin_init::PinInit::__pinned_init(SuspendWakeupTimer::init(callback), timer);
    }
}

/// Destroys a [`SuspendWakeupTimer`] in-place that was previously initialized by
/// [`rust_suspend_wakeup_timer_init`].
///
/// # Safety
///
/// `timer` must point to a valid, initialized [`SuspendWakeupTimer`] and must not be used after
/// this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_suspend_wakeup_timer_destroy(timer: *mut SuspendWakeupTimer) {
    // SAFETY: The caller guarantees `timer` points to a valid, initialized `SuspendWakeupTimer`
    // that will not be used again after this call.
    unsafe {
        core::ptr::drop_in_place(timer);
    }
}

/// Sets the resume deadline on `timer`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_suspend_wakeup_timer_set_resume_deadline(
    timer: &SuspendWakeupTimer,
    resume_at: InstantBoot,
) {
    timer.set_resume_deadline(resume_at);
}

/// Ensures that `timer` is started if a deadline has been configured.
#[unsafe(no_mangle)]
pub extern "C" fn rust_suspend_wakeup_timer_ensure_started(timer: &SuspendWakeupTimer) {
    timer.ensure_started();
}

/// Cancels any pending wakeup timer and resets its deadline.
#[unsafe(no_mangle)]
pub extern "C" fn rust_suspend_wakeup_timer_cancel_timer(timer: &SuspendWakeupTimer) {
    timer.cancel_timer();
}
