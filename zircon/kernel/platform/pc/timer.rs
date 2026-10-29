// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2009 Corey Tabaka
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::hpet::{
    hpet_disable, hpet_enable, hpet_get_value, hpet_is_present, hpet_set_value, hpet_ticks_per_ms,
    hpet_wait_ms,
};
use super::interrupts::{register_permanent_int_handler, unmask_interrupt};
use crate::arch_rs::x86::feature::{
    X86_FEATURE_INVAR_TSC, X86_FEATURE_TSC_DEADLINE, X86ModelInfo, X86VendorList, x86_feature_test,
    x86_get_model, x86_get_vendor, x86_hypervisor_has_pv_clock, x86_lookup_core_crystal_freq,
    x86_lookup_tsc_freq,
};
use crate::arch_rs::x86::ioapic::apic_io_isa_to_global;
use crate::arch_rs::x86::lapic::{
    apic_timer_current_count, apic_timer_set_oneshot, apic_timer_set_tsc_deadline, apic_timer_stop,
    apic_timer_tsc_deadline_init,
};
use crate::arch_rs::x86::pv::{
    pv_clock_get_tsc_freq, pv_clock_init, pv_clock_is_stable, pv_clock_shutdown,
};
use crate::arch_rs::x86::x86::{inp, outp};
use crate::arch_rs::{curr_cpu_num, ints_disabled};
use crate::counters::define_kcounter;
use crate::dev_interrupt::InterruptHandler;
use crate::platform_rs::timer::{
    GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS, GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES,
    GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS, GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES,
    GET_TICKS_SYNC_FLAG_NONE, current_mono_time, timer_get_mono_ticks_offset,
    timer_set_initial_ticks, timer_set_ticks_to_time_ratio, timer_tick, zx_msec,
};
use affine::{Exact, Ratio, Round, Saturate, Transform};
use boot_options::{BootOptions, WallclockType};
use core::arch::asm;
use core::arch::x86_64::{__rdtscp, _rdtsc};
use core::ffi::c_void;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};
use debug::ltracef;
use fixed_point::{Fp3264, fp_32_64_div_32_32};
use kprint::kprint;
use libarch::x86::{device_memory_barrier, serialize_instructions};
use zx_status::Status;
use zx_types::{zx_duration_t, zx_instant_mono_t, zx_instant_mono_ticks_t, zx_ticks_t};

define_kcounter!(PLATFORM_TIMER_SET_COUNTER, "platform.timer.set", Sum);
define_kcounter!(PLATFORM_TIMER_CANCEL_COUNTER, "platform.timer.cancel", Sum);

// Current timer scheme:
// The HPET is used to calibrate the local APIC timers and the TSC.  If the
// HPET is not present, we will fallback to calibrating using the PIT.
//
// For wall-time, we use the following mechanisms, in order of highest
// preference to least:
// 1) TSC: If the CPU advertises an invariant TSC, then we will use the TSC for
// tracking wall time in a tickless manner.
// 2) HPET: If there is an HPET present, we will use its count to track wall
// time in a tickless manner.
// 3) PIT: We will use periodic interrupts to update wall time.
//
// The local APICs are responsible for handling timer callbacks
// sent from the scheduler.

/// Platform clock source selection.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ClockSource {
    /// Used before `WALL_CLOCK` is selected. `current_mono_ticks()` returns 0.
    #[default]
    Unselected = 0,

    Tsc = 1,
    Pit = 2,
    Hpet = 3,
}

/// Number of [`ClockSource`] variants.
const CLOCK_COUNT: usize = 4;

/// Human-readable names indexed by [`ClockSource`].
const CLOCK_NAME: [&str; CLOCK_COUNT] = ["UNSELECTED", "TSC", "PIT", "HPET"];
zr::static_assert!(CLOCK_NAME.len() == CLOCK_COUNT);

/// PIT time accounting info: microseconds per PIT tick in 32.64 fixed point.
static mut US_PER_PIT: Fp3264 = Fp3264 { l0: 0, l32: 0, l64: 0 };
/// Monotonically increasing PIT interrupt tick counter.
static PIT_TICKS: AtomicU64 = AtomicU64::new(0);
/// Configured PIT frequency divisor.
static mut PIT_DIVISOR: u16 = 0;

/// Whether or not we have an Invariant TSC (controls whether we use the PIT or
/// not after initialization).  The Invariant TSC is rate-invariant under P-, C-,
/// and T-state transitions.
static mut INVARIANT_TSC: bool = false;
/// Whether or not we have a Constant TSC (controls whether we bother calibrating
/// the TSC).  Constant TSC predates the Invariant TSC.  The Constant TSC is
/// rate-invariant under P-state transitions.
static mut CONSTANT_TSC: bool = false;

/// The ratio between the chosen reference timer's ticks and the APIC's ticks.
/// This is set after clock selection is complete in `pc_init_timer`.
static mut REFERENCE_TIMER_TICKS_TO_APIC_TICKS: Ratio = Ratio::const_default();

/// Selected wall clock source.
static mut WALL_CLOCK: ClockSource = ClockSource::Unselected;
/// Clock source used for calibrating the APIC timer and TSC.
static mut CALIBRATION_CLOCK: ClockSource = ClockSource::Unselected;

/// APIC timer calibration values: whether TSC deadline mode is active.
static mut USE_TSC_DEADLINE: bool = false;
/// Calibrated APIC timer ticks per millisecond.
static mut APIC_TICKS_PER_MS: u32 = 0;
/// Calibrated APIC timer ticks per nanosecond in 32.64 fixed point.
static mut APIC_TICKS_PER_NS: Fp3264 = Fp3264 { l0: 0, l32: 0, l64: 0 };
/// Calibrated APIC timer divisor.
static mut APIC_DIVISOR: u8 = 0;

/// TSC timer calibration values: TSC ticks per millisecond.
static mut TSC_TICKS_PER_MS: u64 = 0;
/// Nanoseconds per TSC tick in 32.64 fixed point.
static mut NS_PER_TSC: Fp3264 = Fp3264 { l0: 0, l32: 0, l64: 0 };
/// Conversion ratio from RDTSC ticks to clock monotonic nanoseconds.
static mut RDTSC_TICKS_TO_CLOCK_MONOTONIC: Ratio = Ratio::const_default();

/// HPET calibration values: nanoseconds per HPET tick in 32.64 fixed point.
static mut NS_PER_HPET: Fp3264 = Fp3264 { l0: 0, l32: 0, l64: 0 };
/// Conversion ratio from HPET ticks to clock monotonic nanoseconds.
static mut HPET_TICKS_TO_CLOCK_MONOTONIC: Ratio = Ratio::const_default();

