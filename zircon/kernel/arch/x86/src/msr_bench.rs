// Copyright 2022 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! MSR Benchmark:
//!
//! This benchmark attempts to measure the cost of reading and writing MSR
//! registers (specifically, the TSC Deadline register used to implement timers
//! on x64), and the effect that doing so might have on other CPUs' performance.
//!
//! These measurements are meant to serve two purposes:
//!
//! 1) To compare the relative performance of MSR reads/writes across
//!    1a) Native HW environments (eg; running on a 'host');
//!    1b) Guest VM environments running directly inside of a host.
//!    1b) Nested guest VM environments (eg; a guest inside of a guest inside of a
//!    host)
//! 2) To see if reading/writing MSR registers on one CPU has an affect on other
//!    CPUs.
//!
//! #1 helps us to understand the cost of MSR access in a VM environment, while
//! #2 helps us to understand if a VM environments implementation of MSR access
//! affects other CPUs.  We expected that it would not, but VMs can be tricky
//! (esp. nested VMs).
//!
//! The structure of the benchmark is as follows:
//!
//! We will take measurements across a number of stages across all currently
//! online CPUs.  One of the online CPUs is considered to be the "primary" CPU,
//! while the others are considered to be "secondaries".  Each stage has two
//! "actions" it will perform, one for the primary CPU, and another for the
//! secondaries.  During the measurement for a stage, each CPU will disable
//! interrupts, and then see how many times they can complete their assigned
//! action within a fixed measurement interval.
//!
//! During the first stage, all of the CPU actions will consist of simple
//! arithmetic in order to establish a baseline.  Subsequent stages will consist
//! of the tests of MSR register reads and writes, split into two phases.  In the
//! first phase the primary CPU will perform MSR reads/writes, while the
//! secondaries run the arithmetic action.  In the second phase, all of the CPUs
//! will perform the MSR read/writes performed by the primary CPU in the first
//! phase.
//!
//! After taking measurements for each stage the test threads shut down and the
//! results are printed.  If MSR reads/writes are not having an affect on other
//! CPUs, we expect to see the arithmetic numbers for secondaries to be basically
//! unchanged from the baseline established in the first stage when the primary
//! CPU is performing MSR accesses.  Likewise, if MSR accesses have no affect on
//! other CPUs, we expect all CPUs to show the same MSR performance when running
//! concurrently as the primary CPU did when it was the only CPU performing MSR
//! accesses.
//!
//! The console thread is used to sequence the benchmarks, but is not actually
//! responsible for taking any measurements.  It creates one thread per-active
//! CPU, each of which run with default weight and has hard affinity for one of
//! the currently active CPUs.  Each of these threads will
//! spin-sleep until the console thread tells them to start the next measurement
//! stage.
//!
//! At that point in time, all of the threads become more aggressive in their
//! spinning behavior.  Once realizing that the stage has started, each CPU
//! disables interrupts, and then each secondary CPU signals to the primary that
//! they are ready to start before spin-waiting on the signal from the primary
//! CPU to start.
//!
//! The primary spin-waits for the secondaries to become ready, then assigns a
//! deadline for the stage, finally signals to everyone that the measurement is
//! ready to start.  Each thread:
//! 1) Counts the number of times they are able to make it through their stage's
//!    measurement action before the deadline.
//! 2) Records the result.
//! 3) Signals to the console thread that they are finished.
//! 4) Re-enables interrupts.
//! 5) And finally waits for the console thread to tell them to start the next
//!    stage.
//!
//! Once all of the measurements have been taken, the measurement threads exit,
//! the console thread prints the results, and finally cleans up all of the test
//! resources.

use affine::{Ratio, Round};
use core::cell::UnsafeCell;
use core::ffi::{c_char, c_int, c_void};
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicUsize, Ordering};
use fbl::{Array, StringBuffer};
use kalloc::Box;
use kprint::{kprint, kprintln};
use libarch::x86::device_memory_barrier;
use zx_status::Status;
use zx_types::ZX_MAX_NAME_LEN;

