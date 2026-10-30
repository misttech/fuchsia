// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2013, Google Inc. All rights reserved.
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::arch_rs::arm64::arch_quirks_needs_arm_erratum_858921_mitigation;
use crate::arch_rs::{curr_cpu_num, ints_disabled};
use crate::counters::define_kcounter;
use crate::kernel::deadline::{Deadline, InstantUnknown};
use crate::kernel::mp::wait_for_all_cpus_ready;
use crate::kernel::percpu::PerCpu;
use crate::pdev_interrupt::{
    InterruptHandler, InterruptVector, mask_interrupt, register_permanent_int_handler,
    unmask_interrupt,
};
use crate::platform_rs::timer::{
    timer_get_mono_ticks_offset, timer_set_initial_ticks, timer_set_ticks_to_time_ratio,
    timer_tick, zx_sec,
};
use affine::Ratio;
use boot_options::BootOptions;
use core::ffi::c_void;
use core::sync::atomic::{AtomicPtr, AtomicU8, AtomicU32, Ordering};
use debug::{dprintf, ltracef, ltracef_level};
use regio::arm64::{SysReg, spec};
use zbi::DcfgArmGenericTimerDriver;
use zx_status::Status;
use zx_types::zx_ticks_t;

const LOCAL_TRACE: u32 = 0;

// AArch64 timer control registers
const TIMER_REG_CNTKCTL: SysReg<spec::CNTKCTL_EL1, u64> = SysReg::new();
const TIMER_REG_CNTFRQ: SysReg<spec::CNTFRQ_EL0, u64> = SysReg::new();

// CNTP AArch64 registers
const TIMER_REG_CNTP_CTL: SysReg<spec::CNTP_CTL_EL0, u64> = SysReg::new();
const TIMER_REG_CNTP_CVAL: SysReg<spec::CNTP_CVAL_EL0, u64> = SysReg::new();
const TIMER_REG_CNTP_CTL_EL02: SysReg<spec::CNTP_CTL_EL02, u64> = SysReg::new();
const TIMER_REG_CNTP_CVAL_EL02: SysReg<spec::CNTP_CVAL_EL02, u64> = SysReg::new();
const TIMER_REG_CNTPCT: SysReg<spec::CNTPCT_EL0, u64> = SysReg::new();

// CNTPS "AArch64" registers
const TIMER_REG_CNTPS_CTL: SysReg<spec::CNTPS_CTL_EL1, u64> = SysReg::new();
const TIMER_REG_CNTPS_CVAL: SysReg<spec::CNTPS_CVAL_EL1, u64> = SysReg::new();

// CNTV "AArch64" registers
const TIMER_REG_CNTV_CTL: SysReg<spec::CNTV_CTL_EL0, u64> = SysReg::new();
const TIMER_REG_CNTV_CVAL: SysReg<spec::CNTV_CVAL_EL0, u64> = SysReg::new();
const TIMER_REG_CNTV_CTL_EL02: SysReg<spec::CNTV_CTL_EL02, u64> = SysReg::new();
const TIMER_REG_CNTV_CVAL_EL02: SysReg<spec::CNTV_CVAL_EL02, u64> = SysReg::new();
const TIMER_REG_CNTVCT: SysReg<spec::CNTVCT_EL0, u64> = SysReg::new();

define_kcounter!(PLATFORM_TIMER_SET_COUNTER, "platform.timer.set", Sum);
define_kcounter!(PLATFORM_TIMER_CANCEL_COUNTER, "platform.timer.cancel", Sum);

// Counter-timer Kernel Control Register, EL1.
const CNTKCTL_EL1_ENABLE_PHYSICAL_COUNTER: u64 = 1 << 0;
const CNTKCTL_EL1_ENABLE_VIRTUAL_COUNTER: u64 = 1 << 1;

// Global saved config state
static TIMER_IRQ: AtomicU32 = AtomicU32::new(0);

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerIrqAssignment {
    Phys = 0,
    Virt = 1,
    Sphys = 2,
}

static TIMER_ASSIGNMENT: AtomicU8 = AtomicU8::new(TimerIrqAssignment::Phys as u8);

// event stream state
static EVENT_STREAM_SHIFT: AtomicU32 = AtomicU32::new(0);

// Definition of the function signature we use to fetch the value of the chosen
// reference counter.
type ReadArmCounterFunc = fn() -> u64;

unsafe extern "C" {
    fn arm64_allow_pct_in_el0();
}

fn read_cntp_ctl() -> u32 {
    if libarch::arm64::CURRENT_EL.read().el() == 2 {
        return TIMER_REG_CNTP_CTL_EL02.read() as u32;
    }
    TIMER_REG_CNTP_CTL.read() as u32
}

fn read_cntv_ctl() -> u32 {
    if libarch::arm64::CURRENT_EL.read().el() == 2 {
        return TIMER_REG_CNTV_CTL_EL02.read() as u32;
    }
    TIMER_REG_CNTV_CTL.read() as u32
}

fn read_cntps_ctl() -> u32 {
    TIMER_REG_CNTPS_CTL.read() as u32
}

fn write_cntp_ctl(val: u32) {
    ltracef_level!(3, "cntp_ctl: 0x{:x} {:x}\n", val, read_cntp_ctl());
    if libarch::arm64::CURRENT_EL.read().el() == 2 {
        TIMER_REG_CNTP_CTL_EL02.write(val as u64);
    } else {
        TIMER_REG_CNTP_CTL.write(val as u64);
    }
    libarch::arm64::isb();
}

fn write_cntv_ctl(val: u32) {
    ltracef_level!(3, "cntv_ctl: 0x{:x} {:x}\n", val, read_cntv_ctl());
    if libarch::arm64::CURRENT_EL.read().el() == 2 {
        TIMER_REG_CNTV_CTL_EL02.write(val as u64);
    } else {
        TIMER_REG_CNTV_CTL.write(val as u64);
    }
    libarch::arm64::isb();
}