/// An affine transformation from times sampled from the `EarlyTicks` timeline to
/// the chosen ticks timeline.  By default, this transformation is set up as:
///
///   `f(t) = (((t - 0) * 0) / 1) + 0;`
///
/// meaning that it will map all early ticks value `t` to 0, and the inverse
/// transformation will be undefined.  This is consistent with simply
/// reporting 0 for normalized `EarlyTicks` values if we cannot (or do not know how
/// to) convert from one timeline to the other.
static mut EARLY_TICKS_TO_TICKS: Transform = Transform::new(0, 0, Ratio::new(0, 1));

/// Nominal PIT oscillator frequency in Hz.
const INTERNAL_FREQ: u32 = 1_193_182;
/// 3x nominal PIT oscillator frequency in Hz.
const INTERNAL_FREQ_3X: u32 = 3_579_546;

/// Nominal PIT oscillator ticks per millisecond.
const INTERNAL_FREQ_TICKS_PER_MS: u32 = INTERNAL_FREQ / 1000;

/// Maximum amount of time that can be programmed on the timer to schedule the next
/// interrupt, in milliseconds.
const MAX_TIMER_INTERVAL: i64 = zx_msec(55);

/// Local trace verbosity setting.
const LOCAL_TRACE: u32 = 0;

/// Legacy ISA IRQ number for the Programmable Interval Timer (PIT).
const ISA_IRQ_PIT: u8 = 0;

#[inline]
fn current_ticks_rdtsc() -> zx_ticks_t {
    // SAFETY: `_rdtsc` is always safe to execute in ring 0 on x86_64.
    unsafe { _rdtsc() as zx_ticks_t }
}

#[inline]
fn current_ticks_rdtscp() -> zx_ticks_t {
    let mut unused: u32 = 0;
    // SAFETY: `__rdtscp` is safe to execute when RDTSCP is supported; `unused` is a valid pointer.
    unsafe { __rdtscp(&mut unused) as zx_ticks_t }
}

fn current_ticks_hpet() -> zx_ticks_t {
    hpet_get_value() as zx_ticks_t
}

fn current_ticks_pit() -> zx_ticks_t {
    // Relaxed ordering is sufficient because PIT_TICKS is a standalone monotonic counter and
    // callers do not synchronize other memory accesses through it.
    PIT_TICKS.load(Ordering::Relaxed) as zx_ticks_t
}

/// Reads a platform-specific ticks counter synchronized according to `FLAGS`.
#[inline]
pub fn platform_current_raw_ticks_synchronized<const FLAGS: u8>() -> zx_ticks_t {
    // SAFETY: `WALL_CLOCK` is initialized once during single-threaded boot in `pc_init_timer`
    // and is read-only thereafter.
    let wall_clock = unsafe { WALL_CLOCK };
    // Directly call the ticks functions to avoid the cost of a virtual (indirect) call.
    if wall_clock == ClockSource::Tsc {
        // See "Intel® 64 and IA-32 Architectures Software Developer’s Manual Vol.
        // 2B Section 4.3", specifically the entries for RDTSC and RDTSCP for a
        // description of the serialization properties of the instructions which
        // access the TSC.
        //
        // If all stores must be completed and "globally visible" before the TSC is
        // sampled, docs say to put an MFENCE in front of the TSC access.
        if (FLAGS & GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES) != GET_TICKS_SYNC_FLAG_NONE {
            device_memory_barrier();
        }

        // If all loads must be complete and "globally visible" (meaning that the
        // value to load has been determined) before the TSC is sampled, docs say to
        // either execute `LFENCE ; RDTSC` or just `RDTSCP`.
        let ret: zx_ticks_t =
            if (FLAGS & GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS) != GET_TICKS_SYNC_FLAG_NONE {
                current_ticks_rdtscp()
            } else {
                current_ticks_rdtsc()
            };

        // Finally, if we need the TSC sampling to have finished before any
        // subsequent loads/stores start, docs say that we should put an LFENCE
        // immediately after the RDTSC/RDTSCP.
        if (FLAGS
            & (GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS
                | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES))
            != GET_TICKS_SYNC_FLAG_NONE
        {
            // SAFETY: `lfence` serializes instruction execution and loads with no side effects
            // beyond memory ordering. `nomem` and `readonly` are omitted to preserve the compiler
            // memory barrier matching `__asm__ __volatile__("lfence" ::: "memory")`.
            unsafe {
                asm!("lfence", options(nostack, preserves_flags));
            }
        }

        ret
    } else {
        core::hint::cold_path();
        match wall_clock {
            ClockSource::Unselected => 0,
            ClockSource::Pit => {
                // In theory, we should not need anything special to synchronize the
                // PIT. Right now, the PIT is just a global counter incremented by an
                // IRQ handler when the interrupt timer fires once per msec, and Intel's
                // memory model is strongly ordered, implying that no special
                // synchronization should be required.
                current_ticks_pit()
            }
            ClockSource::Hpet => {
                // TODO(johngro): Research and apply any barriers required to
                // synchronize observations of the HPET with the instruction pipeline.
                // Right now, we almost never use the HPET as our reference, which
                // somewhat lowers the priority of this issue.
                current_ticks_hpet()
            }
            ClockSource::Tsc => unimplemented!(),
        }
    }
}

/// Reads the current raw ticks counter without additional synchronization barriers.
#[inline]
pub fn platform_current_raw_ticks() -> zx_ticks_t {
    platform_current_raw_ticks_synchronized::<{ GET_TICKS_SYNC_FLAG_NONE }>()
}

/// Explicit instantiation of all of the forms of synchronized tick access.
macro_rules! expand_platform_current_raw_ticks_synchronized {
    ($($name:ident = $flags:expr;)*) => {$(
        #[doc = concat!(
            "FFI entry point for `platform_current_raw_ticks_synchronized` with flags ",
            stringify!($flags),
            "."
        )]
        #[unsafe(no_mangle)]
        pub extern "C" fn $name() -> zx_ticks_t {
            platform_current_raw_ticks_synchronized::<{ $flags }>()
        }
    )*};
}

expand_platform_current_raw_ticks_synchronized! {
    rust_platform_current_raw_ticks_synchronized_0 = 0;
    rust_platform_current_raw_ticks_synchronized_1 = 1;
    rust_platform_current_raw_ticks_synchronized_2 = 2;
    rust_platform_current_raw_ticks_synchronized_3 = 3;
    rust_platform_current_raw_ticks_synchronized_4 = 4;
    rust_platform_current_raw_ticks_synchronized_5 = 5;
    rust_platform_current_raw_ticks_synchronized_6 = 6;
    rust_platform_current_raw_ticks_synchronized_7 = 7;
    rust_platform_current_raw_ticks_synchronized_8 = 8;
    rust_platform_current_raw_ticks_synchronized_9 = 9;
    rust_platform_current_raw_ticks_synchronized_10 = 10;
    rust_platform_current_raw_ticks_synchronized_11 = 11;
    rust_platform_current_raw_ticks_synchronized_12 = 12;
    rust_platform_current_raw_ticks_synchronized_13 = 13;
    rust_platform_current_raw_ticks_synchronized_14 = 14;
    rust_platform_current_raw_ticks_synchronized_15 = 15;
}