use super::feature::{X86_FEATURE_TSC_DEADLINE, x86_feature_test};
use super::lapic::is_x2apic_enabled;
use super::registers::{X86_MSR_IA32_TSC_DEADLINE, X86_MSR_IA32_X2APIC_LVT_TIMER};
use super::x86::{read_msr, write_msr};
use crate::arch_rs::InterruptDisableGuard;
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::kernel::mp::{self, INVALID_CPU, cpu_num_t};
use crate::kernel::thread::{self, DEFAULT_PRIORITY, ThreadPtr};
use crate::platform_rs::timer::{
    DurationMono, InstantMono, timer_current_mono_ticks, timer_get_ticks_to_time_ratio, zx_sec,
};

type ActionFn = unsafe fn(u64, u64) -> u64;
type EnabledTestFn = fn() -> bool;

/// Defines the name of, and actions for, each measurement stage.
struct TestStage {
    name: &'static str,
    primary_action: ActionFn,
    secondary_action: ActionFn,
    enabled_test: EnabledTestFn,
}

impl TestStage {
    const fn new(
        name: &'static str,
        primary_action: ActionFn,
        secondary_action: ActionFn,
        enabled_test: EnabledTestFn,
    ) -> Self {
        Self { name, primary_action, secondary_action, enabled_test }
    }

    #[inline]
    fn enabled(&self) -> bool {
        (self.enabled_test)()
    }
}

/// Holds the result for a stage: start time, end time, and number of times that a CPU managed to
/// execute its action during the stage. When results are printed, they are normalized to show the
/// number of actions/second the CPU managed to execute.
///
/// Fields use `Atomic*` with `Ordering::Relaxed` for interior mutability through shared
/// `&CpuContext` references; happens-before synchronization between a worker thread's writes in
/// `run_context` and the control thread's reads in `run` is established by the `Ordering::SeqCst`
/// `finished_count` barrier.
#[derive(Default)]
struct StageResults {
    start: AtomicI64,
    end: AtomicI64,
    count: AtomicUsize,
}

/// The arithmetic action just does some simple adds and multiplies before exiting.
/// Note, we need to flag our accumulator as volatile in order to convince the compiler to not
/// simply optimize away this operation.
fn arithmetic_action(a: u64, b: u64) -> u64 {
    const CYCLES: u32 = 1 << 10;
    let mut acc: u64 = 0;

    for _i in 0..CYCLES {
        // SAFETY: `acc` is a live, properly aligned, initialized local stack variable with
        // exclusive access in this function.
        unsafe {
            let cur = core::ptr::read_volatile(&acc);
            core::ptr::write_volatile(&mut acc, cur.wrapping_add(a));
            let cur = core::ptr::read_volatile(&acc);
            core::ptr::write_volatile(&mut acc, cur.wrapping_mul(b));
        }
    }

    // SAFETY: `acc` is a live, properly aligned, initialized local stack variable.
    unsafe { core::ptr::read_volatile(&acc) }
}

/// Read the TSC Deadline register 256 times.
///
/// # Safety
///
/// Must only be called when `X86_FEATURE_TSC_DEADLINE` is supported on the current CPU and
/// interrupts are disabled.
unsafe fn tsc_deadline_read_action(_a: u64, _b: u64) -> u64 {
    const CYCLES: u32 = 1 << 8;

    for _i in 0..CYCLES {
        // SAFETY: Caller guarantees `X86_FEATURE_TSC_DEADLINE` is supported and interrupts are
        // disabled.
        core::hint::black_box(unsafe { read_msr(X86_MSR_IA32_TSC_DEADLINE) });
    }

    0
}