fn write_cntps_ctl(val: u32) {
    ltracef_level!(3, "cntps_ctl: 0x{:x} {:x}\n", val, read_cntps_ctl());
    TIMER_REG_CNTPS_CTL.write(val as u64);
    libarch::arm64::isb();
}

fn write_cntp_cval(val: u64) {
    ltracef_level!(3, "cntp_cval: 0x{:016x}, {}\n", val, val);
    if libarch::arm64::CURRENT_EL.read().el() == 2 {
        TIMER_REG_CNTP_CVAL_EL02.write(val);
    } else {
        TIMER_REG_CNTP_CVAL.write(val);
    }
    libarch::arm64::isb();
}

fn write_cntv_cval(val: u64) {
    ltracef_level!(3, "cntv_cval: 0x{:016x}, {}\n", val, val);
    if libarch::arm64::CURRENT_EL.read().el() == 2 {
        TIMER_REG_CNTV_CVAL_EL02.write(val);
    } else {
        TIMER_REG_CNTV_CVAL.write(val);
    }
    libarch::arm64::isb();
}

fn write_cntps_cval(val: u64) {
    ltracef_level!(3, "cntps_cval: 0x{:016x}, {}\n", val, val);
    TIMER_REG_CNTPS_CVAL.write(val);
    libarch::arm64::isb();
}

fn read_zero() -> u64 {
    0
}

fn read_cntpct_a73() -> u64 {
    // Workaround for Cortex-A73 erratum 858921.
    // Fix will be applied to all cores, as two consecutive reads should be
    // faster than checking if core is A73 and branching before every read.
    let old_read = TIMER_REG_CNTPCT.read();
    let new_read = TIMER_REG_CNTPCT.read();

    if (((old_read ^ new_read) >> 32) & 1) != 0 { old_read } else { new_read }
}

fn read_cntvct_a73() -> u64 {
    // Workaround for Cortex-A73 erratum 858921.
    // Fix will be applied to all cores, as two consecutive reads should be
    // faster than checking if core is A73 and branching before every read.
    let old_read = TIMER_REG_CNTVCT.read();
    let new_read = TIMER_REG_CNTVCT.read();

    if (((old_read ^ new_read) >> 32) & 1) != 0 { old_read } else { new_read }
}

fn read_cntpct() -> u64 {
    TIMER_REG_CNTPCT.read()
}

fn read_cntvct() -> u64 {
    TIMER_REG_CNTVCT.read()
}

/// Early tick sample delivered by the assembly macro `sample_ticks`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EarlyTicks {
    pub cntpct_el0: u64,
    pub cntvct_el0: u64,
}

zr::static_assert_size_and_align!(EarlyTicks, 16, 8);
zr::static_assert_size_and_align!(DcfgArmGenericTimerDriver, 16, 4);

struct TimerRegProcs {
    write_ctl: fn(u32),
    write_cval: fn(u64),
    early_ticks: fn(EarlyTicks) -> u64,
}

static CNTP_PROCS: TimerRegProcs = TimerRegProcs {
    write_ctl: write_cntp_ctl,
    write_cval: write_cntp_cval,
    early_ticks: |sample| sample.cntpct_el0,
};

static CNTV_PROCS: TimerRegProcs = TimerRegProcs {
    write_ctl: write_cntv_ctl,
    write_cval: write_cntv_cval,
    early_ticks: |sample| sample.cntvct_el0,
};

static CNTPS_PROCS: TimerRegProcs = TimerRegProcs {
    write_ctl: write_cntps_ctl,
    write_cval: write_cntps_cval,
    early_ticks: |sample| sample.cntpct_el0,
};

struct AtomicReadArmCounter(AtomicPtr<()>);

impl AtomicReadArmCounter {
    const fn new(func: ReadArmCounterFunc) -> Self {
        Self(AtomicPtr::new(func as *mut ()))
    }

    #[inline]
    fn load(&self, order: Ordering) -> ReadArmCounterFunc {
        // SAFETY: `self.0` is only ever initialized or updated with valid `ReadArmCounterFunc`
        // (`fn() -> u64`) function pointers.
        unsafe { core::mem::transmute(self.0.load(order)) }
    }

    #[inline]
    fn store(&self, func: ReadArmCounterFunc, order: Ordering) {
        self.0.store(func as *mut (), order);
    }
}

// Notes about the `READ_ARM_COUNTER` function pointer:
//
// At startup time, we have not yet initialized our timer hardware, and
// therefore must return zero as required by platform_current_raw_ticks_synchronized
// (see the comments around that function for more details). Therefore, this
// function pointer initially points to a function that always returns zero.
//
// Once we figure out what CPUs we are running on, identify their HW, and check
// in with the rest of the system, we can pick which version of the
// READ_ARM_COUNTER function we need to use. There exists a bug in certain ARM
// Cortex-A73 CPUs which can lead to a bad read of either the VCT or PCT
// counters.  It is documented as Errata 858921
// ( https://documentation-service.arm.com/static/5fa29fa7b209f547eebd3613 )
// Thus, on systems with A73 cores, we must utilize a function that accounts for
// this errata. On systems without A73 cores, we select a faster implementation.
//
// In order to make this switch, however, and not need any locks, we need to
// make sure that the function pointer is declared as an atomic.  Otherwise, we
// could be writing to the pointer when someone else is reading it (in order to
// read the clock) which would be a formal data race.  Note that we don't need
// any ordering of the loads and stores of the function pointer beyond
// `Ordering::Relaxed`.  It is not important that we establish a specific
// order of the pointer's value relative to other memory accesses in the system.
// We just need to make sure that _if_ we decide to switch to the faster version
// of the counter read, that all of the CPUs _eventually_ see the pointer
// update (which they will do because of unavoidable synchronizing events like
// taking exceptions).
//
static READ_ARM_COUNTER: AtomicReadArmCounter = AtomicReadArmCounter::new(read_zero);

#[inline]
fn reg_procs() -> &'static TimerRegProcs {
    match TIMER_ASSIGNMENT.load(Ordering::Relaxed) {
        x if x == TimerIrqAssignment::Virt as u8 => &CNTV_PROCS,
        x if x == TimerIrqAssignment::Sphys as u8 => &CNTPS_PROCS,
        _ => &CNTP_PROCS,
    }
}

