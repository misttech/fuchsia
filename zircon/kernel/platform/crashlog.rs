// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::platform_rs::power::ZirconCrashReason;
use core::cell::UnsafeCell;
use core::ptr::{null_mut, slice_from_raw_parts_mut};
use core::sync::atomic::{AtomicU8, Ordering};
use libc::stdio::FILE;
use zx_status::Status;

/// This is common interface definition for specific implementations of crashlog support.
/// Depending on the platform, and the resources provided to it, users will end up with
///
/// 1) A RAM mappable crashlog implementation, where the crashlog RAM is mapped
///    directly accessible by the CPUs from a mapped virtual address.
/// 2) An EFI capsule based implementation, where crashlogs get stored in an
///    EFI capsule during a non-spontaneous crash.
/// 3) A trivial, no-op implementation, because the kernel was not provided any
///    way to store data which could survive a reboot.
///
/// Generally speaking, the underlying implementations are not multi-thread
/// safe, and users should take care to never do something like having multiple
/// threads calling into `finalize` at the same time.
///
/// # Safety
///
/// Implementations must ensure that if [`Self::get_render_target`] returns a slice pointer `target`
/// with `target.len() > 0`, `target.cast::<u8>()` is non-null, valid for writes of `target.len()`
/// bytes during single-threaded platform halt after [`Self::enable_crashlog_uptime_updates`] has
/// been called with `false`, and remains unaliased until [`Self::finalize`] is called.
pub unsafe trait Interface: Sync {
    /// Returns a span which points to the region which the kernel should render
    /// a crashlog payload to during a non-spontaneous crash.
    fn get_render_target(&self) -> *mut [u8];

    /// Finalize a crashlog just before triggering a reboot. `reason` is the SW
    /// reboot reason which will be stored in the crashlog header, while `amt` is
    /// the amount of the render target (see `get_render_target`) which was filled
    /// before calling finalize.
    ///
    /// # Safety
    ///
    /// Must only be called during single-threaded platform halt after
    /// `self.enable_crashlog_uptime_updates(false)` has disabled periodic updates, and with no
    /// concurrent accesses to the render target or [`Self::recover`].
    unsafe fn finalize(&self, reason: ZirconCrashReason, amt: usize);

    /// Attempt to recover any crashlog from platform specific storage, writing
    /// the results to `tgt`. Returns the number of bytes written to `tgt` in
    /// the process. Users may pass `None` to `tgt` if they wish to simply
    /// measure the size of the crashlog to be recovered.
    ///
    /// # Safety
    ///
    /// Must only be called during boot before `self.enable_crashlog_uptime_updates(true)` or
    /// [`Self::finalize`] is invoked, and any previous crashlog region registered with the
    /// implementation must remain valid for the duration of the call.
    unsafe fn recover(&self, tgt: Option<&FILE>) -> usize;

    /// Enable periodic updates of the uptime estimate in the crashlog header.
    /// This allows systems with directly mappable crashlog RAM to constantly be
    /// stashing a valid header with an uptime estimate and a reboot reason of
    /// "UNKNOWN" to be recovered in the case of a spontaneous reboot.
    fn enable_crashlog_uptime_updates(&self, enabled: bool);
}

struct TrivialImpl;

// SAFETY: `TrivialImpl::get_render_target` always returns an empty slice (`len() == 0`).
unsafe impl Interface for TrivialImpl {
    fn get_render_target(&self) -> *mut [u8] {
        slice_from_raw_parts_mut(null_mut(), 0)
    }

    unsafe fn finalize(&self, _reason: ZirconCrashReason, _amt: usize) {}

    unsafe fn recover(&self, _tgt: Option<&FILE>) -> usize {
        0
    }

    fn enable_crashlog_uptime_updates(&self, _enabled: bool) {}
}

// `&'static dyn Interface` is a two-word wide pointer (data + vtable) that cannot fit in an
// `AtomicPtr`, so we synchronize one-time publication of `INTERFACE` using a 3-state `AtomicU8`.
const STATE_TRIVIAL: u8 = 0;
const STATE_BINDING: u8 = 1;
const STATE_BOUND: u8 = 2;