/// Read the TSC Deadline register, then write to it 256 times before finally restoring it to the
/// initially read value.
///
/// # Safety
///
/// Must only be called when `X86_FEATURE_TSC_DEADLINE` is supported on the current CPU and
/// interrupts are disabled.
unsafe fn tsc_deadline_write_action(_a: u64, _b: u64) -> u64 {
    const CYCLES: u32 = 1 << 8;
    // SAFETY: Caller guarantees `X86_FEATURE_TSC_DEADLINE` is supported and interrupts are disabled
    // during the measurement stage while restoring `original` before returning.
    unsafe {
        let original = read_msr(X86_MSR_IA32_TSC_DEADLINE);

        for i in 0..CYCLES {
            write_msr(
                X86_MSR_IA32_TSC_DEADLINE,
                original.wrapping_add(u64::from(i)).wrapping_add(1),
            );
        }

        write_msr(X86_MSR_IA32_TSC_DEADLINE, original);
        original
    }
}

/// Read the LVT Timer Interrupt control register 256 times.
///
/// # Safety
///
/// Must only be called when x2APIC is enabled on the current CPU and interrupts are disabled.
unsafe fn lvt_timer_read_action(_a: u64, _b: u64) -> u64 {
    const CYCLES: u32 = 1 << 8;

    for _i in 0..CYCLES {
        // SAFETY: Caller guarantees x2APIC is enabled and interrupts are disabled.
        core::hint::black_box(unsafe { read_msr(X86_MSR_IA32_X2APIC_LVT_TIMER) });
    }

    0
}

/// Read the LVT Timer Interrupt control register, then write to it toggling the Masked bit 256
/// times. Make sure that we also backup and restore the value in the TSC_DEADLINE register in the
/// process. When we perform a write to the timer interrupt control register, it will disable any
/// armed deadline. We can re-arm the deadline by writing to the deadline register again.
///
/// # Safety
///
/// Must only be called when both x2APIC and `X86_FEATURE_TSC_DEADLINE` are enabled on the current
/// CPU and interrupts are disabled.
unsafe fn lvt_timer_write_action(_a: u64, _b: u64) -> u64 {
    const CYCLES: u32 = 1 << 8;
    const MASK_BIT: u64 = 0x10000; // Intel SW Dev Manual, Vol 3, section 10.5.1

    // SAFETY: Caller guarantees both x2APIC and `X86_FEATURE_TSC_DEADLINE` are enabled, and
    // interrupts are disabled during the measurement stage.
    unsafe {
        let old_deadline = read_msr(X86_MSR_IA32_TSC_DEADLINE);
        let original = read_msr(X86_MSR_IA32_X2APIC_LVT_TIMER);
        let mut val = original;

        for _i in 0..CYCLES {
            val ^= MASK_BIT;
            write_msr(X86_MSR_IA32_X2APIC_LVT_TIMER, val);
        }

        write_msr(X86_MSR_IA32_X2APIC_LVT_TIMER, original);
        // Make sure we put an explicit MFENCE in-between the write to the timer
        // interrupt control register and the deadline register.  If the timer write
        // hits the register after the deadline write, it will disable the armed
        // deadline.
        device_memory_barrier();
        write_msr(X86_MSR_IA32_TSC_DEADLINE, old_deadline);
        original
    }
}

fn enable_tscd() -> bool {
    x86_feature_test(X86_FEATURE_TSC_DEADLINE)
}

fn enable_lvtt_rd() -> bool {
    is_x2apic_enabled()
}

fn enable_lvtt_wr() -> bool {
    is_x2apic_enabled() && enable_tscd()
}

const NUM_STAGES: usize = 9;