#[inline]
fn write_ctl(val: u32) {
    (reg_procs().write_ctl)(val);
}

#[inline]
fn write_cval(val: u64) {
    (reg_procs().write_cval)(val);
}

extern "C" fn platform_tick(_cookie: *mut c_void) {
    write_ctl(0);
    // SAFETY: `platform_tick` is registered as the timer interrupt handler and only invoked
    // from interrupt context with interrupts disabled.
    unsafe {
        timer_tick();
    }
}

// GetTicksSyncFlag bit constants matching C++ `GetTicksSyncFlag` in `<platform/timer.h>`.
const GET_TICKS_SYNC_FLAG_NONE: u8 = 0b0000;
const GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS: u8 = 0b0001;
const GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES: u8 = 0b0010;
const GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS: u8 = 0b0100;
const GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES: u8 = 0b1000;

/// Reads a platform-specific ticks counter.
///
/// This function is required to return zero if it is invoked before the timer hardware is
/// initialized. Not doing so could violate the monotonicity of both the monotonic and boot
/// timelines.
///
/// This raw counter value is almost certainly not what you want; callers generally want to use the
/// monotonic ticks value provided by timer_current_mono_ticks. This ticks value is the raw counter
/// adjusted by a constant used to make the ticks timeline start ticking from 0 when the system
/// boots.
#[inline(always)]
pub fn platform_current_raw_ticks_synchronized<const FLAGS: u8>() -> zx_ticks_t {
    // Make certain that any reads of the raw system timer are guaranteed to take
    // place in a region defined by the template |FLAGS|.  Note that the
    // methodology used here was defined in
    //
    // 'Arm Architecture Reference Manual for A-profile architecture'
    // revision 'ARM DDI 0487K.a'
    //
    // In particular, please refer to examples D12-3 and D12-4.  Note that we
    // chose to use the "DMB and Branch Dependency" approach (instead of a DSB) to
    // ensure that timer reads take place after all previous memory accesses, and
    // we use a load dependent on the value of the timer load to ensure that the
    // timer load takes place before subsequent memory accesses.  Additionally, we
    // do not make any attempt to use self-synchronizing timer register accesses.
    // Refer to the cited examples for the potentially valid sequences.

    // Do we need to guarantee that this clock read occurs after previous loads,
    // stores, or both?
    //
    // If so, we need to implement the solution described in refer to Example
    // D12-4 in the ARM ARM, referenced above.
    //
    // We need our timer read to happen after all previous memory accesses.
    // This is implemented as a DMB followed by read from any valid memory
    // location with a "branch" which depends on that read.  Note that
    // "branch" is in air quotes because its target is the next instruction,
    // so whether or not the branch gets taken, the result is the same (to
    // execute the next instruction).  Finally, the sequence ends with an ISB.
    //
    // Also note that we need the DMB approach here, even when we only care about
    // previous loads, because we are attempting to ensure that our counter read
    // takes place after _all_ previous loads.  If we were interested in only
    // ensuring that the counter observation took place after all previous loads
    // of a _specific_ variable (call it `X`), there would be another option to
    // consider.  It is shown in example D12-3 of the ARM ARM, and involves
    // creating a branch which depends on the previously loaded value of `X`,
    // followed by an ISB.  Since the spec of this function is to ensure that the
    // counter observation follow _all_ previous loads, it is not a technique we
    // can use here.
    //
    // TODO(johngro): Consider adding an API for a raw counter read which would
    // allow for this.  There are two key cases (reading a synthetic kernel clock,
    // and reading the monotonic or boot timeline) where we don't actually need to
    // ensure that our counter read takes place after all previous loads, just the
    // previous load of a specific variable (the generation counter in the case of
    // a synthetic clock, and the offset in the case of a mono/boot timeline
    // read).  That said, designing such an API presents some challenges as not
    // all of the specific variable reads have the same requirements.  For
    // example, in the case of synthetic kernel clocks, the extra variable needs
    // to be read with acquire semantics.  In the case of mono/boot reads, relaxed
    // semantics are all that are needed.
    //
    let must_read_after_all_previous_accesses: bool = (FLAGS
        & (GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS | GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES))
        != GET_TICKS_SYNC_FLAG_NONE;
    if must_read_after_all_previous_accesses {
        // We need our timer read to happen after all previous memory accesses.
        // This is implemented as a DMB followed by read from any valid memory
        // location with a "branch" which depends on that read.  Note that
        // "branch" is in air quotes because its target is the next instruction,
        // so whether or not the branch gets taken, the result is the same (to
        // execute the next instruction).  Finally, the sequence ends with an ISB.
        //
        // Refer to Example D12-4 in the ARM ARM referenced above.
        //
        // `temp` is overwritten as a scratch register. `nomem` is intentionally
        // omitted so the block acts as a compiler memory barrier to prevent
        // reordering across the sequence, and `preserves_flags` is set because
        // `dmb`, `ldr`, `cbz`, and `isb` do not modify NZCV.
        //
        // SAFETY: `[sp]` is always a valid readable memory location on the current stack,
        // and the assembly sequence only writes to the scratch `temp` register while acting
        // as a memory and instruction barrier. None of the instructions touch NZCV.
        unsafe {
            core::arch::asm!(
                "dmb sy",
                "ldr {temp}, [sp]",
                "cbz {temp}, 1f",
                "1: isb",
                temp = out(reg) _,
                options(nostack, preserves_flags),
            );
        }
    }

    // Now actually read from the configured system timer.
    let ret: zx_ticks_t = READ_ARM_COUNTER.load(Ordering::Relaxed)() as zx_ticks_t;

    // Do we need to guarantee that this clock read occurs before subsequent loads,
    // stores, or both?  If so, the recipe is the same in all cases.  We introduce
    // a load operation which has data dependency on ret, forcing the timer read
    // to finish before the dependent load can occur.
    //
    // Refer to Example D12-4 in the ARM ARM referenced above.
    //
    let must_read_before_any_subsequent_access: bool = (FLAGS
        & (GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS
            | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES))
        != GET_TICKS_SYNC_FLAG_NONE;
    if must_read_before_any_subsequent_access {
        // `temp` is overwritten as a scratch register and `ret` is consumed as
        // an input. Both `preserves_flags` and `nomem` are intentionally
        // omitted so the compiler treats flags as potentially clobbered and
        // treats the block as a compiler memory barrier preventing reordering.
        //
        // SAFETY: `eor` zeroes the scratch `temp` register with a data dependency on `ret`, so
        // `[sp, temp]` reads from `[sp]`, which is always a valid readable stack address.
        unsafe {
            core::arch::asm!(
                "eor {temp}, {ret}, {ret}",
                "ldr {temp}, [sp, {temp}]",
                temp = out(reg) _,
                ret = in(reg) ret,
                options(nostack),
            );
        }
    }

    ret
}