/// Converts a raw TSC duration in ticks to nanoseconds.
pub fn convert_raw_tsc_duration_to_nanoseconds(duration: i64) -> zx_duration_t {
    // SAFETY: `RDTSC_TICKS_TO_CLOCK_MONOTONIC` is initialized once during single-threaded boot in
    // `calibrate_tsc` and is read-only thereafter.
    let ratio = unsafe { RDTSC_TICKS_TO_CLOCK_MONOTONIC };
    ratio.scale::<{ Round::DOWN }>(duration)
}

/// Converts a raw TSC timestamp to the clock monotonic timeline in nanoseconds.
pub fn convert_raw_tsc_timestamp_to_clock_monotonic(ts: i64) -> zx_instant_mono_t {
    // SAFETY: `WALL_CLOCK` and `RDTSC_TICKS_TO_CLOCK_MONOTONIC` are initialized once during
    // single-threaded boot in `pc_init_timer` and are read-only thereafter.
    let (wall_clock, rdtsc_ticks_to_clock_monotonic) =
        unsafe { (WALL_CLOCK, RDTSC_TICKS_TO_CLOCK_MONOTONIC) };
    if wall_clock == ClockSource::Tsc {
        // If TSC is being used as our clock monotonic reference, then conversion is
        // simple.  We just need to convert from the raw TSC timestamps to a ticks
        // timestamp by adding the offset, then scale by the ticks -> mono ratio.
        // As the offset is only updated early during boot when we're running on a
        // single core with interrupts disabled, we don't need to worry about thread
        // synchronization so memory_order_relaxed is sufficient.
        let abs_ticks: i64 = ts.wrapping_add(timer_get_mono_ticks_offset());
        rdtsc_ticks_to_clock_monotonic.scale::<{ Round::DOWN }>(abs_ticks)
    } else {
        core::hint::cold_path();
        // If we are using something other than TSC as our monotonic reference, then
        // things are slightly more tricky.  We need to figure out how far in the
        // future this TSC timestamp is (in nanoseconds), and then add that delta to
        // the current time to establish the new deadline.
        //
        // Bracket our observation of current time with two observations of ticks,
        // and use the average of those two values to create the ticks half of the
        // correspondence pair.
        let before_tsc = current_ticks_rdtsc() as u64;
        let now_mono: zx_instant_mono_t = current_mono_time().0;
        let after_tsc = current_ticks_rdtsc() as u64;
        let now_tsc: u64 = (before_tsc >> 1) + (after_tsc >> 1) + (before_tsc & after_tsc & 1);
        let time_till_tsc_timestamp: i64 = ts.saturating_sub(now_tsc as i64);
        now_mono.saturating_add(
            rdtsc_ticks_to_clock_monotonic.scale::<{ Round::DOWN }>(time_till_tsc_timestamp),
        )
    }
}

/// i8253/i8254 programmable interval timer control register port.
const I8253_CONTROL_REG: u16 = 0x43;
/// i8253/i8254 programmable interval timer data register port.
const I8253_DATA_REG: u16 = 0x40;

/// The PIT timer will keep track of wall time if we aren't using the TSC.
extern "C" fn pit_timer_tick(_cookie: *mut c_void) {
    // Relaxed ordering is sufficient because pit_timer_tick is the sole writer (serialized by
    // the PIT IRQ) and readers only observe the counter value itself.
    PIT_TICKS.store(PIT_TICKS.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
}

/// The APIC timers will call this when they fire.
///
/// # Safety
/// Must only be called from the APIC timer interrupt handler with interrupts disabled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn platform_handle_apic_timer_tick() {
    // SAFETY: Caller guarantees we are in the APIC timer interrupt handler with interrupts
    // disabled.
    unsafe {
        timer_tick();
    }
}

fn set_pit_frequency(frequency: u32) {
    let mut count: u32;
    let mut remainder: u32;

    // figure out the correct pit_divisor for the desired frequency
    if frequency <= 18 {
        count = 0xffff;
    } else if frequency >= INTERNAL_FREQ {
        count = 1;
    } else {
        count = INTERNAL_FREQ_3X / frequency;
        remainder = INTERNAL_FREQ_3X % frequency;

        if remainder >= INTERNAL_FREQ_3X / 2 {
            count += 1;
        }

        count /= 3;
        remainder = count % 3;

        if remainder >= 1 {
            count += 1;
        }
    }

    let pit_divisor = (count & 0xffff) as u16;
    // SAFETY: Single-threaded boot initialization in `pc_init_timer`.
    unsafe {
        PIT_DIVISOR = pit_divisor;

        // funky math that i don't feel like explaining. essentially 32.32 fixed
        // point representation of the configured timer delta.
        fp_32_64_div_32_32(
            NonNull::new_unchecked(&raw mut US_PER_PIT),
            (1000u32 * 1000 * 3).wrapping_mul(count),
            INTERNAL_FREQ_3X,
        );
    }

    // dprintf(DEBUG, "set_pit_frequency: pit_divisor=%04x\n", pit_divisor);

    // setup the Programmable Interval Timer
    // timer 0, mode 2, binary counter, LSB followed by MSB
    // SAFETY: Port I/O to I8253 PIT control and data registers during boot initialization.
    unsafe {
        outp(I8253_CONTROL_REG, 0x34);
        outp(I8253_DATA_REG, pit_divisor as u8); // LSB
        outp(I8253_DATA_REG, (pit_divisor >> 8) as u8); // MSB
    }
}

#[inline]
fn pit_calibration_cycle_preamble(ms: u16) {
    // Make the PIT run for
    let init_pic_count = (INTERNAL_FREQ_TICKS_PER_MS * u32::from(ms)) as u16;
    // Program PIT in the interrupt on terminal count configuration,
    // this makes it count down and set the output high when it hits 0.
    // SAFETY: Port I/O to I8253 PIT registers during single-threaded timer calibration.
    unsafe {
        outp(I8253_CONTROL_REG, 0x30);
        outp(I8253_DATA_REG, init_pic_count as u8); // LSB
    }
}