/// The definitions of each benchmark stage.
static STAGES: [TestStage; NUM_STAGES] = [
    TestStage::new("basic arithmetic", arithmetic_action, arithmetic_action, || true),
    TestStage::new("primary TSCD Rd", tsc_deadline_read_action, arithmetic_action, enable_tscd),
    TestStage::new("primary TSCD Wr", tsc_deadline_write_action, arithmetic_action, enable_tscd),
    TestStage::new("all TSCD Rd", tsc_deadline_read_action, tsc_deadline_read_action, enable_tscd),
    TestStage::new(
        "all TSCD Wr",
        tsc_deadline_write_action,
        tsc_deadline_write_action,
        enable_tscd,
    ),
    TestStage::new("primary LVTT Rd", lvt_timer_read_action, arithmetic_action, enable_lvtt_rd),
    TestStage::new("primary LVTT Wr", lvt_timer_write_action, arithmetic_action, enable_lvtt_wr),
    TestStage::new("all LVTT Rd", lvt_timer_read_action, lvt_timer_read_action, enable_lvtt_rd),
    TestStage::new("all LVTT Wr", lvt_timer_write_action, lvt_timer_write_action, enable_lvtt_wr),
];

/// Holds a CPU's context: the state for a CPU's thread, and the results for that CPU's
/// measurements.
struct CpuContext {
    owner: AtomicPtr<BenchmarkState>,
    thread: UnsafeCell<Option<ThreadPtr>>,
    cpu_id: cpu_num_t,
    is_primary: bool,
    results: [StageResults; NUM_STAGES],
}

// SAFETY: `CpuContext` instances are shared across worker threads by reference. `owner` is an
// `AtomicPtr`, `results` are only written by the assigned CPU's worker thread, and `thread` is
// exclusively accessed on the control thread during `init` and `cleanup`.
unsafe impl Sync for CpuContext {}

impl Default for CpuContext {
    fn default() -> Self {
        Self {
            owner: AtomicPtr::new(core::ptr::null_mut()),
            thread: UnsafeCell::new(None),
            cpu_id: INVALID_CPU,
            is_primary: false,
            results: Default::default(),
        }
    }
}

impl CpuContext {
    /// Initializes and resumes the worker thread for this CPU context.
    ///
    /// # Safety
    ///
    /// The caller must ensure that this method is only called on the control thread before the
    /// worker thread for `self` is running, and that `owner` and `arg` refer to live (not freed)
    /// objects that remain valid for the lifetime of the spawned thread (until `cleanup` joins it).
    unsafe fn init(
        &self,
        owner: *const BenchmarkState,
        entry: extern "C" fn(*mut c_void) -> c_int,
        arg: *mut c_void,
    ) -> Result<(), Status> {
        debug_assert!({
            // SAFETY: Only the control thread accesses `self.thread`, and the worker thread for
            // `self` has not been created yet.
            unsafe { (*self.thread.get()).is_none() }
        });
        debug_assert_ne!(self.cpu_id, INVALID_CPU);

        self.owner.store(owner.cast_mut(), Ordering::Relaxed);

        let mut name = StringBuffer::<ZX_MAX_NAME_LEN>::new();
        let _ = write!(name, "BenchmarkState {}", self.cpu_id);

        // Create our thread, then set its hard affinity its assigned CPU before
        // allowing it to run.
        // SAFETY: `name` is NUL-terminated, and caller guarantees `entry` and `arg` are valid
        // until `cleanup()` joins the thread.
        let thread = unsafe {
            thread::create_with_priority(
                name.as_ptr().cast::<c_char>(),
                entry,
                arg,
                DEFAULT_PRIORITY,
            )?
        };

        // SAFETY: Only the control thread accesses `self.thread`, and the worker thread has not
        // been resumed yet, so there is no concurrent access to `self.thread`.
        let thread_ref = unsafe { (*self.thread.get()).insert(thread) };

        // SAFETY: `thread_ref` was just created and has not been resumed or joined yet.
        unsafe {
            thread_ref.set_cpu_affinity(mp::cpu_num_to_mask(self.cpu_id));
            thread_ref.resume();
        }

        Ok(())
    }

    /// Cleans up and joins the worker thread for this CPU context.
    ///
    /// # Safety
    ///
    /// Must only be called on the control thread non-reentrantly.
    unsafe fn cleanup(&self) {
        // SAFETY: Only the control thread ever accesses `self.thread`, and `cleanup` is not called
        // re-entrantly.
        let thread = unsafe { (*self.thread.get()).take() };
        if let Some(thread) = thread {
            // SAFETY: `thread` is owned by `self` and joined at most once here.
            let _ = unsafe { thread.join(InstantMono::INFINITE) };
        }
    }
}