// Explicit instantiation of all of the forms of synchronized tick access.
macro_rules! define_raw_ticks_exports {
    ($( $name:ident => $flags:expr ),* $(,)?) => {
        $(
            /// FFI entry point for synchronized raw tick reads.
            #[unsafe(no_mangle)]
            pub extern "C" fn $name() -> zx_ticks_t {
                platform_current_raw_ticks_synchronized::<$flags>()
            }
        )*
    };
}

define_raw_ticks_exports! {
    rust_platform_current_raw_ticks_synchronized_0 => 0,
    rust_platform_current_raw_ticks_synchronized_1 => 1,
    rust_platform_current_raw_ticks_synchronized_2 => 2,
    rust_platform_current_raw_ticks_synchronized_3 => 3,
    rust_platform_current_raw_ticks_synchronized_4 => 4,
    rust_platform_current_raw_ticks_synchronized_5 => 5,
    rust_platform_current_raw_ticks_synchronized_6 => 6,
    rust_platform_current_raw_ticks_synchronized_7 => 7,
    rust_platform_current_raw_ticks_synchronized_8 => 8,
    rust_platform_current_raw_ticks_synchronized_9 => 9,
    rust_platform_current_raw_ticks_synchronized_10 => 10,
    rust_platform_current_raw_ticks_synchronized_11 => 11,
    rust_platform_current_raw_ticks_synchronized_12 => 12,
    rust_platform_current_raw_ticks_synchronized_13 => 13,
    rust_platform_current_raw_ticks_synchronized_14 => 14,
    rust_platform_current_raw_ticks_synchronized_15 => 15,
}

/// Convert a sample taken early on to a proper zx_ticks_t, if possible.
/// This returns 0 if early samples are not convertible.
pub fn platform_convert_early_ticks(sample: EarlyTicks) -> zx_ticks_t {
    // Early tick timestamps are always raw ticks.  We need to convert back to
    // ticks by subtracting the raw_ticks to ticks offset.
    let early_ticks = (reg_procs().early_ticks)(sample);
    early_ticks.wrapping_add(timer_get_mono_ticks_offset() as u64) as zx_ticks_t
}

/// API to set/clear a hardware timer that is responsible for calling timer_tick() when it fires.
pub fn platform_set_oneshot_timer(mut deadline: zx_ticks_t) -> Result<(), Status> {
    debug_assert!(ints_disabled());

    if deadline < 0 {
        deadline = 0;
    }

    // Even if the deadline has already passed, the ARMv8-A timer will fire the
    // interrupt.
    write_cval(deadline as u64);
    write_ctl(1);
    PLATFORM_TIMER_SET_COUNTER.add(1);

    Ok(())
}

/// Stops the hardware timer on the calling CPU.
pub fn platform_stop_timer() {
    write_ctl(0);
    PLATFORM_TIMER_CANCEL_COUNTER.add(1);
}

/// Shutdown the calling CPU's platform timer.
///
/// Should be called after |platform_stop_timer|, but before taking the CPU offline.
///
/// TODO(maniscalco): Provide a "resume" function so we can suspend/resume.
pub fn platform_shutdown_timer() {
    debug_assert!(ints_disabled());
    // SAFETY: The interrupt controller ops are registered before any CPU reaches
    // `platform_shutdown_timer`, and `TIMER_IRQ` holds the vector chosen by
    // `arm_generic_timer_init`.
    let _ = unsafe { mask_interrupt(InterruptVector(TIMER_IRQ.load(Ordering::Relaxed))) };
}

/// Suspend the platform timer for the calling CPU.
///
/// It is an error to call this function with interrupts enabled.
pub fn platform_suspend_timer_curr_cpu() -> Result<(), Status> {
    debug_assert!(ints_disabled());

    // Save the cntkctl_el1 register, which includes the event stream state and
    // whether EL0 can read PCT.
    PerCpu::get_current()
        .resume_state
        .cntkctl_el1
        .store(TIMER_REG_CNTKCTL.read(), Ordering::Relaxed);

    write_ctl(0);

    // SAFETY: The interrupt controller ops are registered before CPU suspend, and `TIMER_IRQ`
    // holds the vector chosen by `arm_generic_timer_init`.
    unsafe { mask_interrupt(InterruptVector(TIMER_IRQ.load(Ordering::Relaxed))) }
}

/// Resume the platform timer for the calling CPU.
///
/// It is an error to call this function with interrupts enabled.
pub fn platform_resume_timer_curr_cpu() -> Result<(), Status> {
    debug_assert!(ints_disabled());

    TIMER_REG_CNTKCTL.write(PerCpu::get_current().resume_state.cntkctl_el1.load(Ordering::Relaxed));

    // SAFETY: The interrupt controller ops are registered before CPU resume, and `TIMER_IRQ`
    // holds the vector chosen by `arm_generic_timer_init`.
    let _ = unsafe { unmask_interrupt(InterruptVector(TIMER_IRQ.load(Ordering::Relaxed))) };

    // Kick the timer to get things going again.
    //
    // TODO(https://fxbug.dev/417558115): Remove/merge this with the logic in
    // IdlePowerThread::UpdateMonotonicClock such that we don't kick the platform
    // timer twice.
    platform_set_oneshot_timer(0)
}

