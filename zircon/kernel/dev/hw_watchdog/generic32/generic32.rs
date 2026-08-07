// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
//
// Ported from zircon/kernel/dev/hw_watchdog/generic32/hw_watchdog.cc

use crate::kernel::timer::{Deadline, SlackMode, Timer, TimerSlack, ZX_CLOCK_BOOT};
use crate::platform_rs::timer::current_boot_time;
use core::ptr::with_exposed_provenance_mut;
use debug::dprintf;
use regio::{Mmio, MmioPtr, RwSafe};
#[cfg(ktest)]
use unittest as _;
use zx_status::Status;

const FORCE_WATCHDOG_DISABLED_NAME: &str = "kernel.force-watchdog-disabled";

unsafe extern "C" fn watchdog_timer_cb(_timer: *mut Timer, _now: i64, arg: *mut core::ffi::c_void) {
    // SAFETY: The caller must ensure that `arg` is a valid pointer that can be passed
    // to `rust_watchdog_on_pet_timer`.
    unsafe {
        rust_watchdog_on_pet_timer(arg);
    }
}

// TODO(https://fxbug.dev/42062786): Switch to //sdk/rust/zbi (or //sdk/fidl/zbi) once Zither
// supports bare-metal kernel_rust_mod dependencies without serde_core.

/// Defines a register write action for a generic 32-bit kernel watchdog driver.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ZbiDcfgGeneric32WatchdogAction {
    /// Physical or virtual MMIO register address.
    pub addr: u64,
    /// Bitmask of bits to clear in the register before setting new bits.
    pub clr_mask: u32,
    /// Bitmask of bits to set in the register.
    pub set_mask: u32,
}

/// Driver configuration item (`ZBI_KERNEL_DRIVER_GENERIC32_WATCHDOG`) passed from bootloader.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ZbiDcfgGeneric32Watchdog {
    /// The register action needed to pet (dismiss) the hardware watchdog.
    pub pet_action: ZbiDcfgGeneric32WatchdogAction,
    /// The register action needed to enable the hardware watchdog.
    pub enable_action: ZbiDcfgGeneric32WatchdogAction,
    /// The register action needed to disable the hardware watchdog.
    pub disable_action: ZbiDcfgGeneric32WatchdogAction,
    /// Nominal watchdog timeout period in nanoseconds (`zx_duration_boot_t`).
    pub watchdog_period_nsec: i64,
    /// Configuration flags (e.g., `ZBI_KERNEL_DRIVER_GENERIC32_WATCHDOG_FLAGS_ENABLED`).
    pub flags: u32,
    /// Reserved padding field, must be zero.
    pub reserved: u32,
}

pub const ZBI_KERNEL_DRIVER_GENERIC32_WATCHDOG_FLAGS_ENABLED: u32 = 1 << 0;
pub const ZBI_KERNEL_DRIVER_GENERIC32_WATCHDOG_MIN_PERIOD: i64 = 1_000_000;

zr::static_assert!(core::mem::size_of::<ZbiDcfgGeneric32WatchdogAction>() == 16);
zr::static_assert!(core::mem::align_of::<ZbiDcfgGeneric32WatchdogAction>() == 8);
zr::static_assert!(core::mem::size_of::<ZbiDcfgGeneric32Watchdog>() == 64);
zr::static_assert!(core::mem::align_of::<ZbiDcfgGeneric32Watchdog>() == 8);

unsafe extern "C" {
    /// Translates a physical MMIO address in-place to a virtual address. Returns `true` if valid.
    fn cpp_watchdog_translate_paddr(paddr: *mut u64) -> bool;
    /// Returns `true` if the `force_watchdog_disabled` kernel command-line option is set.
    fn cpp_watchdog_is_force_disabled_cmdline() -> bool;
}