struct InterfaceSlot(UnsafeCell<&'static dyn Interface>);

// SAFETY: Access to `InterfaceSlot` is synchronized by `STATE`: `INTERFACE` is written at most
// once by the thread that transitions `STATE` from `STATE_TRIVIAL` to `STATE_BINDING`, before
// publishing `STATE_BOUND` with `Release` ordering, and is only read after observing `STATE_BOUND`
// with `Acquire` ordering.
unsafe impl Sync for InterfaceSlot {}

/// The singleton interface. This can be set to a non-trivial implementation
/// only once via `bind`.
static TRIVIAL_IMPL: TrivialImpl = TrivialImpl;
static STATE: AtomicU8 = AtomicU8::new(STATE_TRIVIAL);
static INTERFACE: InterfaceSlot = InterfaceSlot(UnsafeCell::new(&TRIVIAL_IMPL));

/// Singleton crashlog interface manager.
pub struct PlatformCrashlog {
    /// No one should be creating or destroying any PlatformCrashlog instances.
    /// Everything should be going through a `crashlog::Interface` instead.
    _private: (),
}

impl PlatformCrashlog {
    /// Interface management.
    ///
    /// By default, there is always a Crashlog implementation available, and a
    /// reference to it can be fetched using the `get` method. At boot, however,
    /// this will be a default trivial implementation which does nothing. The
    /// system may switch away from this interface when it discovers usable
    /// non-volatile storage by calling `bind`, and passing a reference to a
    /// specific Crashlog implementation. Note that this implementation must stay
    /// alive for the entire life of the kernel. Once bound to a non-trivial
    /// implementation, the interface can no longer be rebound, and any attempts to
    /// do so will return an error.
    ///
    /// Code may check to see if a non-trivial implementation has been bound by
    /// calling `has_non_trivial_impl`.
    #[inline]
    pub fn get() -> &'static dyn Interface {
        // Acquire ordering pairs with the Release store in `bind`, ensuring the write to
        // `INTERFACE` is visible before reading it.
        if STATE.load(Ordering::Acquire) == STATE_BOUND {
            // SAFETY: `STATE_BOUND` is only stored with `Release` after `INTERFACE` has been
            // written once and for all in `bind`.
            unsafe { *INTERFACE.0.get() }
        } else {
            &TRIVIAL_IMPL
        }
    }

    /// Returns whether a non-trivial crashlog implementation has been bound.
    #[inline]
    pub fn has_non_trivial_impl() -> bool {
        // Acquire ordering pairs with the Release store in `bind`.
        STATE.load(Ordering::Acquire) == STATE_BOUND
    }

    /// Binds `impl_ref` as the singleton crashlog implementation, or returns
    /// [`Status::ALREADY_BOUND`] if a non-trivial implementation has already been bound.
    pub fn bind(impl_ref: &'static dyn Interface) -> Result<(), Status> {
        // AcqRel on success and Acquire on failure ensure exclusive one-time initialization of
        // `INTERFACE`.
        if STATE
            .compare_exchange(STATE_TRIVIAL, STATE_BINDING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Status::ALREADY_BOUND);
        }
        // SAFETY: Winning the CAS from `STATE_TRIVIAL` to `STATE_BINDING` grants exclusive write
        // access to `INTERFACE`, and readers only read `INTERFACE` after observing `STATE_BOUND`
        // with `Acquire`.
        unsafe {
            *INTERFACE.0.get() = impl_ref;
        }
        // Release ordering publishes the initialized `INTERFACE` to readers using `Acquire` in
        // `get` and `has_non_trivial_impl`.
        STATE.store(STATE_BOUND, Ordering::Release);
        Ok(())
    }
}

// TODO(https://fxbug.dev/507423574): Remove once all platform callers are ported to Rust.
/// FFI wrapper around [`PlatformCrashlog::has_non_trivial_impl`] for C++ callers.
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_crashlog_has_non_trivial_impl() -> bool {
    PlatformCrashlog::has_non_trivial_impl()
}