/// A bool indicating whether or not user mode has direct access to the registers
/// which allow directly observing the tick counter or not.
pub fn platform_usermode_can_access_tick_registers() -> bool {
    // We always use the ARM generic timer for the tick counter, and these
    // registers are accessible from usermode
    true
}

fn arm_generic_timer_compute_conversion_factors<const ALLOW_DEBUG_PRINT: bool>(
    cntfrq: u32,
) -> Ratio {
    let cntpct_to_nsec = Ratio::new(zx_sec(1) as u32, cntfrq);
    if ALLOW_DEBUG_PRINT {
        dprintf!(
            SPEW,
            "arm generic timer cntpct_per_nsec: {}/{}\n",
            cntpct_to_nsec.numerator(),
            cntpct_to_nsec.denominator()
        );
    }
    cntpct_to_nsec
}

// Run once on the boot cpu to decide if we want to start an event stream on each
// cpu and at what rate.
fn event_stream_init(cntfrq: u32) {
    if !BootOptions::get().arm64_event_stream_enabled {
        return;
    }

    // Compute the closest power of two from the timer frequency to get to the target.
    //
    // The mechanism to select the rate of the event counter is to select which bit in the virtual
    // counter it should watch for a transition from 0->1 or 1->0 on. This has the effect of
    // of selecting a power of two to divide the virtual counter by plus one.
    //
    // There's no real out of range value here. Everything gets clamped to a shift value of [0, 15].
    let mut shift: u32 = 0;
    while shift <= 14 {
        // Find a matching shift to the target frequency within range. If the target frequency is
        // too large even for shift 0 then it'll just pick shift 0 because of the <=.
        if (cntfrq >> (shift + 1)).checked_ilog2().unwrap_or(0)
            <= BootOptions::get().arm64_event_stream_freq_hz.checked_ilog2().unwrap_or(0)
        {
            break;
        }
        shift += 1;
    }
    // If we ran off the end of the for loop 15 is the max shift and is okay
    debug_assert!(shift <= 15);

    // Save the computed state
    let freq = cntfrq >> (shift + 1);
    EVENT_STREAM_SHIFT.store(shift, Ordering::Relaxed);

    dprintf!(
        INFO,
        "arm generic timer will enable event stream on all cpus: shift {}, {} Hz\n",
        shift,
        freq
    );
}

fn event_stream_enable_percpu() {
    if !BootOptions::get().arm64_event_stream_enabled {
        return;
    }

    let shift = EVENT_STREAM_SHIFT.load(Ordering::Relaxed);
    debug_assert!(shift <= 15);

    // Enable the event stream
    let mut cntkctl = TIMER_REG_CNTKCTL.read();
    // Set the trigger bit (field 7:4)
    cntkctl &= !(0xfu64 << 4);
    cntkctl |= (shift as u64) << 4; // EVNTI
    // Clear the transition bit to 0
    cntkctl &= !(1 << 3); // ENVTDIR
    // Enable the stream
    cntkctl |= 1 << 2; // EVNTEN
    TIMER_REG_CNTKCTL.write(cntkctl);

    dprintf!(INFO, "arm generic timer cpu-{}: event stream enabled\n", curr_cpu_num());
}

fn arm_generic_timer_init_secondary_cpu(_level: init::LkInitLevel) {
    // try to enable the event stream if requested
    event_stream_enable_percpu();

    let irq = TIMER_IRQ.load(Ordering::Relaxed);
    ltracef!("unmask irq {} on cpu {}\n", irq, curr_cpu_num());
    // SAFETY: The interrupt controller ops are registered before secondary CPUs run this hook,
    // and `irq` is the vector chosen by `arm_generic_timer_init`.
    let _ = unsafe { unmask_interrupt(InterruptVector(irq)) };
}

// secondary cpu initialize the timer just before the kernel starts with interrupts enabled
init::lk_init_hook_flags!(
    arm_generic_timer_init_secondary_cpu,
    arm_generic_timer_init_secondary_cpu,
    init::LkInitLevel(init::LK_INIT_LEVEL_THREADING.0 - 1),
    init::LkInitFlags::SecondaryCpus
);