/// Inner state of the generic 32-bit watchdog driver, protected by a spinlock.
pub struct GenericWatchdog32Inner {
    /// Driver configuration received during early boot.
    pub cfg: ZbiDcfgGeneric32Watchdog,
    /// Result code (`zx_status_t`) of the early initialization phase.
    pub early_init_result: i32,
    /// Timestamp (`zx_instant_boot_t`) when the watchdog was last pet.
    pub last_pet_time: i64,
    /// Zircon kernel timer used for scheduling periodic pet callbacks.
    pub pet_timer: core::mem::MaybeUninit<Timer>,
    pub pet_timer_initialized: bool,
    /// Whether the hardware watchdog is currently enabled.
    pub is_enabled: bool,
    /// Whether watchdog petting is currently suppressed (e.g., during panic/crashlog generation).
    pub is_petting_suppressed: bool,
}

impl GenericWatchdog32Inner {
    const fn new() -> Self {
        Self {
            cfg: ZbiDcfgGeneric32Watchdog {
                pet_action: ZbiDcfgGeneric32WatchdogAction { addr: 0, clr_mask: 0, set_mask: 0 },
                enable_action: ZbiDcfgGeneric32WatchdogAction { addr: 0, clr_mask: 0, set_mask: 0 },
                disable_action: ZbiDcfgGeneric32WatchdogAction {
                    addr: 0,
                    clr_mask: 0,
                    set_mask: 0,
                },
                watchdog_period_nsec: 0,
                flags: 0,
                reserved: 0,
            },
            early_init_result: -1, // Status::INTERNAL.into_raw()
            last_pet_time: 0,
            pet_timer: core::mem::MaybeUninit::uninit(),
            pet_timer_initialized: false,
            is_enabled: false,
            is_petting_suppressed: false,
        }
    }

    /// Performs the read-modify-write MMIO action on the watchdog register using `regio`.
    ///
    /// # Safety
    ///
    /// `action.addr` must be a valid, mapped virtual address pointing to a 32-bit MMIO register.
    unsafe fn take_action(&self, action: &ZbiDcfgGeneric32WatchdogAction) {
        if action.addr == 0 {
            return;
        }
        // SAFETY: `action.addr` is verified to be a valid mapped virtual address when non-zero.
        let raw_ptr = with_exposed_provenance_mut::<u32>(action.addr as usize);
        let ptr = unsafe { MmioPtr::<u32, RwSafe>::new(raw_ptr) };
        let reg = Mmio::<u32, u32, RwSafe>::new(ptr);
        reg.modify(|val| {
            *val &= !action.clr_mask;
            *val |= action.set_mask;
        });
    }

    fn pet_locked(&mut self) -> i64 {
        let now = current_boot_time().0;
        if !self.is_petting_suppressed {
            self.last_pet_time = now;
            // SAFETY: `self.cfg.pet_action` contains the validated MMIO address for petting.
            unsafe { self.take_action(&self.cfg.pet_action) };
        }
        now
    }

    fn handle_pet_timer_locked(&mut self) {
        if self.is_enabled {
            let last_pet = self.pet_locked();
            let timeout = self.cfg.watchdog_period_nsec;
            let next_pet_time = last_pet.saturating_add(timeout / 2);
            let slack = timeout / 4;
            if self.pet_timer_initialized {
                // SAFETY: G_WATCHDOG is a global static, so pet_timer is in a stable location
                // and we can safely create a Pin<&mut Timer>.
                unsafe {
                    let mut timer =
                        core::pin::Pin::new_unchecked(&mut *self.pet_timer.as_mut_ptr());
                    let deadline = Deadline {
                        when: next_pet_time,
                        slack: TimerSlack { amount: slack, mode: SlackMode::Early },
                    };
                    timer.as_mut().set_deadline(
                        &deadline,
                        watchdog_timer_cb,
                        core::ptr::addr_of!(*G_WATCHDOG).cast_mut().cast(),
                    );
                }
            }
        }
    }