#[inline]
fn pit_calibration_cycle(ms: u16) {
    // Make the PIT run for ms millis, see comments in the preamble
    let init_pic_count = (INTERNAL_FREQ_TICKS_PER_MS * u32::from(ms)) as u16;
    // SAFETY: Port I/O to I8253 PIT registers during single-threaded timer calibration.
    unsafe {
        outp(I8253_DATA_REG, (init_pic_count >> 8) as u8); // MSB
    }

    let mut status: u8;
    loop {
        // Send a read-back command that latches the status of ch0
        // SAFETY: Port I/O to I8253 PIT registers during single-threaded timer calibration.
        unsafe {
            outp(I8253_CONTROL_REG, 0xe2);
            status = inp(I8253_DATA_REG);
        }
        // Wait for bit 7 (output) to go high and for bit 6 (null count) to go low
        if (status & 0xc0) == 0x80 {
            break;
        }
    }
}

#[inline]
fn pit_calibration_cycle_cleanup() {
    // Stop the PIT by starting a mode change but not writing a counter
    // SAFETY: Port I/O to I8253 PIT control register during single-threaded timer calibration.
    unsafe {
        outp(I8253_CONTROL_REG, 0x38);
    }
}

#[inline]
fn hpet_calibration_cycle_preamble() {
    hpet_enable();
}

#[inline]
fn hpet_calibration_cycle(ms: u16) {
    hpet_wait_ms(ms);
}

#[inline]
fn hpet_calibration_cycle_cleanup() {
    hpet_disable();
}

fn calibrate_apic_timer() {
    assert!(ints_disabled());

    let apic_freq: u64 = x86_lookup_core_crystal_freq();
    if apic_freq != 0 {
        assert!(apic_freq / 1000 <= u64::from(u32::MAX));
        let apic_ticks_per_ms = (apic_freq / 1000) as u32;
        // SAFETY: Single-threaded boot calibration in `pc_init_timer`.
        unsafe {
            APIC_TICKS_PER_MS = apic_ticks_per_ms;
            APIC_DIVISOR = 1;
            fp_32_64_div_32_32(
                NonNull::new_unchecked(&raw mut APIC_TICKS_PER_NS),
                apic_ticks_per_ms,
                1000 * 1000,
            );
        }
        kprint!("APIC frequency: {} ticks/ms\n", apic_ticks_per_ms);
        return;
    }

    // SAFETY: `CALIBRATION_CLOCK` is set in `pc_init_timer` before calling `calibrate_apic_timer`.
    let calibration_clock = unsafe { CALIBRATION_CLOCK };
    kprint!(
        "Could not find APIC frequency: Calibrating APIC with {:s}\n",
        CLOCK_NAME[calibration_clock as usize]
    );

    let mut apic_divisor: u8 = 1;
    let mut apic_ticks_per_ms: u32 = 0;
    'outer: while apic_divisor != 0 {
        let mut best_time: [u32; 2] = [u32::MAX, u32::MAX];
        const DURATION_MS: [u16; 2] = [2, 4];
        for trial in 0..2 {
            for tries in 0..3 {
                match calibration_clock {
                    ClockSource::Hpet => hpet_calibration_cycle_preamble(),
                    ClockSource::Pit => pit_calibration_cycle_preamble(DURATION_MS[trial]),
                    _ => unimplemented!(),
                }

                // Setup APIC timer to count down with interrupt masked
                let status = apic_timer_set_oneshot(u32::MAX, apic_divisor, true);
                assert!(status.is_ok());

                match calibration_clock {
                    ClockSource::Hpet => hpet_calibration_cycle(DURATION_MS[trial]),
                    ClockSource::Pit => pit_calibration_cycle(DURATION_MS[trial]),
                    _ => unimplemented!(),
                }

                let apic_ticks: u32 = u32::MAX - apic_timer_current_count();
                if apic_ticks < best_time[trial] {
                    best_time[trial] = apic_ticks;
                }
                ltracef!("Calibration trial {} found {} ticks/ms\n", tries, apic_ticks);

                match calibration_clock {
                    ClockSource::Hpet => hpet_calibration_cycle_cleanup(),
                    ClockSource::Pit => pit_calibration_cycle_cleanup(),
                    _ => unimplemented!(),
                }
            }

            // If the APIC ran out of time every time, try again with a higher
            // divisor
            if best_time[trial] == u32::MAX {
                apic_divisor = apic_divisor.wrapping_mul(2);
                continue 'outer;
            }
        }
        apic_ticks_per_ms =
            (best_time[1] - best_time[0]) / u32::from(DURATION_MS[1] - DURATION_MS[0]);
        // SAFETY: Single-threaded boot calibration in `pc_init_timer`.
        unsafe {
            fp_32_64_div_32_32(
                NonNull::new_unchecked(&raw mut APIC_TICKS_PER_NS),
                apic_ticks_per_ms,
                1000 * 1000,
            );
        }
        break;
    }
    assert!(apic_divisor != 0);

    // SAFETY: Single-threaded boot calibration in `pc_init_timer`.
    unsafe {
        APIC_DIVISOR = apic_divisor;
        APIC_TICKS_PER_MS = apic_ticks_per_ms;
    }

    kprint!("APIC timer calibrated: {} ticks/ms, divisor {}\n", apic_ticks_per_ms, apic_divisor);
}

fn calibrate_tsc_count(duration_ms: u16) -> u64 {
    let mut best_time: zx_ticks_t = zx_ticks_t::MAX;
    // SAFETY: `CALIBRATION_CLOCK` is set in `pc_init_timer` before calling `calibrate_tsc_count`.
    let calibration_clock = unsafe { CALIBRATION_CLOCK };

    for tries in 0..3 {
        match calibration_clock {
            ClockSource::Hpet => hpet_calibration_cycle_preamble(),
            ClockSource::Pit => pit_calibration_cycle_preamble(duration_ms),
            _ => unimplemented!(),
        }

        serialize_instructions();
        // SAFETY: `_rdtsc` is always safe to execute in ring 0 on x86_64.
        let start: u64 = unsafe { _rdtsc() };
        serialize_instructions();

        match calibration_clock {
            ClockSource::Hpet => hpet_calibration_cycle(duration_ms),
            ClockSource::Pit => pit_calibration_cycle(duration_ms),
            _ => unimplemented!(),
        }

        serialize_instructions();
        // SAFETY: `_rdtsc` is always safe to execute in ring 0 on x86_64.
        let end: zx_ticks_t = unsafe { _rdtsc() as zx_ticks_t };
        serialize_instructions();

        let tsc_ticks: zx_ticks_t = end.wrapping_sub(start as zx_ticks_t);
        if tsc_ticks < best_time {
            best_time = tsc_ticks;
        }
        ltracef!("Calibration trial {} found {} ticks/ms\n", tries, tsc_ticks);
        match calibration_clock {
            ClockSource::Hpet => hpet_calibration_cycle_cleanup(),
            ClockSource::Pit => pit_calibration_cycle_cleanup(),
            _ => unimplemented!(),
        }
    }

    best_time as u64
}

