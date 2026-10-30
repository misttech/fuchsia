// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::deadline::{Deadline, SlackMode, TimerSlack};
use crate::kernel::timer::{Timer, ZX_CLOCK_MONOTONIC};
use crate::persistent_debuglog_rs::persistent_dlog_get_recovered_log;
use crate::platform_rs::crashlog::Interface;
use crate::platform_rs::power::ZirconCrashReason;
use crate::platform_rs::timer::{
    DurationMono, DurationUnknown, current_boot_time, current_mono_time, zx_msec, zx_sec,
};
use core::cmp::{max, min};
use core::convert::Infallible;
use core::ffi::{c_char, c_int, c_void};
use core::marker::PhantomPinned;
use core::pin::Pin;
use core::ptr::{from_mut, from_ref, null, null_mut, slice_from_raw_parts_mut};
use core::sync::atomic::{AtomicBool, Ordering};
use kprint::{kformat, kprint};
use ksync::{KMutex, RawSpinlock, guarded};
use libc::stdio::FILE;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zbi::HwRebootReason;
use zr::{slice_from_raw_parts, static_assert_size_and_align};
use zx_status::Status;
use zx_types::{zx_instant_boot_t, zx_instant_mono_t, zx_status_t};

const RAM_CRASHLOG_V1_SIZE: usize = 72;

#[repr(C)]
#[derive(Clone, Copy)]
struct RecoveredRamCrashlog {
    uptime: zx_instant_boot_t,
    runtime: zx_instant_mono_t,
    reason: u32,
    payload_valid: bool,
    payload: *const u8,
    payload_len: u32,
}

static_assert_size_and_align!(RecoveredRamCrashlog, 40, 8);

#[allow(clippy::derivable_impls)]
impl Default for RecoveredRamCrashlog {
    fn default() -> Self {
        Self {
            uptime: 0,
            runtime: 0,
            reason: 0,
            payload_valid: false,
            payload: null(),
            payload_len: 0,
        }
    }
}

unsafe extern "C" {
    fn ram_crashlog_stow(
        buf: *mut c_void,
        blen: usize,
        payload: *const c_void,
        plen: u32,
        sw_reason: ZirconCrashReason,
        uptime_nvram: zx_instant_boot_t,
        runtime_nvram: zx_instant_mono_t,
    ) -> zx_status_t;

    fn ram_crashlog_recover(
        buf: *const c_void,
        blen: usize,
        log_out: *mut RecoveredRamCrashlog,
    ) -> zx_status_t;

    fn cpp_boot_constants_hw_reboot_reason() -> u32;
}

/// # Safety
///
/// Always safe to call; only inspects `len`.
unsafe extern "C" fn null_file_write(_ptr: *mut c_void, _str: *const c_char, len: usize) -> c_int {
    debug_assert!(len <= c_int::MAX as usize);
    len as c_int
}

// SAFETY: `null_file_write` ignores `_ptr` and `_str` and only uses `len`.
const NULL_FILE: FILE = unsafe { FILE::new(Some(null_file_write), null_mut()) };

/// RAM-mapped crashlog implementation.
#[guarded]
#[pin_data(PinnedDrop)]
pub struct MappedCrashlog {
    crashlog_buffer: *mut [u8],
    render_target: *mut [u8],

    recovered_log: RecoveredRamCrashlog,
    log_recovery_result: Result<(), Status>,

    #[mutex]
    uptime_updater_lock: KMutex<RawSpinlock>,
    #[pin]
    #[guarded_by(uptime_updater_lock)]
    uptime_updater_timer: Timer,
    #[guarded_by(uptime_updater_lock)]
    uptime_updater_enabled: bool,

    /// Make sure we print the crashlog status to the debuglog only once, no matter
    /// how many times [`Self::recover`] is called.
    status_printed_to_debuglog: AtomicBool,
    #[pin]
    _pin: PhantomPinned,
}

// SAFETY: `crashlog_buffer`, `render_target`, and `recovered_log.payload` point to persistent RAM
// wired for the lifetime of the kernel, and mutable state is synchronized by
// `uptime_updater_lock` or `status_printed_to_debuglog`.
unsafe impl Send for MappedCrashlog {}
// SAFETY: See `Send` safety comment above.
unsafe impl Sync for MappedCrashlog {}