impl Drop for CpuContext {
    fn drop(&mut self) {
        debug_assert!(self.thread.get_mut().is_none());
    }
}

/// Top-level state for the benchmark. Holds each of the CPU contexts, as well as the atomic
/// variables used for advancing through each of the stages and synchronizing the CPU test threads
/// during the measurement phase of each stage.
#[derive(Default)]
struct BenchmarkState {
    cpu_contexts: Array<CpuContext>,
    shutdown_now: AtomicBool,
    stage_gate: AtomicUsize,
    ready_to_start_count: AtomicUsize,
    finished_count: AtomicUsize,
    ticks_deadline: AtomicI64,
}

impl BenchmarkState {
    const MEASUREMENT_TIME: i64 = zx_sec(1);

    fn wait_for_gate(&self, gate_id: usize) -> bool {
        while !self.shutdown_now.load(Ordering::SeqCst)
            && (self.stage_gate.load(Ordering::SeqCst) < gate_id)
        {
            let _ = thread::sleep_relative(DurationMono::from_millis(1));
        }

        !self.shutdown_now.load(Ordering::SeqCst)
    }

    /// Releases and joins all worker threads.
    ///
    /// # Safety
    ///
    /// Must only be called on the control thread non-reentrantly.
    unsafe fn cleanup(&self) {
        // Release any running threads from whatever they are doing.
        self.shutdown_now.store(true, Ordering::SeqCst);

        // Then clean them all up.
        for ctx in self.cpu_contexts.iter() {
            // SAFETY: Called on control thread non-reentrantly.
            unsafe { ctx.cleanup() };
        }
    }