fn calibrate_tsc(has_pv_clock: bool) {
    assert!(ints_disabled());

    let tsc_freq: u64 = if has_pv_clock { pv_clock_get_tsc_freq() } else { x86_lookup_tsc_freq() };
    let tsc_ticks_per_ms: u64;
    if tsc_freq != 0 {
        let mut n: u64 = 1_000_000_000;
        let mut d: u64 = tsc_freq;
        Ratio::reduce_u64(&mut n, &mut d);

        // ASSERT that we can represent this as a 32 bit ratio.  If we cannot,
        // it means that tsc_freq is a number so large, and with so few prime
        // factors of 2 and 5, that it cannot be reduced to fit into a 32 bit
        // integer.  This is pretty unreasonable, for now, just assert that it
        // will not happen.
        assert!(
            (n <= u64::from(u32::MAX)) && (d <= u64::from(u32::MAX)),
            "Clock monotonic ticks : RDTSC ticks ratio ({} : {}) too large to store in a 32 bit \
             ratio!!",
            n,
            d
        );
        // SAFETY: Single-threaded boot calibration in `pc_init_timer`.
        unsafe {
            RDTSC_TICKS_TO_CLOCK_MONOTONIC = Ratio::new(n as u32, d as u32);
        }

        tsc_ticks_per_ms = tsc_freq / 1000;
        // SAFETY: Single-threaded boot calibration in `pc_init_timer`.
        unsafe {
            TSC_TICKS_PER_MS = tsc_ticks_per_ms;
        }
        kprint!("TSC frequency: {} ticks/ms\n", tsc_ticks_per_ms);
    } else {
        // SAFETY: `CALIBRATION_CLOCK` is set in `pc_init_timer` before calling `calibrate_tsc`.
        let calibration_clock = unsafe { CALIBRATION_CLOCK };
        kprint!(
            "Could not find TSC frequency: Calibrating TSC with {:s}\n",
            CLOCK_NAME[calibration_clock as usize]
        );

        let mut duration_ms: [u32; 2] = [2, 4];
        let mut best_time: [u64; 2] = [
            calibrate_tsc_count(duration_ms[0] as u16),
            calibrate_tsc_count(duration_ms[1] as u16),
        ];

        while best_time[0] >= best_time[1] && i64::from(2 * duration_ms[1]) < MAX_TIMER_INTERVAL {
            core::hint::cold_path();
            duration_ms[0] = duration_ms[1];
            duration_ms[1] *= 2;
            best_time[0] = best_time[1];
            best_time[1] = calibrate_tsc_count(duration_ms[1] as u16);
        }

        assert!(best_time[0] < best_time[1]);

        let tsc_ticks_per_sec: u64 =
            ((best_time[1] - best_time[0]) * 1000) / u64::from(duration_ms[1] - duration_ms[0]);

        assert!(
            tsc_ticks_per_sec <= u64::from(u32::MAX),
            "Estimated TSC ({}) is too high!\n",
            tsc_ticks_per_sec
        );

        tsc_ticks_per_ms = tsc_ticks_per_sec / 1000;
        // SAFETY: Single-threaded boot calibration in `pc_init_timer`.
        unsafe {
            TSC_TICKS_PER_MS = tsc_ticks_per_ms;
            RDTSC_TICKS_TO_CLOCK_MONOTONIC = Ratio::new(1_000_000_000, tsc_ticks_per_sec as u32);
        }

        kprint!("TSC calibrated: {} ticks/ms\n", tsc_ticks_per_ms);
    }

    assert!(tsc_ticks_per_ms <= u64::from(u32::MAX));
    // SAFETY: Single-threaded boot calibration in `pc_init_timer`.
    let ns_per_tsc = unsafe {
        fp_32_64_div_32_32(
            NonNull::new_unchecked(&raw mut NS_PER_TSC),
            1000 * 1000,
            tsc_ticks_per_ms as u32,
        );
        NS_PER_TSC
    };

    ltracef!("ns_per_tsc: {:08x}.{:08x}{:08x}\n", ns_per_tsc.l0, ns_per_tsc.l32, ns_per_tsc.l64);
}