impl MappedCrashlog {
    /// Creates an in-place initializer for a [`MappedCrashlog`] backed by `range`.
    pub fn new(range: &'static mut [u8]) -> impl PinInit<Self, Infallible> {
        let crashlog_buffer: *mut [u8] =
            if range.is_empty() || !range.as_ptr().cast::<u64>().is_aligned() {
                slice_from_raw_parts_mut(null_mut(), 0)
            } else {
                from_mut(range)
            };
        let render_target: *mut [u8] = if crashlog_buffer.len() > RAM_CRASHLOG_V1_SIZE {
            // SAFETY: `crashlog_buffer` has length `> RAM_CRASHLOG_V1_SIZE`, so advancing by
            // `RAM_CRASHLOG_V1_SIZE` stays within the same allocated object.
            unsafe {
                slice_from_raw_parts_mut(
                    crashlog_buffer.cast::<u8>().add(RAM_CRASHLOG_V1_SIZE),
                    crashlog_buffer.len() - RAM_CRASHLOG_V1_SIZE,
                )
            }
        } else {
            slice_from_raw_parts_mut(null_mut(), 0)
        };

        let mut recovered_log = RecoveredRamCrashlog::default();
        let mut log_recovery_result = Err(Status::INTERNAL);
        if !crashlog_buffer.is_empty() {
            // Go ahead and "recover" the log right now. All this will do is verify the
            // various CRCs and extract the results if everything checks out. We don't
            // want to do this more than once.
            // SAFETY: `crashlog_buffer` is non-empty, 8-byte aligned, and valid for reads of
            // `crashlog_buffer.len()` bytes, and `recovered_log` is valid for writes.
            log_recovery_result = unsafe {
                Status::ok(ram_crashlog_recover(
                    crashlog_buffer.cast::<c_void>(),
                    crashlog_buffer.len(),
                    &mut recovered_log,
                ))
            };
        }

        pin_init!(Self {
            crashlog_buffer,
            render_target,
            recovered_log,
            log_recovery_result,
            uptime_updater_lock <- KMutex::init(),
            uptime_updater_timer <- ksync::kcell_init(Timer::init(ZX_CLOCK_MONOTONIC)),
            uptime_updater_enabled: false.into(),
            status_printed_to_debuglog: AtomicBool::new(false),
            _pin: PhantomPinned,
        })
    }

    fn should_print_crashlog_status(&self) -> bool {
        // `Relaxed` ordering is sufficient because this flag only gates one-time diagnostic
        // logging and does not synchronize other memory accesses.
        self.status_printed_to_debuglog
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    /// # Safety
    ///
    /// `arg` must be a valid pointer to a live [`MappedCrashlog`].
    unsafe extern "C" fn uptime_updater_callback(
        _timer: *mut Timer,
        _now: zx_instant_mono_t,
        arg: *mut c_void,
    ) {
        // SAFETY: `arg` was passed as `self` in `update_uptime_locked` and points to a
        // static-lifetime `MappedCrashlog`.
        let thiz = unsafe { &*arg.cast::<Self>() };
        ksync::lock!(let mut guard = thiz.lock_uptime_updater_lock());
        thiz.update_uptime_locked(guard.as_mut());
    }

    fn update_uptime_locked(&self, mut guard: Pin<&mut MappedCrashlogUptimeUpdaterLockGuard<'_>>) {
        if *guard.uptime_updater_enabled() {
            const DEFAULT_UPDATE_INTERVAL: DurationMono = DurationMono(zx_sec(1));

            // SAFETY: `self.crashlog_buffer` is either empty or points to valid persistent RAM for
            // `self.crashlog_buffer.len()` bytes.
            unsafe {
                ram_crashlog_stow(
                    self.crashlog_buffer.cast::<c_void>(),
                    self.crashlog_buffer.len(),
                    null(),
                    0,
                    ZirconCrashReason::Unknown,
                    current_boot_time().0,
                    current_mono_time().0,
                );
            }

            let next_update_time = Deadline::after_mono(
                DEFAULT_UPDATE_INTERVAL,
                TimerSlack::new(DurationUnknown(DEFAULT_UPDATE_INTERVAL.0 / 2), SlackMode::Center),
            );
            // SAFETY: `self` is pinned in static storage for the life of the kernel,
            // `Self::uptime_updater_callback` is a valid callback, and `uptime_updater_lock`
            // serializes all `set_deadline` and `cancel` operations on `uptime_updater_timer`.
            unsafe {
                guard.as_mut().uptime_updater_timer_mut().as_ref().set_deadline(
                    &next_update_time,
                    Self::uptime_updater_callback,
                    from_ref(self).cast_mut().cast::<c_void>(),
                );
            }
        }
    }
}

// SAFETY: `self.render_target` is either empty or points into `self.crashlog_buffer` (`'static`
// persistent RAM), which is valid for writes and unaliased once `enable_crashlog_uptime_updates`
// is disabled during platform halt.
unsafe impl Interface for MappedCrashlog {
    fn get_render_target(&self) -> *mut [u8] {
        self.render_target
    }