    fn run(&mut self) -> c_int {
        // Figure out how many CPUs we have currently online.
        let mut online_cpus = mp::get_online_mask();
        let online_count = online_cpus.count_ones() as usize;

        // Allocate enough context storage for the online CPUs.
        let Ok(mut contexts) = Array::<CpuContext>::try_new(online_count) else {
            kprintln!(
                "Failed to allocate {} CpuContexts (mask 0x{:08x})",
                online_count,
                online_cpus
            );
            return -1;
        };

        let mut cpu_id: cpu_num_t = 0;
        let mut is_primary = true;
        let mut ndx: usize = 0;
        while online_cpus != 0 {
            if (online_cpus & 0x1) != 0 {
                contexts[ndx].cpu_id = cpu_id;
                contexts[ndx].is_primary = is_primary;
                ndx += 1;
            }

            cpu_id += 1;
            is_primary = false;
            online_cpus >>= 1;
        }
        self.cpu_contexts = contexts;

        // Now start each of the test threads.
        extern "C" fn thread_entry(arg: *mut c_void) -> c_int {
            // SAFETY: `arg` is `core::ptr::from_ref(ctx)` passed to `CpuContext::init`, and
            // `ctx.owner` points to the enclosing `BenchmarkState`, both of which remain shared
            // and valid until `this.cleanup()` joins all threads before `run` returns.
            let ctx = unsafe { &*arg.cast::<CpuContext>() };
            let owner_ptr = ctx.owner.load(Ordering::Relaxed);
            // SAFETY: `ctx.owner` was set before `thread.resume()` and points to the shared
            // `BenchmarkState`.
            let owner = unsafe { &*owner_ptr };
            owner.run_context(ctx)
        }

        // Reborrow `self` as a shared `&BenchmarkState` for the entire lifetime of the worker
        // threads so no `&mut BenchmarkState` or `&mut [CpuContext]` exists concurrently with the
        // shared references held by worker threads.
        let this: &BenchmarkState = &*self;
        let self_ptr: *const BenchmarkState = this;
        for ctx in this.cpu_contexts.iter() {
            let arg = core::ptr::from_ref(ctx).cast_mut().cast::<c_void>();
            // SAFETY: `this` (heap-allocated in `Box<BenchmarkState>`) and `this.cpu_contexts`
            // (heap-allocated in `Array<CpuContext>`) remain live and shared at stable addresses
            // until `this.cleanup()` joins all threads before `run` returns.
            if let Err(err) = unsafe { ctx.init(self_ptr, thread_entry, arg) } {
                kprintln!(
                    "Failed to initialize CpuContext for cpu {} (status {})",
                    ctx.cpu_id,
                    err.into_raw()
                );
                // SAFETY: Called on control thread non-reentrantly while `this: &BenchmarkState`
                // is live so any already-started worker threads exit before `run` returns.
                unsafe { this.cleanup() };
                return -1;
            }
        }

        // Cycle all of test threads through all of the stages.
        for (stage, s) in STAGES.iter().enumerate() {
            // Reset the stage sync state, and report which stage we are about to measure.
            this.ready_to_start_count.store(0, Ordering::SeqCst);
            this.finished_count.store(0, Ordering::SeqCst);
            kprintln!(
                "{:s} stage \"{:s}\".",
                if s.enabled() { "Measuring" } else { "Skipping" },
                s.name
            );
            let _ = thread::sleep_relative(DurationMono::from_millis(10));

            // Signal the threads that they may start the next measurement stage, and
            // wait until they have finished.
            this.stage_gate.store(stage + 1, Ordering::SeqCst);
            while this.finished_count.load(Ordering::SeqCst) < this.cpu_contexts.len() {
                let _ = thread::sleep_relative(DurationMono::from_millis(1));
            }
        }

        // Print out results and exit, cleaning up as we go.
        kprint!(" {:>22s} |", "Stage");
        for ctx in this.cpu_contexts.iter() {
            kprint!("      {:c}CPU {:2} |", if ctx.is_primary { '*' } else { ' ' }, ctx.cpu_id);
        }

        kprint!("\n------------------------+");
        for _i in 0..this.cpu_contexts.len() {
            kprint!("--------------+");
        }
        kprintln!("");

        for (stage, s) in STAGES.iter().enumerate() {
            if !s.enabled() {
                continue;
            }
            kprint!(" {:>22s} |", s.name);
            for ctx in this.cpu_contexts.iter() {
                debug_assert_eq!(STAGES.len(), ctx.results.len());

                let result = &ctx.results[stage];
                // `Relaxed` is sufficient because the `SeqCst` `finished_count` barrier above
                // synchronizes with the worker thread's `finished_count.fetch_add(1, SeqCst)`.
                let ticks_duration = result
                    .end
                    .load(Ordering::Relaxed)
                    .saturating_sub(result.start.load(Ordering::Relaxed));
                let time_duration =
                    timer_get_ticks_to_time_ratio().scale::<{ Round::DOWN }>(ticks_duration);
                if time_duration > 0 && time_duration <= i64::from(u32::MAX) {
                    let scaled = Ratio::new(zx_sec(1) as u32, time_duration as u32)
                        .scale::<{ Round::DOWN }>(result.count.load(Ordering::Relaxed) as i64);
                    kprint!(" {:12} |", scaled);
                } else {
                    kprint!(" {:>12s} |", "???");
                }
            }
            kprintln!("");
        }

        // SAFETY: Called on control thread non-reentrantly while `this: &BenchmarkState` is still
        // live so all worker threads have exited before `Drop::drop` takes `&mut BenchmarkState`.
        unsafe { this.cleanup() };
        0
    }