fn pc_init_timer(_level: init::LkInitLevel) {
    let cpu_model: &X86ModelInfo = x86_get_model();
    // Declares the desired PIT frequency to be 1000, which gives us ~1ms granularity.
    // This may not be used if we chose to use a different platform reference timer.
    const DESIRED_PIT_FREQUENCY: u32 = 1000;

    let mut constant_tsc = false;
    if x86_get_vendor() == X86VendorList::X86_VENDOR_INTEL {
        // This condition taken from Intel 3B 17.15 (Time-Stamp Counter).  This
        // is the negation of the non-Constant TSC section, since the Constant
        // TSC section is incomplete (the behavior is architectural going
        // forward, and modern CPUs are not on the list).
        constant_tsc = !((cpu_model.family == 0x6 && cpu_model.model == 0x9)
            || (cpu_model.family == 0x6 && cpu_model.model == 0xd)
            || (cpu_model.family == 0xf && cpu_model.model < 0x3));
    }
    let mut invariant_tsc = x86_feature_test(X86_FEATURE_INVAR_TSC);

    let mut has_pv_clock = x86_hypervisor_has_pv_clock();
    if has_pv_clock {
        let status = pv_clock_init();
        if status.is_ok() {
            invariant_tsc = pv_clock_is_stable();
        } else {
            has_pv_clock = false;
        }
    }

    // SAFETY: Single-threaded boot initialization in `pc_init_timer`.
    unsafe {
        CONSTANT_TSC = constant_tsc;
        INVARIANT_TSC = invariant_tsc;
    }

    let has_hpet = hpet_is_present();
    if has_hpet {
        let hpet_ms_rate: u64 = hpet_ticks_per_ms();
        assert!(hpet_ms_rate <= u64::from(u32::MAX));
        kprint!("HPET frequency: {} ticks/ms\n", hpet_ms_rate);
        // SAFETY: Single-threaded boot initialization in `pc_init_timer`.
        unsafe {
            CALIBRATION_CLOCK = ClockSource::Hpet;
            fp_32_64_div_32_32(
                NonNull::new_unchecked(&raw mut NS_PER_HPET),
                1000 * 1000,
                hpet_ms_rate as u32,
            );
        }
    } else {
        // SAFETY: Single-threaded boot initialization in `pc_init_timer`.
        unsafe {
            CALIBRATION_CLOCK = ClockSource::Pit;
        }
    }

    let force_wallclock = BootOptions::get().x86_wallclock != WallclockType::AutoDetect;
    let use_invariant_tsc = invariant_tsc
        && (!force_wallclock || BootOptions::get().x86_wallclock == WallclockType::Tsc);

    let use_tsc_deadline = use_invariant_tsc && x86_feature_test(X86_FEATURE_TSC_DEADLINE);
    // SAFETY: Single-threaded boot initialization in `pc_init_timer`.
    unsafe {
        USE_TSC_DEADLINE = use_tsc_deadline;
    }
    if use_tsc_deadline {
        apic_timer_tsc_deadline_init();
    } else {
        calibrate_apic_timer();
    }

    let wall_clock: ClockSource;
    if use_invariant_tsc {
        calibrate_tsc(has_pv_clock);

        // Program PIT in the software strobe configuration, but do not load
        // the count.  This will pause the PIT.
        // SAFETY: Port I/O to I8253 PIT control register during boot initialization.
        unsafe {
            outp(I8253_CONTROL_REG, 0x38);
        }

        // Set up our wall clock to rdtsc, and stash the initial
        // transformation from ticks to clock monotonic.
        //
        // We cannot (or at least, really should not) reset the TSC to zero, so
        // instead we use the time of clock selection ("now" according to the TSC)
        // to define the zero point on our ticks timeline moving forward.
        // SAFETY: Single-threaded boot initialization on the boot CPU before secondary CPUs start.
        unsafe {
            timer_set_ticks_to_time_ratio(RDTSC_TICKS_TO_CLOCK_MONOTONIC);
            timer_set_initial_ticks(current_ticks_rdtsc() as u64);
        }

        // A note about this wrapping negation.  There is a technical risk of
        // signed overflow if -mono_ticks_offset is a value too large to fit
        // into a signed 64 bit integer (i.e. >= 2^63).
        //
        // This _should_ never happen, however.  This offset is the two's complement
        // of what the TSC read when we decided that the ticks timeline should be
        // zero.  For -mono_ticks_offset to be >= 2^63, the TSC counter
        // value itself would have needed to be >= 2^63 in the line above where it
        // was sampled.  Assuming that the TSC started to count from 0 at cold power
        // on time, and assuming that the TSC was running extremely quickly (say,
        // 5GHz), the system would have needed to be powered on for at least ~58.45
        // years before we hit this mark (and this assumes that the TSC is not reset
        // during a warm reboot, or that no warm reboots take place over almost 60
        // years of uptime).  So, for now, we perform the wrapping negation,
        // assuming that nothing bad will happen.
        wall_clock = ClockSource::Tsc;
        // SAFETY: Single-threaded boot initialization in `pc_init_timer`.
        unsafe {
            EARLY_TICKS_TO_TICKS =
                Transform::new(timer_get_mono_ticks_offset().wrapping_neg(), 0, Ratio::new(1, 1));
            WALL_CLOCK = wall_clock;
        }
    } else {
        if constant_tsc || invariant_tsc {
            // Calibrate the TSC even though it's not as good as we want, so we
            // can still let folks still use it for cheap timing.
            calibrate_tsc(has_pv_clock);
        }

        if has_hpet && (!force_wallclock || BootOptions::get().x86_wallclock == WallclockType::Hpet)
        {
            // Set up our wall clock to the HPET, and stash the initial
            // transformation from ticks to clock monotonic.
            // SAFETY: Single-threaded boot initialization on the boot CPU.
            unsafe {
                timer_set_ticks_to_time_ratio(HPET_TICKS_TO_CLOCK_MONOTONIC);
                timer_set_initial_ticks(0);
            }

            // Explicitly set the value of the HPET to zero, then make sure it is
            // started.  Take a correspondence pair between HPET and TSC by observing
            // TSC after we start the HPET so we can define the transformation between
            // TSC (the EarlyTicks reference) and HPET.
            //
            // Note: we do not bother to bracket the observation of HPET with a TSC
            // observation before and after.  We are at a point in the boot where we
            // are running on a single core, and should not be taking exceptions or
            // interrupts yet.  TL;DR, this observation should be "good enough"
            // without any need for averaging.
            let _ = hpet_set_value(0);
            hpet_enable();
            let tsc_reference: zx_ticks_t = current_ticks_rdtsc();

            // Now set up our transformation from EarlyTicks (using TSC as a
            // reference) and HPET (the reference for the zx_ticks_get timeline).
            // SAFETY: `RDTSC_TICKS_TO_CLOCK_MONOTONIC` and `HPET_TICKS_TO_CLOCK_MONOTONIC` are
            // initialized during single-threaded boot.
            let (rdtsc_ratio, hpet_ratio) =
                unsafe { (RDTSC_TICKS_TO_CLOCK_MONOTONIC, HPET_TICKS_TO_CLOCK_MONOTONIC) };
            let rdtsc_ticks_to_hpet_ticks =
                Ratio::product(rdtsc_ratio, hpet_ratio.inverse(), Exact::No);
            // SAFETY: Single-threaded boot initialization in `pc_init_timer`.
            unsafe {
                EARLY_TICKS_TO_TICKS = Transform::new(tsc_reference, 0, rdtsc_ticks_to_hpet_ticks);
            }

            // HPET is now our chosen "ticks" reference.
            wall_clock = ClockSource::Hpet;
            // SAFETY: Single-threaded boot initialization in `pc_init_timer`.
            unsafe {
                WALL_CLOCK = wall_clock;
            }
        } else {
            if force_wallclock && BootOptions::get().x86_wallclock != WallclockType::Pit {
                panic!("Could not satisfy kernel.wallclock choice\n");
            }

            // Set up our wall clock to pit, and stash the initial
            // transformation from ticks to clock monotonic.
            // SAFETY: Single-threaded boot initialization on the boot CPU.
            unsafe {
                timer_set_ticks_to_time_ratio(Ratio::new(1_000_000, 1));
            }

            set_pit_frequency(DESIRED_PIT_FREQUENCY);

            let irq: u32 = apic_io_isa_to_global(ISA_IRQ_PIT);
            let status = register_permanent_int_handler(
                irq,
                InterruptHandler { cookie: core::ptr::null_mut(), r#fn: Some(pit_timer_tick) },
            );
            debug_assert!(status.is_ok());
            let _ = unmask_interrupt(irq);

            // See the HPET code above.  Observe the value of TSC as we figure out the
            // PIT offset so that we can define a function which maps EarlyTicks to
            // ticks.
            // SAFETY: Single-threaded boot initialization on the boot CPU.
            unsafe {
                timer_set_initial_ticks(current_ticks_pit() as u64);
            }
            let tsc_reference: zx_ticks_t = current_ticks_rdtsc();

            // SAFETY: `RDTSC_TICKS_TO_CLOCK_MONOTONIC` is initialized during single-threaded boot.
            let rdtsc_ticks_to_pit_ticks = unsafe {
                Ratio::product(RDTSC_TICKS_TO_CLOCK_MONOTONIC, Ratio::new(1, 1_000_000), Exact::No)
            };

            // Note, see the comment above in the TSC section for why it is considered
            // to be reasonably safe to perform the wrapping negation here.
            // SAFETY: Single-threaded boot initialization in `pc_init_timer`.
            unsafe {
                EARLY_TICKS_TO_TICKS = Transform::new(
                    tsc_reference,
                    timer_get_mono_ticks_offset().wrapping_neg(),
                    rdtsc_ticks_to_pit_ticks,
                );
            }

            // PIT is now our chosen "ticks" reference.
            wall_clock = ClockSource::Pit;
            // SAFETY: Single-threaded boot initialization in `pc_init_timer`.
            unsafe {
                WALL_CLOCK = wall_clock;
            }
        }
    }

    // Now that we've decided on which wall_clock to use as our timer reference, set up the ratio
    // that converts from reference timer ticks to APIC ticks.
    // SAFETY: `APIC_TICKS_PER_MS` is only modified during single-threaded boot in `pc_init_timer`.
    let apic_ticks_per_ms = unsafe { APIC_TICKS_PER_MS };
    match wall_clock {
        ClockSource::Unselected => {
            panic!("Wall clock was unselected by the time pc_init_timer completed\n");
        }
        ClockSource::Tsc => {
            // SAFETY: `TSC_TICKS_PER_MS` is initialized in `calibrate_tsc`.
            let tsc_ticks_per_ms = unsafe { TSC_TICKS_PER_MS };
            assert!(tsc_ticks_per_ms <= u64::from(u32::MAX));
            // SAFETY: Single-threaded boot initialization in `pc_init_timer`.
            unsafe {
                REFERENCE_TIMER_TICKS_TO_APIC_TICKS =
                    Ratio::new(apic_ticks_per_ms, tsc_ticks_per_ms as u32);
            }
        }
        ClockSource::Hpet => {
            let hpet_ticks_ms: u64 = hpet_ticks_per_ms();
            assert!(hpet_ticks_ms <= u64::from(u32::MAX));
            // SAFETY: Single-threaded boot initialization in `pc_init_timer`.
            unsafe {
                REFERENCE_TIMER_TICKS_TO_APIC_TICKS =
                    Ratio::new(apic_ticks_per_ms, hpet_ticks_ms as u32);
            }
        }
        ClockSource::Pit => {
            // Here's how we computed the ms_per_pit ratio:
            //
            // count = INTERNAL_FREQ_3X/DESIRED_PIT_FREQUENCY
            // ms/pit = (3000 * count) / INTERNAL_FREQ_3X
            //        = (3000 * (INTERNAL_FREQ_3X/DESIRED_PIT_FREQUENCY))/ INTERNAL_FREQ_3X
            //        = (3000 * INTERNAL_FREQ_3X) / (DESIRED_PIT_FREQUENCY * INTERNAL_FREQ_3X)
            //        = 3000/DESIRED_PIT_FREQUENCY
            let ms_per_pit = Ratio::new(3000, DESIRED_PIT_FREQUENCY);
            let apic_per_ms = Ratio::new(apic_ticks_per_ms, 1);
            // SAFETY: Single-threaded boot initialization in `pc_init_timer`.
            unsafe {
                REFERENCE_TIMER_TICKS_TO_APIC_TICKS =
                    Ratio::product(apic_per_ms, ms_per_pit, Exact::Yes);
            }
        }
    }

    kprint!(
        "timer features: constant_tsc {} invariant_tsc {} tsc_deadline {}\n",
        u32::from(constant_tsc),
        u32::from(invariant_tsc),
        u32::from(use_tsc_deadline)
    );
    kprint!("Using {:s} as wallclock\n", CLOCK_NAME[wall_clock as usize]);
}
init::lk_init_hook!(timer, pc_init_timer, init::LkInitLevel(init::LK_INIT_LEVEL_VM.0 + 3));

/// Converts the given duration's units from the platform's selected tick source to APIC ticks.
fn apic_ticks_from_platform_ticks(interval: zx_duration_t) -> u64 {
    // SAFETY: `WALL_CLOCK` and `REFERENCE_TIMER_TICKS_TO_APIC_TICKS` are initialized once during
    // single-threaded boot in `pc_init_timer` and are read-only thereafter.
    let (wall_clock, reference_timer_ticks_to_apic_ticks) =
        unsafe { (WALL_CLOCK, REFERENCE_TIMER_TICKS_TO_APIC_TICKS) };
    debug_assert!(wall_clock != ClockSource::Unselected);
    reference_timer_ticks_to_apic_ticks.scale::<{ Round::UP }>(interval) as u64
}

/// Programs the local APIC oneshot timer to fire at the given `deadline` in raw ticks.
pub fn platform_set_oneshot_timer(mut deadline: zx_ticks_t) -> Result<(), Status> {
    debug_assert!(ints_disabled());
    // We use 1 tick as the minimum deadline here because we want a deadline that immediately fires
    // the timer, but we can't use 0 because setting a TSC deadline to zero disables the APIC timer.
    deadline = deadline.max(1);

    // SAFETY: `USE_TSC_DEADLINE` is initialized once during single-threaded boot in `pc_init_timer`
    // and is read-only thereafter.
    if unsafe { USE_TSC_DEADLINE } {
        ltracef!("Scheduling oneshot timer: {} deadline\n", deadline);
        apic_timer_set_tsc_deadline(deadline as u64);
        PLATFORM_TIMER_SET_COUNTER.add(1);
        return Ok(());
    }

    core::hint::cold_path();
    let now: zx_ticks_t = platform_current_raw_ticks();
    if now >= deadline {
        core::hint::cold_path();
        // Deadline has already passed. We still need to schedule a timer so that
        // the interrupt fires.
        ltracef!("Scheduling oneshot timer for min duration\n");
        PLATFORM_TIMER_SET_COUNTER.add(1);
        return apic_timer_set_oneshot(1, 1, false /* unmasked */);
    }

    let interval: zx_duration_t = deadline.saturating_sub(now);
    debug_assert!(interval > 0);

    // Convert the interval, which is in platform reference timer ticks, to APIC timer ticks.
    let apic_ticks_needed: u64 = apic_ticks_from_platform_ticks(interval);
    debug_assert!(apic_ticks_needed > 0);

    // Find the shift needed for this timeout, since count is 32-bit.
    let highest_set_bit: u32 = apic_ticks_needed.checked_ilog2().unwrap_or(0);
    let mut extra_shift: u8 = if highest_set_bit <= 31 { 0 } else { (highest_set_bit - 31) as u8 };
    if extra_shift > 8 {
        extra_shift = 8;
    }

    // SAFETY: `APIC_DIVISOR` is initialized once during single-threaded boot in
    // `calibrate_apic_timer` and is read-only thereafter.
    let mut divisor: u32 = u32::from(unsafe { APIC_DIVISOR }) << extra_shift;
    let mut count: u32;
    // If the divisor is too large, we're at our maximum timeout.  Saturate the
    // timer.  It'll fire earlier than requested, but the scheduler will notice
    // and ask us to set the timer up again.
    if divisor <= 128 {
        count = (apic_ticks_needed >> extra_shift) as u32;
        debug_assert!((apic_ticks_needed >> extra_shift) <= u64::from(u32::MAX));
    } else {
        core::hint::cold_path();
        divisor = 128;
        count = u32::MAX;
    }

    // Make sure we're not underflowing
    if count == 0 {
        core::hint::cold_path();
        debug_assert!(divisor == 1);
        count = 1;
    }

    ltracef!("Scheduling oneshot timer: {} count, {} div\n", count, divisor);
    PLATFORM_TIMER_SET_COUNTER.add(1);
    apic_timer_set_oneshot(count, divisor as u8, false /* unmasked */)
}

/// Stops the calling CPU's hardware oneshot timer.
pub fn platform_stop_timer() {
    // Enable interrupt mode that will stop the decreasing counter of the PIT
    // outp(I8253_CONTROL_REG, 0x30);
    // SAFETY: `USE_TSC_DEADLINE` is initialized once during single-threaded boot in `pc_init_timer`
    // and is read-only thereafter.
    if unsafe { USE_TSC_DEADLINE } {
        // In TSC deadline mode, a deadline of 0 disarms the LAPIC timer
        apic_timer_set_tsc_deadline(0);
    } else {
        core::hint::cold_path();
        apic_timer_stop();
    }
    PLATFORM_TIMER_CANCEL_COUNTER.add(1);
}

/// Shuts down the platform timer on the calling CPU.
pub fn platform_shutdown_timer() {
    debug_assert!(ints_disabled());

    if x86_hypervisor_has_pv_clock() && curr_cpu_num() == 0 {
        pv_clock_shutdown();
    }
}

/// Suspends the platform timer on the current CPU.
pub fn platform_suspend_timer_curr_cpu() -> Result<(), Status> {
    Err(Status::NOT_SUPPORTED)
}

/// Resumes the platform timer on the current CPU.
pub fn platform_resume_timer_curr_cpu() -> Result<(), Status> {
    Err(Status::NOT_SUPPORTED)
}

/// Represents a raw early tick sample collected before the platform timer is initialized.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EarlyTicks {
    /// Raw TSC value sampled during early boot.
    pub tsc: u64,
}