/// Initializes the driver from the ZBI config, selecting the timer and setting up the ticks
/// timeline and event stream.
///
/// # Safety
///
/// Must be called once, on the boot CPU with interrupts disabled, before secondary CPUs are
/// started and before anything converts ticks to time.
pub unsafe fn arm_generic_timer_init(config: &DcfgArmGenericTimerDriver) {
    let irq_phys = config.irq_phys;
    let mut irq_virt = config.irq_virt;
    let irq_sphys = config.irq_sphys;

    // If boot-options have been configured to force us to use the physical
    // counter as our reference, clear out any virtual timer hardware and let
    // nature take its course.  If we don't have an interrupt configured for using
    // the physical timer hardware (either PHYS or SPHYS), we are going to end up
    // panicking.
    if BootOptions::get().arm64_force_pct
        || (libarch::arm64::CURRENT_EL.read().el() == 2 && irq_phys != 0)
    {
        dprintf!(
            INFO,
            "arm generic timer forcing use of PCT.  \
             IRQs provided were (virt {}, phys {}, sphys {})\n",
            irq_virt,
            irq_phys,
            irq_sphys
        );
        irq_virt = 0;
    }

    // Always prefer to use the virtual timer if we have the option to do so.
    // Additionally, always start by using the versions of the timer read which
    // have the A73 errata workaround.
    //
    // Currently, we have not had a chance to boot all of our CPUs and determine
    // if we have any A73's in the mix.  Until we know, it is safe to use the A73
    // versions of the reads, just a small bit slower.  Later on, if we know it is
    // safe to do so, we can switch to using the workaround-free version.
    let timer_str: &str;
    let timer_irq: u32;
    if irq_virt != 0 {
        timer_str = "virt";
        timer_irq = irq_virt;
        TIMER_IRQ.store(timer_irq, Ordering::Relaxed);
        TIMER_ASSIGNMENT.store(TimerIrqAssignment::Virt as u8, Ordering::Relaxed);
        READ_ARM_COUNTER.store(read_cntvct_a73, Ordering::Relaxed);
    } else if irq_phys != 0 {
        timer_str = "phys";
        timer_irq = irq_phys;
        TIMER_IRQ.store(timer_irq, Ordering::Relaxed);
        TIMER_ASSIGNMENT.store(TimerIrqAssignment::Phys as u8, Ordering::Relaxed);
        READ_ARM_COUNTER.store(read_cntpct_a73, Ordering::Relaxed);
        // SAFETY: FFI call with no preconditions; declared in <arch/arm64.h>.
        unsafe {
            arm64_allow_pct_in_el0();
        }
    } else if irq_sphys != 0 {
        timer_str = "sphys";
        timer_irq = irq_sphys;
        TIMER_IRQ.store(timer_irq, Ordering::Relaxed);
        TIMER_ASSIGNMENT.store(TimerIrqAssignment::Sphys as u8, Ordering::Relaxed);
        READ_ARM_COUNTER.store(read_cntpct_a73, Ordering::Relaxed);
        // SAFETY: FFI call with no preconditions; declared in <arch/arm64.h>.
        unsafe {
            arm64_allow_pct_in_el0();
        }
    } else {
        panic!("no irqs set in arm_generic_timer_pdev_init\n");
    }
    // We cannot actually reset the value on the ticks timer, so instead we use
    // the time of clock selection (now) to define the zero point on our ticks
    // timeline moving forward.
    // SAFETY: The caller upholds the one-time boot-CPU contract of this function.
    unsafe {
        timer_set_initial_ticks(READ_ARM_COUNTER.load(Ordering::Relaxed)());
    }
    libarch::arm64::thread_memory_barrier();

    let timer_cntfrq = if config.freq_override == 0 {
        // Read the firmware supplied cntfrq register. Note: it may not be correct
        // in buggy firmware situations, so always provide a mechanism to override it.
        let cntfrq = TIMER_REG_CNTFRQ.read() as u32;
        ltracef!("cntfrq: {:#010x}, {}\n", cntfrq, cntfrq);
        cntfrq
    } else {
        config.freq_override
    };

    dprintf!(
        INFO,
        "arm generic timer using {} timer, irq {}, freq {} Hz\n",
        timer_str,
        timer_irq,
        timer_cntfrq
    );

    // No way to reasonably continue. Just hard stop.
    assert!(timer_cntfrq != 0);

    // SAFETY: The caller upholds the one-time boot-CPU contract of this function.
    unsafe {
        timer_set_ticks_to_time_ratio(arm_generic_timer_compute_conversion_factors::<true>(
            timer_cntfrq,
        ));
    }

    // At this point in time, we expect that the `cntkctl_el1` timer register has
    // the bit which permits EL0 access to the VCT to be set, and perhaps also the
    // bit which allows access to the PCT if that happens to be the time reference
    // we have decided to use.
    //
    // *None* of the other timer HW access bits should be set.  EL0 only gets to
    // look at the counter, and nothing more.
    //
    // ASSERT this now.
    let expected = CNTKCTL_EL1_ENABLE_VIRTUAL_COUNTER
        | if arm_use_phys_timer_in_vdso() { CNTKCTL_EL1_ENABLE_PHYSICAL_COUNTER } else { 0 };
    let current = TIMER_REG_CNTKCTL.read();
    assert!(
        current == expected,
        "CNTKCTL_EL1 register does not match reference counter selection ({:016x} != {:016x})",
        current,
        expected
    );

    // Determine and compute values for the event stream if requested
    event_stream_init(timer_cntfrq);

    // try to enable the event stream if requested
    event_stream_enable_percpu();
}

/// Another shot at initialization post interrupt controller and VM.
pub fn arm_generic_timer_init_post_vm(_config: &DcfgArmGenericTimerDriver) {
    let irq = TIMER_IRQ.load(Ordering::Relaxed);
    // Set up the hardware timer irq handler for this vector. Use the permanent irq handler
    // registration scheme since it is enabled on all cpus and does not need any locking
    // for reentrancy and deregistration purposes.
    // SAFETY: `platform_tick` is safe to invoke from interrupt context and the interrupt
    // controller has been initialized.
    let status = unsafe {
        register_permanent_int_handler(
            InterruptVector(irq),
            InterruptHandler { cookie: core::ptr::null_mut(), r#fn: Some(platform_tick) },
        )
    };
    debug_assert!(status.is_ok());

    // enable the IRQ on the boot cpu
    ltracef!("unmask irq {} on cpu {}\n", irq, curr_cpu_num());
    // SAFETY: The interrupt controller ops are registered (see above), and `irq` is the vector
    // chosen by `arm_generic_timer_init`.
    let _ = unsafe { unmask_interrupt(InterruptVector(irq)) };
}

/// Indicates that we should be using the physical view of the system timer
/// instead of the virtual view.
pub fn arm_use_phys_timer_in_vdso() -> bool {
    TIMER_ASSIGNMENT.load(Ordering::Relaxed) != TimerIrqAssignment::Virt as u8
}

fn late_update_keep_or_disable_a73_timer_workaround(_level: init::LkInitLevel) {
    // By the time we make it to LK_INIT_LEVEL_SMP_READY we should have started
    // and sync'ed up with all of our secondary CPUs. If not, something has gone
    // terribly wrong, and we should continue to use the A73 workaround out of an
    // abundance of caution.
    //
    let status = wait_for_all_cpus_ready(Deadline::no_slack(InstantUnknown(0)));
    if status.is_err() {
        dprintf!(
            ALWAYS,
            "At least one CPU has failed to check in by INIT_LEVEL_SMP_READY \
             in the init sequence.  Keeping A73 counter workarounds in place.\n"
        );
    } else {
        // SAFETY: All CPUs have checked in and published their MIDR / uarch quirks.
        let needs_mitigation = unsafe { arch_quirks_needs_arm_erratum_858921_mitigation() };
        if !needs_mitigation {
            // If all of our CPUs have checked in, and we have not discovered any A73
            // CPUs, we can switch to using the simple register read instead of the
            // double-read required by the A73 workaround.
            let thunk: ReadArmCounterFunc =
                if arm_use_phys_timer_in_vdso() { read_cntpct } else { read_cntvct };
            READ_ARM_COUNTER.store(thunk, Ordering::Relaxed);
        } else {
            dprintf!(INFO, "A73 cores detected.  Keeping arm generic timer A73 workaround\n");
        }
    }
}

init::lk_init_hook!(
    late_update_keep_or_disable_a73_timer_workaround,
    late_update_keep_or_disable_a73_timer_workaround,
    init::LK_INIT_LEVEL_SMP_READY
);

// C FFI entrypoints for callers in C++.

/// FFI entry point for [`platform_convert_early_ticks`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_convert_early_ticks(sample: EarlyTicks) -> zx_ticks_t {
    platform_convert_early_ticks(sample)
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

/// FFI entry point for [`platform_usermode_can_access_tick_registers`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_usermode_can_access_tick_registers() -> bool {
    platform_usermode_can_access_tick_registers()
}