    fn run_context(&self, ctx: &CpuContext) -> c_int {
        let cpu_count = self.cpu_contexts.len();
        debug_assert!(cpu_count >= 1);

        // Run through all of the measurement stages, syncing up with the other
        // threads at each stage.
        for (stage, s) in STAGES.iter().enumerate() {
            // Wait until the control thread tells us it is OK to shut interrupts off
            // and to start the next measurement.  If something goes wrong, this wait
            // will return false, and we should bail out immediately.
            if !self.wait_for_gate(stage + 1) {
                return -1;
            }

            // It is time to take the next stage measurements.  Turn off interrupts for
            // the duration of the measurement cycle.
            {
                let _irqd = InterruptDisableGuard::new();

                // Only take the measurement if this stage is actually enabled.
                if s.enabled() {
                    // Are we the "primary" CPU?  If so, wait until all of the secondary
                    // CPUs are ready to go.  Then set up the deadline for the measurement
                    // cycle and join the group of ready threads (signaling that the
                    // measurement is ready to start).
                    //
                    // If we are a "secondary" CPU, simply indicate that we are ready to go,
                    // and wait for all of the other CPUs to be ready as well.
                    let action = if ctx.is_primary { s.primary_action } else { s.secondary_action };
                    if ctx.is_primary {
                        while self.ready_to_start_count.load(Ordering::SeqCst) < (cpu_count - 1) {
                            libarch::r#yield();
                        }

                        let ticks = timer_get_ticks_to_time_ratio()
                            .inverse()
                            .scale::<{ Round::DOWN }>(Self::MEASUREMENT_TIME);
                        self.ticks_deadline
                            .store(timer_current_mono_ticks().0 + ticks, Ordering::SeqCst);
                        self.ready_to_start_count.fetch_add(1, Ordering::SeqCst);
                    } else {
                        self.ready_to_start_count.fetch_add(1, Ordering::SeqCst);
                        while self.ready_to_start_count.load(Ordering::SeqCst) < cpu_count {
                            libarch::r#yield();
                        }
                    }

                    // OK, time to take the actual measurement.  See how many times we can
                    // make it through the measurement action before we hit the deadline, then
                    // record the start/end times, as well as the count.
                    let mut count: usize = 0;
                    let mut end: i64;
                    let deadline = self.ticks_deadline.load(Ordering::SeqCst);
                    let start = timer_current_mono_ticks().0;

                    loop {
                        // SAFETY: Stage is enabled (`s.enabled()` checked above), interrupts are
                        // disabled by `_irqd`, and calling CPU satisfies hardware prerequisites for
                        // `action`.
                        unsafe { action(0xc235_754e_f00c_463d, 0x9ba8_562d_dc09_32cf) };
                        count += 1;
                        end = timer_current_mono_ticks().0;
                        if end >= deadline {
                            break;
                        }
                    }

                    // Record our results; `Relaxed` is sufficient because the `SeqCst`
                    // `finished_count.fetch_add` below establishes happens-before with the control
                    // thread.
                    ctx.results[stage].start.store(start, Ordering::Relaxed);
                    ctx.results[stage].end.store(end, Ordering::Relaxed);
                    ctx.results[stage].count.store(count, Ordering::Relaxed);
                }

                // Signal that we are finished, then wait until everyone else is as well.
                self.finished_count.fetch_add(1, Ordering::SeqCst);
                while self.finished_count.load(Ordering::SeqCst) < cpu_count {
                    libarch::r#yield();
                }
            }
        }

        0
    }
}

impl Drop for BenchmarkState {
    fn drop(&mut self) {
        // SAFETY: `drop` runs exclusively on the control thread after the benchmark finishes.
        unsafe { self.cleanup() };
    }
}

/// # Safety
///
/// The caller must ensure nothing: `argv` is never dereferenced. The function is `unsafe` only to
/// match the console command signature.
unsafe extern "C" fn msr_bench(_argc: c_int, _argv: *const CmdArgs, _flags: u32) -> c_int {
    let Ok(mut benchmark) = Box::try_new(BenchmarkState::default()) else {
        kprintln!("Failed to allocate benchmark context!");
        return -1;
    };

    benchmark.run()
}

static_command!(
    CMD_MSR_BENCH,
    c"msr_bench".as_ptr(),
    c"MSR bechmarks".as_ptr(),
    msr_bench,
    CMD_AVAIL_NORMAL
);