zr::static_assert_size_and_align!(EarlyTicks, 8, 8);

/// Converts an early boot tick sample to monotonic ticks.
pub fn platform_convert_early_ticks(sample: EarlyTicks) -> zx_instant_mono_ticks_t {
    // SAFETY: `EARLY_TICKS_TO_TICKS` is initialized once during single-threaded boot in
    // `pc_init_timer` and is read-only thereafter.
    let early_ticks_to_ticks = unsafe { EARLY_TICKS_TO_TICKS };
    early_ticks_to_ticks.apply::<{ Saturate::Yes }>(sample.tsc as i64)
}

/// Currently, usermode can access our source of ticks only if we have chosen TSC
/// to be our tick counter.  Otherwise, they will need to go through a syscall.
///
/// In theory, we can fix this, but it would require having the vDSO map some
/// read-only memory in the user mode process (either the HPET registers, or the
/// variable which represents the PIT timer).  Currently, doing this is not
/// something we support, and the vast majority of x64 systems that we run on
/// have an invariant TSC which is accessible from usermode.  For now, we just
/// take the syscall hit instead of attempting to get more fancy.
pub fn platform_usermode_can_access_tick_registers() -> bool {
    // SAFETY: `WALL_CLOCK` is initialized once during single-threaded boot in `pc_init_timer`
    // and is read-only thereafter.
    let wall_clock = unsafe { WALL_CLOCK };
    wall_clock == ClockSource::Tsc
}