    fn init_early(&mut self, config: &ZbiDcfgGeneric32Watchdog) {
        // SAFETY: We initialize pet_timer in place using its pin initializer.
        unsafe {
            let slot = self.pet_timer.as_mut_ptr();
            if pin_init::PinInit::__pinned_init(Timer::init(ZX_CLOCK_BOOT), slot).is_ok() {
                self.pet_timer_initialized = true;
            }
        }

        if config.pet_action.addr == 0 {
            self.early_init_result = Status::INVALID_ARGS.into_raw();
            return;
        }
        if config.watchdog_period_nsec < ZBI_KERNEL_DRIVER_GENERIC32_WATCHDOG_MIN_PERIOD {
            self.early_init_result = Status::INVALID_ARGS.into_raw();
            return;
        }

        self.cfg = *config;
        // SAFETY: `self.cfg.pet_action.addr` is a pointer to the address field in our local config copy, valid for modification by the C++ translation shim.
        if !unsafe { cpp_watchdog_translate_paddr(&mut self.cfg.pet_action.addr) } {
            self.early_init_result = Status::IO.into_raw();
            return;
        }
        // SAFETY: `enable_action.addr` and `disable_action.addr` point to valid `u64` address fields in `self.cfg`.
        unsafe {
            cpp_watchdog_translate_paddr(&mut self.cfg.enable_action.addr);
            cpp_watchdog_translate_paddr(&mut self.cfg.disable_action.addr);
        }

        self.is_enabled =
            (self.cfg.flags & ZBI_KERNEL_DRIVER_GENERIC32_WATCHDOG_FLAGS_ENABLED) != 0;

        self.pet_locked();
        // SAFETY: Calling C++ helper to read kernel boot options requires no parameters and has no side effects.
        if unsafe { cpp_watchdog_is_force_disabled_cmdline() }
            && self.is_enabled
            && self.cfg.disable_action.addr != 0
        {
            // SAFETY: `disable_action` contains the validated MMIO address for disabling.
            unsafe { self.take_action(&self.cfg.disable_action) };
            self.is_enabled = false;
        }

        self.early_init_result = Status::OK.into_raw();
    }

    fn init(&mut self) {
        if self.early_init_result != Status::OK.into_raw() {
            dprintf!(
                INFO,
                "WDT: Generic watchdog driver attempted to load, but failed during early init (res {}).\n",
                self.early_init_result
            );
            return;
        }
        let force_disabled = unsafe { cpp_watchdog_is_force_disabled_cmdline() };
        let can_disable = self.cfg.disable_action.addr != 0;
        let msec = self.cfg.watchdog_period_nsec / 1_000_000;
        let usec = (self.cfg.watchdog_period_nsec % 1_000_000) / 1000;
        dprintf!(
            INFO,
            "WDT: Generic watchdog driver loaded.  Period ({}.{:03} mSec) Enabled ({})\n",
            msec,
            usec,
            if self.is_enabled { "yes" } else { "no" }
        );
        if force_disabled {
            if can_disable {
                dprintf!(
                    INFO,
                    "WDT: {} was set, watchdog was force-disabled\n",
                    FORCE_WATCHDOG_DISABLED_NAME
                );
            } else {
                dprintf!(
                    INFO,
                    "WDT: {} was set, but the watchdog cannot be disabled.  It is currently {}.\n",
                    FORCE_WATCHDOG_DISABLED_NAME,
                    if self.is_enabled { "enabled" } else { "disabled" }
                );
            }
        }
        self.handle_pet_timer_locked();
    }

    fn set_enabled(&mut self, enb: bool) -> Result<(), Status> {
        if enb == self.is_enabled {
            return Ok(());
        }
        let target_addr =
            if enb { self.cfg.enable_action.addr } else { self.cfg.disable_action.addr };
        if target_addr == 0 {
            return Err(Status::NOT_SUPPORTED);
        }
        self.is_enabled = enb;
        if self.is_enabled {
            // SAFETY: `enable_action` contains the validated MMIO address for enabling.
            unsafe { self.take_action(&self.cfg.enable_action) };
            self.handle_pet_timer_locked();
        } else {
            // SAFETY: `disable_action` contains the validated MMIO address for disabling.
            unsafe { self.take_action(&self.cfg.disable_action) };
            if self.pet_timer_initialized {
                // SAFETY: G_WATCHDOG is static, so pet_timer is pinned.
                let mut timer =
                    unsafe { core::pin::Pin::new_unchecked(&mut *self.pet_timer.as_mut_ptr()) };
                timer.as_mut().cancel();
            }
        }
        Ok(())
    }
}