/// FFI entry point for [`arm_generic_timer_init`].
///
/// # Safety
///
/// See [`arm_generic_timer_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arm_generic_timer_init(config: &DcfgArmGenericTimerDriver) {
    // SAFETY: The C++ caller upholds the one-time boot-CPU contract.
    unsafe { arm_generic_timer_init(config) };
}

/// FFI entry point for [`arm_generic_timer_init_post_vm`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_arm_generic_timer_init_post_vm(config: &DcfgArmGenericTimerDriver) {
    arm_generic_timer_init_post_vm(config);
}

/// FFI entry point for [`arm_use_phys_timer_in_vdso`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_arm_use_phys_timer_in_vdso() -> bool {
    arm_use_phys_timer_in_vdso()
}

/// Tests for ARM tick count and current time
#[cfg(ktest)]
#[unittest::suite(name = "arm_clock")]
mod arm_clock_tests {
    use crate::arch_rs::InterruptDisableGuard;
    use crate::kernel::mp::{get_online_mask, is_cpu_online};
    use crate::kernel::percpu::PerCpu;
    use crate::kernel::scheduler::{peek_active_mask, peek_is_active};
    use crate::kernel::thread::{DEFAULT_PRIORITY, ThreadPtr, create_with_priority};
    use crate::kernel::types::{SMP_MAX_CPUS, cpu_num_t, cpu_num_to_mask};
    use crate::platform_rs::timer::{InstantMono, ticks_per_second, timer_get_ticks_to_time_ratio};
    use affine::Round;
    use core::sync::atomic::AtomicU32;
    use kprint::kprintln;
    use unittest::{assert_le, assert_true, unwrap_ok};
    use zx_types::zx_time_t;

    const MIN_TEST_FREQ: u32 = 1;
    const MAX_TEST_FREQ: u32 = u32::MAX;
    const CUR_TEST_FREQ: u32 = 0;

    #[inline]
    fn abs_int64(a: i64) -> u64 {
        if a > 0 { a as u64 } else { a.wrapping_neg() as u64 }
    }

    fn test_time_conversion_check_result(a: u64, b: u64, limit: u64) -> bool {
        (unittest::subtest!(|a: u64, b: u64, limit: u64| {
            if a != b {
                let diff = abs_int64(a.wrapping_sub(b) as i64);
                assert_le!(diff, limit);
            }
        }))(a, b, limit)
    }

    fn test_time_to_ticks(cntfrq: u32) -> bool {
        (unittest::subtest!(|cntfrq_arg: u32| {
            let mut cntfrq = cntfrq_arg;
            let time_to_ticks: Ratio;
            if cntfrq == CUR_TEST_FREQ {
                let tps = ticks_per_second() as u64;
                assert_le!(tps, u32::MAX as u64);
                cntfrq = tps as u32;
                time_to_ticks = timer_get_ticks_to_time_ratio().inverse();
            } else {
                time_to_ticks =
                    arm_generic_timer_compute_conversion_factors::<false>(cntfrq).inverse();
            }

            const VECTORS: [u64; 6] = [
                0,
                1,
                60 * 60 * 24,
                60 * 60 * 24 * 365,
                60 * 60 * 24 * (365 * 10 + 2),
                60 * 60 * 24 * (365 * 100 + 2),
            ];

            for vec in VECTORS {
                let cntpct = time_to_ticks.scale::<{ Round::DOWN }>(vec as i64) as u64;
                const NANOS_PER_SEC: u64 = zx_sec(1) as u64;
                let expected_cntpct = (cntfrq as u64 * vec + (NANOS_PER_SEC / 2)) / NANOS_PER_SEC;

                if !test_time_conversion_check_result(cntpct, expected_cntpct, 1) {
                    kprintln!(
                        "FAIL: zx_time_to_ticks({}): got {}, expect {}",
                        vec,
                        cntpct,
                        expected_cntpct
                    );
                    assert_true!(false);
                }
            }
        }))(cntfrq)
    }

    fn test_ticks_to_time(cntfrq: u32) -> bool {
        (unittest::subtest!(|cntfrq_arg: u32| {
            let mut cntfrq = cntfrq_arg;
            let ticks_to_time: Ratio;
            if cntfrq == CUR_TEST_FREQ {
                let tps = ticks_per_second() as u64;
                assert_le!(tps, u32::MAX as u64);
                cntfrq = tps as u32;
                ticks_to_time = timer_get_ticks_to_time_ratio();
            } else {
                ticks_to_time = arm_generic_timer_compute_conversion_factors::<false>(cntfrq);
            }

            const VECTORS: [u64; 5] = [
                1,
                60 * 60 * 24,
                60 * 60 * 24 * 365,
                60 * 60 * 24 * (365 * 10 + 2),
                60 * 60 * 24 * (365 * 50 + 2),
            ];

            for vec in VECTORS {
                let expected_zx_time: zx_time_t = zx_sec(vec as i64);
                let cntpct = cntfrq as u64 * vec;
                let zx_time: zx_time_t = ticks_to_time.scale::<{ Round::DOWN }>(cntpct as i64);

                let limit = (1000u64 * 1000).div_ceil(cntfrq as u64);
                if !test_time_conversion_check_result(
                    zx_time as u64,
                    expected_zx_time as u64,
                    limit,
                ) {
                    kprintln!(
                        "ticks_to_zx_time(0x{:x}): got 0x{:x}, expect 0x{:x}",
                        cntpct,
                        zx_time as u64,
                        expected_zx_time as u64
                    );
                    assert_true!(false);
                }
            }
        }))(cntfrq)
    }