/// Sets the conversion ratio from HPET ticks to clock monotonic nanoseconds.
///
/// # Safety
/// Must be called at most once during single-threaded HPET initialization (`LK_INIT_LEVEL_VM + 2`)
/// before `pc_init_timer` (`LK_INIT_LEVEL_VM + 3`).
pub(super) unsafe fn hpet_set_ticks_to_clock_monotonic(n: u32, d: u32) {
    // SAFETY: Caller guarantees single-threaded initialization before `pc_init_timer`.
    unsafe {
        HPET_TICKS_TO_CLOCK_MONOTONIC = Ratio::new(n, d);
    }
}

// TODO(https://fxbug.dev/576428709): Remove these C FFI entry points once all callers are migrated
// to Rust.

/// FFI entry point for [`convert_raw_tsc_duration_to_nanoseconds`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_convert_raw_tsc_duration_to_nanoseconds(duration: i64) -> zx_duration_t {
    convert_raw_tsc_duration_to_nanoseconds(duration)
}

/// FFI entry point for [`convert_raw_tsc_timestamp_to_clock_monotonic`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_convert_raw_tsc_timestamp_to_clock_monotonic(ts: i64) -> zx_instant_mono_t {
    convert_raw_tsc_timestamp_to_clock_monotonic(ts)
}

/// FFI entry point for [`platform_set_oneshot_timer`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_set_oneshot_timer(deadline: zx_ticks_t) -> Result<(), Status> {
    platform_set_oneshot_timer(deadline)
}

/// FFI entry point for [`platform_stop_timer`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_stop_timer() {
    platform_stop_timer();
}

/// FFI entry point for [`platform_shutdown_timer`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_shutdown_timer() {
    platform_shutdown_timer();
}

/// FFI entry point for [`platform_suspend_timer_curr_cpu`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_suspend_timer_curr_cpu() -> Result<(), Status> {
    platform_suspend_timer_curr_cpu()
}

/// FFI entry point for [`platform_resume_timer_curr_cpu`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_resume_timer_curr_cpu() -> Result<(), Status> {
    platform_resume_timer_curr_cpu()
}

/// FFI entry point for [`platform_convert_early_ticks`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_convert_early_ticks(sample: EarlyTicks) -> zx_instant_mono_ticks_t {
    platform_convert_early_ticks(sample)
}

/// FFI entry point for [`platform_usermode_can_access_tick_registers`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_usermode_can_access_tick_registers() -> bool {
    platform_usermode_can_access_tick_registers()
}