/// Global 32-bit generic hardware watchdog driver structure.
#[ksync::guarded]
pub struct GenericWatchdog32 {
    #[guarded_by(lock)]
    inner: GenericWatchdog32Inner,

    #[mutex]
    lock: ksync::KMutex<ksync::RawSpinlock>,
}

struct WatchdogHolder(core::cell::UnsafeCell<core::mem::MaybeUninit<GenericWatchdog32>>);

// SAFETY: Synchronization is managed by `GenericWatchdog32`'s internal spinlock (`lock`).
unsafe impl Sync for WatchdogHolder {}

static G_WATCHDOG: WatchdogHolder =
    WatchdogHolder(core::cell::UnsafeCell::new(core::mem::MaybeUninit::uninit()));

impl WatchdogHolder {
    /// Initializes `G_WATCHDOG` in place during early single-threaded boot.
    ///
    /// # Safety
    /// Must only be called once during early single-threaded boot before multiple CPUs or threads are active.
    #[inline]
    unsafe fn init_in_place(&self) {
        use pin_init::InPlaceWrite as _;
        // SAFETY: Called once during early boot; `self.0.get()` points to valid static uninitialized memory.
        unsafe {
            let uninit_mut: &'static mut core::mem::MaybeUninit<GenericWatchdog32> =
                &mut *self.0.get();
            let initializer = pin_init::pin_init!(GenericWatchdog32 {
                inner: ksync::KCell::new(GenericWatchdog32Inner::new()),
                lock <- ksync::KSpinlock::init(),
            });
            let _ = uninit_mut.write_pin_init(initializer);
        }
    }
}

impl core::ops::Deref for WatchdogHolder {
    type Target = GenericWatchdog32;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: `G_WATCHDOG` is initialized in `generic_32bit_watchdog_early_init` prior to any reference
        // or usage, and lives in static storage forever.
        unsafe { &*self.0.get().cast::<GenericWatchdog32>() }
    }
}

/// Callback invoked from Zircon timer tick when the pet timer expires.
///
/// # Safety
///
/// `arg` must be a pointer to `GenericWatchdog32`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_watchdog_on_pet_timer(arg: *mut core::ffi::c_void) {
    // SAFETY: `arg` is verified upon timer registration (`cpp_watchdog_timer_set`) to be a pointer to `G_WATCHDOG`.
    let watchdog = unsafe { &*(arg as *const GenericWatchdog32) };
    ksync::lock!(let mut guard = watchdog.lock_lock());
    guard.as_mut().fields_mut().inner.handle_pet_timer_locked();
}

extern "C" fn thunk_pet() {
    ksync::lock!(let mut guard = G_WATCHDOG.lock_lock());
    let inner = guard.as_mut().fields_mut().inner;
    if inner.is_enabled {
        inner.pet_locked();
    }
}

extern "C" fn thunk_set_enabled(enabled: bool) -> i32 {
    ksync::lock!(let mut guard = G_WATCHDOG.lock_lock());
    guard
        .as_mut()
        .fields_mut()
        .inner
        .set_enabled(enabled)
        .map_or_else(|s| s.into_raw(), |_| Status::OK.into_raw())
}

extern "C" fn thunk_is_enabled() -> bool {
    ksync::lock!(let guard = G_WATCHDOG.lock_lock());
    guard.fields().inner.is_enabled
}

extern "C" fn thunk_get_timeout_nsec() -> i64 {
    ksync::lock!(let guard = G_WATCHDOG.lock_lock());
    guard.fields().inner.cfg.watchdog_period_nsec
}

extern "C" fn thunk_get_last_pet_time() -> i64 {
    ksync::lock!(let guard = G_WATCHDOG.lock_lock());
    guard.fields().inner.last_pet_time
}

extern "C" fn thunk_suppress_petting(suppress: bool) {
    ksync::lock!(let mut guard = G_WATCHDOG.lock_lock());
    guard.as_mut().fields_mut().inner.is_petting_suppressed = suppress;
}

extern "C" fn thunk_is_petting_suppressed() -> bool {
    ksync::lock!(let guard = G_WATCHDOG.lock_lock());
    guard.fields().inner.is_petting_suppressed
}