    unsafe fn finalize(&self, reason: ZirconCrashReason, mut amt: usize) {
        // Whatever the user tells us, the amt of the crashlog render target which was
        // filled cannot exceed the amount that we originally reported, nor can it be
        // larger than what a u32 can hold.
        amt = min(amt, self.render_target.len());
        amt = min(amt, u32::MAX as usize);

        // The RAM crashlog library will gracefully handle a nullptr or 0 length here;
        // no need to explicitly check that they are valid.
        // SAFETY: `self.crashlog_buffer` and `self.render_target` are either empty or point to
        // valid, 8-byte aligned persistent RAM regions for their respective lengths.
        unsafe {
            ram_crashlog_stow(
                self.crashlog_buffer.cast::<c_void>(),
                self.crashlog_buffer.len(),
                self.render_target.cast::<c_void>(),
                amt as u32,
                reason,
                current_boot_time().0,
                current_mono_time().0,
            );
        }
    }

    unsafe fn recover(&self, tgt: Option<&FILE>) -> usize {
        // If the user didn't supply a target FILE to render to, use the NULL_FILE
        // instead so that we compute a proper length for them as we go.
        let tgt: &FILE = tgt.unwrap_or(&NULL_FILE);

        // Create a string representation of the HW reboot reason.
        // SAFETY: `kBootConstants` is initialized by physboot before kernel entry and remains
        // read-only for the life of the kernel.
        let hw_reason = unsafe { cpp_boot_constants_hw_reboot_reason() };
        let mut str_hw_reason_buf = [0u8; 16];
        let str_hw_reason: &[u8] = match HwRebootReason::from_raw(hw_reason) {
            Some(HwRebootReason::Undefined) => b"UNKNOWN",
            Some(HwRebootReason::Cold) => b"COLD BOOT",
            Some(HwRebootReason::Warm) => b"WARM BOOT",
            Some(HwRebootReason::Brownout) => b"BROWNOUT",
            Some(HwRebootReason::Watchdog) => b"HW WATCHDOG",
            Some(HwRebootReason::UserHardReset) => b"USER HARD RESET",
            None => kformat!(&mut str_hw_reason_buf, "0x{:08x}", hw_reason),
        };

        if let Err(status) = self.log_recovery_result
            && self.should_print_crashlog_status()
        {
            kprint!(
                "Crashlog: Failed to recover crashlog.  Result {}, HW Reboot Reason \"{:s}\"\n",
                status.into_raw(),
                str_hw_reason
            );
        }

        // If we failed to recover any crashlog and there is no HW reboot reason,
        // simply report the size as 0.
        if self.log_recovery_result.is_err()
            && HwRebootReason::from_raw(hw_reason) == Some(HwRebootReason::Undefined)
        {
            return 0;
        }

        // From then on, we always return something, starting with the HW reboot reason.
        let mut fmt_buf = [0u8; 80];
        let mut written: usize = 0;
        let mut write = |bytes: &[u8]| {
            written = written.wrapping_add(max(tgt.write(bytes), 0) as usize);
        };
        write(kformat!(&mut fmt_buf, "HW REBOOT REASON ({:s})\n\n", str_hw_reason));

        if self.log_recovery_result.is_err() {
            write(
                b"WARNING - Could not recover crashlog from RAM. \
Only HW reboot reason is available.\n",
            );
            return written;
        }

        // OK, we have a log. Currently, the log is expected to be nothing but text,
        // so we need to take the structured information we have access to and put it
        // into string form. This includes:
        //
        // 1) The uptime estimate
        // 2) The runtime estimate
        // 3) The "software" reboot reason.
        // 4) The payload damage indicator (only if there was potential damage to the
        //    payload)
        //
        // The first few lines of text need to be structured so that they can be
        // understood by the crash-log harvester up in userland. Right now, this is
        // just a loose convention. Someday, it would be good to pass this data in a
        // much more structured form.
        let rlog = &self.recovered_log;
        let str_reason: &[u8] = match ZirconCrashReason::try_from(rlog.reason) {
            Ok(ZirconCrashReason::Unknown) => b"UNKNOWN",
            Ok(ZirconCrashReason::Oom) => b"OOM",
            Ok(ZirconCrashReason::Panic) => b"KERNEL PANIC",
            Ok(ZirconCrashReason::SoftwareWatchdog) => b"SW WATCHDOG",
            Ok(ZirconCrashReason::NoCrash) => b"NO CRASH",
            Ok(ZirconCrashReason::UserspaceRootJobTermination) => b"USERSPACE ROOT JOB TERMINATION",
            Ok(ZirconCrashReason::Invalid) | Err(_) => b"<null>",
        };

        if self.should_print_crashlog_status() {
            // Provide some basic details about the crashlog we recovered in the kernel
            // log. This can assist in debugging failure in CI/CQ where we might have
            // access to serial logs, but nothing else.
            let uptime_msec = rlog.uptime / zx_msec(1);
            if ZirconCrashReason::try_from(rlog.reason) == Ok(ZirconCrashReason::NoCrash) {
                kprint!(
                    "Crashlog: Clean reboot. Uptime ({}.{:03} sec) HW Reason \"{:s}\"\n",
                    uptime_msec / 1000,
                    uptime_msec % 1000,
                    str_hw_reason
                );
            } else {
                kprint!(
                    "Crashlog: Uptime ({}.{:03} sec) SW Reason \"{:s}\" HW Reason \"{:s}\" \
Payload {:s} PLen {}\n",
                    uptime_msec / 1000,
                    uptime_msec % 1000,
                    str_reason,
                    str_hw_reason,
                    if rlog.payload_valid { "valid" } else { "invalid" },
                    rlog.payload_len
                );
            }
        }

        // Second line must give the Zircon reboot reason, and be followed by two newlines.
        write(kformat!(&mut fmt_buf, "ZIRCON REBOOT REASON ({:s})\n\n", str_reason));

        // Uptime estimate comes next with a newline between the tag and the actual number
        write(kformat!(&mut fmt_buf, "UPTIME (ms)\n{}\n", rlog.uptime / zx_msec(1)));

        // Runtime estimate comes next with a newline between the tag and the actual number.
        write(kformat!(&mut fmt_buf, "RUNTIME (ms)\n{}\n", rlog.runtime / zx_msec(1)));

        if !rlog.payload_valid {
            write(
                b"WARNING - The following crashlog payload failed length/CRC sanity checks \
and may contain errors!\n",
            );
        }

        let payload_written = if !rlog.payload.is_null() && rlog.payload_len != 0 {
            // SAFETY: `rlog.payload` is a non-null pointer into `self.crashlog_buffer` validated
            // by `ram_crashlog_recover` for `rlog.payload_len` bytes, and `recover` is called
            // before `enable_crashlog_uptime_updates(true)` or `finalize` modifies the buffer.
            let payload = unsafe { slice_from_raw_parts(rlog.payload, rlog.payload_len as usize) };
            tgt.write(payload);
            rlog.payload_len as usize
        } else {
            0
        };

        // Render any persistent dlog we happened to recover
        let dlog = persistent_dlog_get_recovered_log().unwrap_or(&[]);
        if !dlog.is_empty() {
            write(kformat!(
                &mut fmt_buf,
                "Recovered {} bytes from the persistent debug log\n",
                dlog.len()
            ));
            write(b"=================== BEGIN ===================\n");
            write(dlog);
            write(b"=================== END ===================\n");
        }

        // Report the total length.
        written.wrapping_add(payload_written).wrapping_add(rlog.payload_len as usize)
    }

    fn enable_crashlog_uptime_updates(&self, enabled: bool) {
        ksync::lock!(let mut guard = self.lock_uptime_updater_lock());

        if *guard.uptime_updater_enabled() != enabled {
            *guard.as_mut().uptime_updater_enabled_mut() = enabled;
            if *guard.uptime_updater_enabled() {
                self.update_uptime_locked(guard.as_mut());
            } else {
                guard.uptime_updater_timer().cancel();
            }
        }
    }
}

#[pinned_drop]
impl PinnedDrop for MappedCrashlog {
    fn drop(self: Pin<&mut Self>) {
        // In case `self` is ever destroyed, ensure the timer has been canceled first.
        self.enable_crashlog_uptime_updates(false);
    }
}