    /// Time --> Ticks (min freq)
    #[test]
    fn test_time_to_ticks_min_freq() {
        assert_true!(test_time_to_ticks(MIN_TEST_FREQ));
    }

    /// Time --> Ticks (max freq)
    #[test]
    fn test_time_to_ticks_max_freq() {
        assert_true!(test_time_to_ticks(MAX_TEST_FREQ));
    }

    /// Time --> Ticks (cur freq)
    #[test]
    fn test_time_to_ticks_cur_freq() {
        assert_true!(test_time_to_ticks(CUR_TEST_FREQ));
    }

    /// Ticks --> Time (min freq)
    #[test]
    fn test_ticks_to_time_min_freq() {
        assert_true!(test_ticks_to_time(MIN_TEST_FREQ));
    }

    /// Ticks --> Time (max freq)
    #[test]
    fn test_ticks_to_time_max_freq() {
        assert_true!(test_ticks_to_time(MAX_TEST_FREQ));
    }

    /// Ticks --> Time (cur freq)
    #[test]
    fn test_ticks_to_time_cur_freq() {
        assert_true!(test_ticks_to_time(CUR_TEST_FREQ));
    }

    // Verify that the event stream will break CPUs out of WFE.
    //
    // Start one thread for each CPU that's online and active.  Each thread will then disable
    // interrupts and issue a series of WFEs.  If the event stream is working as expected, each
    // thread will eventually complete its series of WFEs and terminate.  If the event stream is not
    // working as expected, one or more threads will hang.
    /// Event Stream
    #[test]
    fn test_event_stream() {
        if !BootOptions::get().arm64_event_stream_enabled {
            kprintln!("event stream disabled, skipping test");
            return true;
        }

        struct Args {
            waiting: AtomicU32,
        }

        extern "C" fn func(a: *mut c_void) -> i32 {
            // SAFETY: `a` points to `Args` on the caller's stack, valid until all threads join.
            let args = unsafe { &*(a as *const Args) };
            {
                let _guard = InterruptDisableGuard::new();

                // Signal that we are ready.
                args.waiting.fetch_sub(1, Ordering::SeqCst);
                // Wait until everyone else is ready.
                while args.waiting.load(Ordering::SeqCst) > 0 {
                    libarch::r#yield();
                }

                // If the event stream is working, it (or something else) will break us out on each
                // iteration.
                for _ in 0..1000 {
                    // The SEVL sets the event flag for this CPU.  The first WFE consumes the now
                    // set event flag.  By setting then consuming, we can be sure the second WFE
                    // will actually wait for an event.
                    // SAFETY: `sevl` and `wfe` are architectural hint instructions with no memory,
                    // register, or stack side effects.
                    unsafe {
                        core::arch::asm!("sevl;wfe;wfe");
                    }
                }
            }
            kprintln!("cpu-{} done", curr_cpu_num());
            0
        }

        let args = Args { waiting: AtomicU32::new(0) };
        let mut threads: [Option<ThreadPtr>; SMP_MAX_CPUS] = [None; SMP_MAX_CPUS];

        // How many online+active CPUs do we have?
        let num_cpus = (get_online_mask() & peek_active_mask()).count_ones();
        args.waiting.store(num_cpus, Ordering::SeqCst);

        // Create a thread bound to each online+active CPU, but don't start them just yet.
        let mut last: cpu_num_t = 0;
        let count = PerCpu::processor_count() as cpu_num_t;
        for i in 0..count {
            if is_cpu_online(i) && peek_is_active(i) {
                // SAFETY: `func` and `args` remain valid until `join` completes.
                let t_ptr = unwrap_ok!(unsafe {
                    create_with_priority(
                        c"test_event_stream".as_ptr(),
                        func,
                        core::ptr::from_ref(&args).cast_mut().cast(),
                        DEFAULT_PRIORITY,
                    )
                });
                // SAFETY: `t_ptr` was just created and has not been resumed yet.
                unsafe { t_ptr.set_cpu_affinity(cpu_num_to_mask(i)) };
                threads[i as usize] = Some(t_ptr);
                last = i;
            }
        }

        // Because these threads have hard affinity and will disable interrupts we need to take care
        // in how we start them.  If we start one that's bound to our current CPU, we may get
        // preempted deadlock.  To avoid this, bind the current thread to the *last* online+active
        // CPU.
        // SAFETY: `ThreadPtr::current()` returns the valid current thread.
        let orig_mask = unsafe {
            let curr = ThreadPtr::current();
            let orig = curr.get_cpu_affinity();
            curr.set_cpu_affinity(cpu_num_to_mask(last));
            orig
        };
        let _restore_mask = zr::defer(move || {
            // SAFETY: `ThreadPtr::current()` returns the valid current thread.
            unsafe { ThreadPtr::current().set_cpu_affinity(orig_mask) };
        });

        // Now that we're running on the last online+active CPU we can simply start them in order.
        for thread in threads.iter().take(PerCpu::processor_count()) {
            if let Some(t_ptr) = *thread {
                // SAFETY: `t_ptr` is a valid unresumed thread.
                unsafe { t_ptr.resume() };
            }
        }

        // Finally, wait for them to complete.
        for thread in threads.iter().take(PerCpu::processor_count()) {
            if let Some(t_ptr) = *thread {
                // SAFETY: `t_ptr` is a valid thread that has not been joined or detached.
                let _ = unsafe { t_ptr.join(InstantMono::INFINITE) };
            }
        }
    }
}