static THUNKS: crate::pdev_watchdog::PdevWatchdogOps = crate::pdev_watchdog::PdevWatchdogOps {
    pet: thunk_pet,
    set_enabled: thunk_set_enabled,
    is_enabled: thunk_is_enabled,
    get_timeout_nsec: thunk_get_timeout_nsec,
    get_last_pet_time: thunk_get_last_pet_time,
    suppress_petting: thunk_suppress_petting,
    is_petting_suppressed: thunk_is_petting_suppressed,
};

/// Early single-threaded initialization routine for the generic 32-bit watchdog driver.
///
/// # Safety
///
/// `config` must be a valid pointer to a `ZbiDcfgGeneric32Watchdog` structure.
/// This must only be called once during early single-threaded boot before multiple CPUs or threads are active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn generic_32bit_watchdog_early_init(
    config: *const ZbiDcfgGeneric32Watchdog,
) {
    // SAFETY: Called once during early single-threaded boot before any references to G_WATCHDOG.
    unsafe { G_WATCHDOG.init_in_place() };
    if config.is_null() {
        return;
    }
    ksync::lock!(let mut guard = G_WATCHDOG.lock_lock());
    // SAFETY: `config` is checked to be non-null and guaranteed by caller to point to a valid `ZbiDcfgGeneric32Watchdog`.
    let inner = guard.as_mut().fields_mut().inner;
    inner.init_early(unsafe { &*config });
    if inner.early_init_result == Status::OK.into_raw() {
        // SAFETY: Registering THUNKS operations table with pdev watchdog layer.
        unsafe { crate::pdev_watchdog::pdev_register_watchdog(&THUNKS) };
    }
}

/// Post-VM initialization routine for the generic 32-bit watchdog driver.
/// Currently a no-op matching C++.
///
/// # Safety
///
/// `config` must be a valid pointer to a `ZbiDcfgGeneric32Watchdog` structure if accessed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn generic_32bit_watchdog_init_post_vm(
    _config: *const ZbiDcfgGeneric32Watchdog,
) {
}

/// Late multi-threaded initialization routine for the generic 32-bit watchdog driver.
#[unsafe(no_mangle)]
pub extern "C" fn generic_32bit_watchdog_late_init() {
    ksync::lock!(let mut guard = G_WATCHDOG.lock_lock());
    guard.as_mut().fields_mut().inner.init();
}

/// Generic 32-bit watchdog kernel unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "generic32_watchdog")]
mod tests {
    use unittest::{assert_eq, assert_false, assert_true};

    /// Tests `take_action` MMIO read-modify-write on a mock register buffer and no-op on zero address.
    #[test]
    fn test_generic32_take_action_mock_mmio() {
        let mut mock_mmio: u32 = 0x1234_5678;
        let action = ZbiDcfgGeneric32WatchdogAction {
            addr: core::ptr::addr_of_mut!(mock_mmio) as u64,
            clr_mask: 0x0000_FF00,
            set_mask: 0x0000_00AB,
        };
        let zero_action = ZbiDcfgGeneric32WatchdogAction {
            addr: 0,
            clr_mask: 0xFFFF_FFFF,
            set_mask: 0xFFFF_FFFF,
        };
        let inner = GenericWatchdog32Inner::new();
        // SAFETY: `action` points to valid stack u32, and `zero_action` has addr 0 which safely returns early.
        unsafe {
            inner.take_action(&action);
            inner.take_action(&zero_action);
        }
        assert_eq!(mock_mmio, 0x1234_00FB);

        // Verify region Mmio register construction and read interface directly.
        let raw_ptr = core::ptr::addr_of_mut!(mock_mmio);
        let ptr = unsafe { MmioPtr::<u32, RwSafe>::new(raw_ptr) };
        let reg = Mmio::<u32, u32, RwSafe>::new(ptr);
        assert_eq!(reg.read(), 0x1234_00FB);
    }

    /// Tests `init_early` configuration parsing and validation.
    #[test]
    fn test_generic32_config_parsing() {
        let mut inner = GenericWatchdog32Inner::new();
        let mut bad_config = ZbiDcfgGeneric32Watchdog {
            pet_action: ZbiDcfgGeneric32WatchdogAction { addr: 0, clr_mask: 0, set_mask: 0 },
            enable_action: ZbiDcfgGeneric32WatchdogAction { addr: 0, clr_mask: 0, set_mask: 0 },
            disable_action: ZbiDcfgGeneric32WatchdogAction { addr: 0, clr_mask: 0, set_mask: 0 },
            watchdog_period_nsec: ZBI_KERNEL_DRIVER_GENERIC32_WATCHDOG_MIN_PERIOD,
            flags: 0,
            reserved: 0,
        };
        inner.init_early(&bad_config);
        assert_eq!(inner.early_init_result, Status::INVALID_ARGS.into_raw());

        let mut mock_mmio: u32 = 0;
        bad_config.pet_action.addr = core::ptr::addr_of_mut!(mock_mmio) as u64;
        bad_config.watchdog_period_nsec = ZBI_KERNEL_DRIVER_GENERIC32_WATCHDOG_MIN_PERIOD - 1;
        inner.init_early(&bad_config);
        assert_eq!(inner.early_init_result, Status::INVALID_ARGS.into_raw());

        bad_config.watchdog_period_nsec = ZBI_KERNEL_DRIVER_GENERIC32_WATCHDOG_MIN_PERIOD;
        inner.init_early(&bad_config);
        assert_eq!(inner.early_init_result, Status::OK.into_raw());
    }

    /// Tests watchdog state transitions when enabling, disabling, and suppressing petting.
    #[test]
    fn test_generic32_state_transitions() {
        let mut mock_pet: u32 = 0;
        let mut mock_enb: u32 = 0;
        let mut mock_dis: u32 = 0;

        let config = ZbiDcfgGeneric32Watchdog {
            pet_action: ZbiDcfgGeneric32WatchdogAction {
                addr: core::ptr::addr_of_mut!(mock_pet) as u64,
                clr_mask: 0,
                set_mask: 1,
            },
            enable_action: ZbiDcfgGeneric32WatchdogAction {
                addr: core::ptr::addr_of_mut!(mock_enb) as u64,
                clr_mask: 0,
                set_mask: 2,
            },
            disable_action: ZbiDcfgGeneric32WatchdogAction {
                addr: core::ptr::addr_of_mut!(mock_dis) as u64,
                clr_mask: 0,
                set_mask: 4,
            },
            watchdog_period_nsec: ZBI_KERNEL_DRIVER_GENERIC32_WATCHDOG_MIN_PERIOD,
            flags: 0,
            reserved: 0,
        };

        let mut inner = GenericWatchdog32Inner::new();
        inner.init_early(&config);
        assert_eq!(inner.early_init_result, Status::OK.into_raw());
        assert_false!(inner.is_enabled);

        assert_true!(inner.set_enabled(true).is_ok());
        assert_true!(inner.is_enabled);
        assert_eq!(mock_enb, 2);

        mock_enb = 0;
        assert_true!(inner.set_enabled(true).is_ok());
        assert_eq!(mock_enb, 0);

        assert_false!(inner.is_petting_suppressed);
        inner.is_petting_suppressed = true;
        assert_true!(inner.is_petting_suppressed);

        assert_true!(inner.set_enabled(false).is_ok());
        assert_false!(inner.is_enabled);
        assert_eq!(mock_dis, 4);
    }

    /// Tests global boot handoff null safety and `THUNKS` operations table callbacks.
    #[test]
    fn test_generic32_thunks_and_handoff_apis() {
        // SAFETY: Testing global C-ABI initialization functions with null pointers, which should safely early-return as no-ops.
        unsafe {
            generic_32bit_watchdog_early_init(core::ptr::null());
            generic_32bit_watchdog_init_post_vm(core::ptr::null());
        }

        // Verify `THUNKS` operations table callbacks.
        assert_false!((THUNKS.is_enabled)());
        (THUNKS.suppress_petting)(true);
        assert_true!((THUNKS.is_petting_suppressed)());
        (THUNKS.suppress_petting)(false);
        assert_false!((THUNKS.is_petting_suppressed)());
        (THUNKS.pet)();
    }
}
