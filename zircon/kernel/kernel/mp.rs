// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! MP (multiprocessor) subsystem interface.

use super::deadline::Deadline;

pub use super::types::{cpu_mask_t, cpu_num_t};
use zx_status::Status;
use zx_types::zx_instant_mono_t;

/// Maximum number of CPUs the kernel is built to support; the Rust view of the
/// C++ `SMP_MAX_CPUS` build parameter.
pub const SMP_MAX_CPUS: usize =
    zr::parse_usize(env!("SMP_MAX_CPUS")).expect("SMP_MAX_CPUS invalid");

pub const INVALID_CPU: cpu_num_t = super::types::INVALID_CPU;
pub const CPU_MASK_ALL: cpu_mask_t = super::types::CPU_MASK_ALL;

/// C++ `MpUnplugEvent`, opaque here.  An unplugging CPU is handed one and signals
/// it once its state is flushed.
#[repr(C)]
pub struct MpUnplugEvent {
    _opaque: [u8; 0],
}

/// Signals `event`, releasing the CPU that requested this CPU's unplug.
///
/// # Safety
///
/// `event` must be the pointer handed to `arch_flush_state_and_halt()`.
pub unsafe fn unplug_event_signal(event: *mut MpUnplugEvent) {
    // SAFETY: forwarded to `MpUnplugEvent::Signal()` with a caller-verified pointer.
    unsafe { cpp_mp_unplug_event_signal(event) };
}

unsafe extern "C" {
    fn cpp_mp_set_cpu_online(cpu: cpu_num_t, online: bool);
    fn cpp_mp_set_curr_cpu_online(online: bool);
    fn cpp_mp_get_online_mask() -> cpu_mask_t;
    fn cpp_mp_is_cpu_online(cpu: cpu_num_t) -> bool;

    fn cpp_mp_signal_curr_cpu_ready();
    fn cpp_mp_wait_for_all_cpus_ready(deadline: *const Deadline) -> zx_types::zx_status_t;

    fn cpp_mp_reschedule(mask: cpu_mask_t, flags: u32);
    fn cpp_mp_reschedule_self();
    fn cpp_mp_interrupt(target: MpIpiTarget, mask: cpu_mask_t);

    fn cpp_mp_unplug_event_signal(event: *mut MpUnplugEvent);

    fn cpp_mp_mbx_generic_irq();
    fn cpp_mp_mbx_reschedule_irq();
    fn cpp_mp_mbx_interrupt_irq();

    fn cpp_mp_sync_exec(
        target: MpIpiTarget,
        mask: cpu_mask_t,
        task: SyncTask,
        context: *mut core::ffi::c_void,
    );

    fn cpp_mp_hotplug_cpu_mask(mask: cpu_mask_t) -> zx_types::zx_status_t;
    fn cpp_mp_hotplug_cpu(cpu: cpu_num_t) -> zx_types::zx_status_t;
    fn cpp_mp_unplug_current_cpu();
    fn cpp_mp_unplug_cpu_mask(
        mask: cpu_mask_t,
        deadline: zx_instant_mono_t,
    ) -> zx_types::zx_status_t;
    fn cpp_mp_unplug_cpu(cpu: cpu_num_t) -> zx_types::zx_status_t;
}

/// Target CPUs for inter-processor interrupts (IPIs).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpIpiTarget {
    /// Target a specific set of CPUs given by bitmask.
    Mask = 0,
    /// Target all online CPUs.
    All = 1,
    /// Target all online CPUs except the calling CPU.
    ///
    /// Note: Calling with `AllButLocal` requires interrupts to be disabled.
    AllButLocal = 2,
}

/// IPI types dispatched across processors.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MpIpi {
    /// Generic task execution IPI.
    Generic = 0,
    /// Reschedule IPI.
    Reschedule = 1,
    /// Interrupt-only IPI.
    Interrupt = 2,
    /// Halt CPU IPI.
    Halt = 3,
}

zr::static_assert!(core::mem::size_of::<MpIpiTarget>() == 1);
zr::static_assert!(core::mem::size_of::<MpIpi>() == 1);

/// Initializes the MP subsystem.
pub fn init() {}

/// Converts a CPU ID to a CPU bitmask.
pub const fn cpu_num_to_mask(cpu: cpu_num_t) -> cpu_mask_t {
    if cpu < cpu_mask_t::BITS { 1 << cpu } else { 0 }
}

/// Sets whether a given CPU is online and initialized.
pub fn set_cpu_online(cpu: cpu_num_t, online: bool) {
    // SAFETY: FFI call with value arguments has no safety preconditions.
    unsafe { cpp_mp_set_cpu_online(cpu, online) };
}

/// Sets whether the calling CPU is online and initialized.
pub fn set_curr_cpu_online(online: bool) {
    // SAFETY: FFI call has no safety preconditions.
    unsafe { cpp_mp_set_curr_cpu_online(online) };
}

/// Returns the bitmask of currently online CPUs.
pub fn get_online_mask() -> cpu_mask_t {
    // SAFETY: FFI call reads the kernel atomic online mask.
    unsafe { cpp_mp_get_online_mask() }
}

/// Checks whether a specific CPU is currently online.
pub fn is_cpu_online(cpu: cpu_num_t) -> bool {
    // SAFETY: FFI call with value argument has no safety preconditions.
    unsafe { cpp_mp_is_cpu_online(cpu) }
}

/// Signals that the current CPU has reached the ready state during startup.
pub fn signal_curr_cpu_ready() {
    // SAFETY: FFI call has no safety preconditions.
    unsafe { cpp_mp_signal_curr_cpu_ready() };
}

/// Waits until all CPUs in the system have signalled that they are ready, up to `deadline`.
pub fn wait_for_all_cpus_ready(deadline: Deadline) -> Result<(), Status> {
    // SAFETY: `&deadline` is a valid pointer to a `Deadline` struct for the call duration.
    let status = unsafe { cpp_mp_wait_for_all_cpus_ready(&deadline) };
    Status::ok(status)
}

/// Triggers reschedules on CPUs specified by `mask`.
pub fn reschedule(mask: cpu_mask_t, flags: u32) {
    // SAFETY: FFI call with value arguments has no safety preconditions.
    unsafe { cpp_mp_reschedule(mask, flags) };
}

/// Triggers a reschedule on the calling CPU using a self-IPI.
pub fn reschedule_self() {
    // SAFETY: FFI call has no safety preconditions.
    unsafe { cpp_mp_reschedule_self() };
}

/// Triggers an interrupt on target CPUs without a corresponding reschedule.
pub fn interrupt(target: MpIpiTarget, mask: cpu_mask_t) {
    // SAFETY: FFI call with value arguments has no safety preconditions.
    unsafe { cpp_mp_interrupt(target, mask) };
}

/// Generic mailbox interrupt handler.
pub fn mbx_generic_irq() {
    // SAFETY: FFI call has no safety preconditions.
    unsafe { cpp_mp_mbx_generic_irq() };
}

/// Reschedule mailbox interrupt handler.
pub fn mbx_reschedule_irq() {
    // SAFETY: FFI call has no safety preconditions.
    unsafe { cpp_mp_mbx_reschedule_irq() };
}

/// Interrupt-only mailbox interrupt handler.
pub fn mbx_interrupt_irq() {
    // SAFETY: FFI call has no safety preconditions.
    unsafe { cpp_mp_mbx_interrupt_irq() };
}

/// Raw function pointer callback for synchronous cross-CPU task execution.
pub type SyncTask = unsafe extern "C" fn(context: *mut core::ffi::c_void);

/// Executes a raw task function synchronously on the specified CPUs and blocks
/// until all target CPUs have completed execution.
///
/// # Safety
///
/// The caller must ensure that:
/// - `task` is safe to call concurrently from multiple CPUs with `context`.
/// - `context` remains valid for the duration of the execution.
/// - If `target == MpIpiTarget::AllButLocal`, interrupts are disabled on the calling CPU.
pub unsafe fn sync_exec_raw(
    target: MpIpiTarget,
    mask: cpu_mask_t,
    task: SyncTask,
    context: *mut core::ffi::c_void,
) {
    // SAFETY: The caller guarantees the validity of `task` and `context`.
    unsafe { cpp_mp_sync_exec(target, mask, task, context) };
}

/// Executes a closure synchronously across target CPUs and blocks until all
/// target CPUs have completed execution.
///
/// If `target == MpIpiTarget::AllButLocal`, interrupts must be disabled on the calling CPU.
pub fn sync_exec<F: Fn() + Sync>(target: MpIpiTarget, mask: cpu_mask_t, f: F) {
    unsafe extern "C" fn sync_exec_trampoline<F: Fn() + Sync>(context: *mut core::ffi::c_void) {
        // SAFETY: `context` points to a valid `F` on the caller's stack. Because `F: Sync`,
        // multiple CPUs can safely execute `closure()` concurrently via shared reference `&F`.
        let closure = unsafe { &*(context as *const F) };
        closure();
    }

    // SAFETY: `&f` remains valid on the stack for the entire duration of `cpp_mp_sync_exec`,
    // which blocks synchronously until all target CPUs have finished executing the trampoline.
    // TODO(https://fxbug.dev/517305410): This is not safe if CPUs are offlined and then brought
    // back online with this IPI still pending.
    unsafe {
        cpp_mp_sync_exec(
            target,
            mask,
            sync_exec_trampoline::<F>,
            &f as *const F as *mut core::ffi::c_void,
        );
    }
}

/// Hotplugs CPUs specified in `mask`, blocking until all CPUs are online or an error occurs.
pub fn hotplug_cpu_mask(mask: cpu_mask_t) -> Result<(), Status> {
    // SAFETY: FFI call with value argument has no safety preconditions.
    let status = unsafe { cpp_mp_hotplug_cpu_mask(mask) };
    Status::ok(status)
}

/// Hotplugs a single CPU by ID, blocking until it is online or an error occurs.
pub fn hotplug_cpu(cpu: cpu_num_t) -> Result<(), Status> {
    // SAFETY: FFI call with value argument has no safety preconditions.
    let status = unsafe { cpp_mp_hotplug_cpu(cpu) };
    Status::ok(status)
}

/// Unplugs the calling CPU, halting it and taking it offline.
pub fn unplug_current_cpu() {
    // SAFETY: FFI call terminates the current CPU.
    unsafe { cpp_mp_unplug_current_cpu() };
}

/// Unplugs CPUs specified by `mask`, waiting up to `deadline` for them to go offline.
pub fn unplug_cpu_mask(mask: cpu_mask_t, deadline: zx_instant_mono_t) -> Result<(), Status> {
    // SAFETY: FFI call with value arguments has no safety preconditions.
    let status = unsafe { cpp_mp_unplug_cpu_mask(mask, deadline) };
    Status::ok(status)
}

/// Unplugs a single CPU by ID, waiting indefinitely for it to go offline.
pub fn unplug_cpu(cpu: cpu_num_t) -> Result<(), Status> {
    // SAFETY: FFI call with value argument has no safety preconditions.
    let status = unsafe { cpp_mp_unplug_cpu(cpu) };
    Status::ok(status)
}

/// # Safety
///
/// `argv` must point to a valid array of at least `argc` [`crate::console::CmdArgs`] elements,
/// where `argc >= 1` and each element's `arg_str` is a valid null-terminated C string.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_mp(
    argc: core::ffi::c_int,
    argv: *const crate::console::CmdArgs,
    _flags: u32,
) -> core::ffi::c_int {
    use crate::arch_rs::{InterruptDisableGuard, curr_cpu_num};
    use crate::kernel::scheduler;
    use core::ffi::c_int;
    use kprint::kprintln;
    use zx_types::ZX_OK;

    // SAFETY: The caller guarantees `argc >= 1` and `argv` points to at least `argc` initialized
    // `CmdArgs` elements.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    let usage = || -> c_int {
        kprintln!("usage:");
        kprintln!("{:s} unplug <cpu_id>", argv[0].as_str());
        kprintln!("{:s} hotplug <cpu_id>", argv[0].as_str());
        kprintln!(
            "{:s} reschedule <cpu_id>        : send a reschedule ipi to <cpu_id>",
            argv[0].as_str()
        );
        Status::INTERNAL.into_raw()
    };
    if argc < 2 {
        kprintln!("not enough arguments");
        return usage();
    }

    if argv[1].as_str() == "unplug" {
        if argc < 3 {
            kprintln!("specify a cpu_id");
            return usage();
        }
        let status = Status::result_into_raw(unplug_cpu(argv[2].arg_uint as cpu_num_t));
        kprintln!(
            "CPU {} unplug {:s} {}",
            argv[2].arg_uint,
            if status == ZX_OK { "succeeded" } else { "failed" },
            status
        );
    } else if argv[1].as_str() == "hotplug" {
        if argc < 3 {
            kprintln!("specify a cpu_id");
            return usage();
        }
        let status = Status::result_into_raw(hotplug_cpu(argv[2].arg_uint as cpu_num_t));
        kprintln!(
            "CPU {} hotplug {:s} {}",
            argv[2].arg_uint,
            if status == ZX_OK { "succeeded" } else { "failed" },
            status
        );
    } else if argv[1].as_str() == "reschedule" {
        if argc < 3 {
            kprintln!("specify a cpu_id");
            return usage();
        }

        let target_cpu = argv[2].arg_uint as cpu_num_t;
        if !scheduler::peek_is_active(target_cpu) {
            kprintln!("target cpu {} is not active", target_cpu);
            return ZX_OK;
        }

        let mask = cpu_num_to_mask(target_cpu);
        let sending_cpu;
        {
            // Disable interrupts so that the sending CPU we record here cannot change
            // during our call to mp_reschedule.
            let _irqd = InterruptDisableGuard::new();
            sending_cpu = curr_cpu_num();
            reschedule(mask, 0);
        }

        if sending_cpu == target_cpu {
            kprintln!("sending cpu is same as target cpu, no ipi sent");
        } else {
            kprintln!("sent reschedule ipi to cpu {}", target_cpu);
        }
    } else {
        kprintln!("unknown command");
        return usage();
    }

    ZX_OK
}

#[cfg(console_enabled)]
crate::console::static_command!(
    CMD_MP,
    c"mp".as_ptr(),
    c"mp test commands".as_ptr(),
    cmd_mp,
    crate::console::CMD_AVAIL_NORMAL
);

/// Kernel MP unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "mp_rust")]
mod tests {
    use crate::arch_rs::InterruptDisableGuard;
    use crate::kernel::thread::preempt_is_enabled;
    use crate::platform_rs::timer::InstantUnknown;
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use unittest::{assert_false, assert_true, subtest};

    /// Verifies querying online CPU mask and per-CPU online status.
    #[test]
    fn test_mp_online_mask() {
        let mask = get_online_mask();
        unittest::expect_ne!(mask, 0);

        for cpu in 0..32 {
            let is_set = (mask & (1 << cpu)) != 0;
            unittest::expect_eq!(is_cpu_online(cpu), is_set);
        }
    }

    /// Verifies synchronous cross-CPU closure execution via `sync_exec` across various targets.
    #[test]
    fn test_mp_sync_exec() {
        let online = get_online_mask();
        let num_online = online.count_ones();

        // 1. Target with explicit bitmask.
        let count_mask = AtomicU32::new(0);
        sync_exec(MpIpiTarget::Mask, online, || {
            count_mask.fetch_add(1, Ordering::SeqCst);
        });
        unittest::expect_eq!(count_mask.load(Ordering::SeqCst), num_online);

        // 2. Target all online CPUs.
        let count_all = AtomicU32::new(0);
        sync_exec(MpIpiTarget::All, 0, || {
            count_all.fetch_add(1, Ordering::SeqCst);
        });
        unittest::expect_eq!(count_all.load(Ordering::SeqCst), num_online);
    }

    /// Verifies waiting for all CPUs to be ready during kernel test execution.
    #[test]
    fn test_mp_ready() {
        let status = wait_for_all_cpus_ready(Deadline::no_slack(InstantUnknown(0)));
        unittest::expect_ok!(status);
    }

    // Verify that preemption is disabled during mp_sync_exec'd tasks.
    /// mp_sync_exec_preempt_disabled
    #[test]
    fn mp_sync_exec_preempt_disabled() {
        let test_with_target_mask = subtest!(|target: MpIpiTarget, mask: cpu_mask_t| {
            assert_true!(preempt_is_enabled());

            let preempt_enabled = AtomicBool::new(false);
            sync_exec(target, mask, || {
                if preempt_is_enabled() {
                    preempt_enabled.store(true, Ordering::Relaxed);
                }
            });

            assert_false!(preempt_enabled.load(Ordering::Relaxed));

            assert_true!(preempt_is_enabled());
        });

        // 1. ALL
        assert_true!(test_with_target_mask(MpIpiTarget::All, /* ignored */ 0));
        {
            let _irqd = InterruptDisableGuard::new();
            assert_true!(test_with_target_mask(MpIpiTarget::All, /* ignored */ 0));
        }

        // 2. ALL_BUT_LOCAL
        {
            let _irqd = InterruptDisableGuard::new();
            assert_true!(test_with_target_mask(MpIpiTarget::AllButLocal, /* ignored */ 0));
        }

        // 3. MASK
        assert_true!(test_with_target_mask(MpIpiTarget::Mask, CPU_MASK_ALL));
        {
            let _irqd = InterruptDisableGuard::new();
            assert_true!(test_with_target_mask(MpIpiTarget::Mask, CPU_MASK_ALL));
        }
    }
}

/// Synchronous IPI unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "sync_ipi_tests_rust")]
mod sync_ipi_tests {
    use crate::arch_rs::{
        InterruptDisableGuard, blocking_disallowed, curr_cpu_num, ints_disabled, max_num_cpus,
    };
    use crate::kernel::event::Event;
    use crate::kernel::thread::{self, DEFAULT_PRIORITY, ThreadPtr};
    use crate::platform_rs::timer::InstantMono;
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicI32, Ordering};
    use debug::{ltracef, tracef};
    use kprint::kprintln;
    use pin_init::stack_pin_init;

    const LOCAL_TRACE: u32 = 0;

    const TEST_RUNS: u32 = 1000;

    fn inorder_count_task(inorder_counter: &AtomicI32) {
        assert!(ints_disabled());
        assert!(blocking_disallowed());
        let cpu_num = curr_cpu_num();

        let oldval = inorder_counter.fetch_add(1, Ordering::SeqCst);
        assert_eq!(oldval, cpu_num as i32);
        ltracef!("  CPU {} checked in\n", cpu_num);
    }

    fn counter_task(counter: &AtomicI32) {
        assert!(ints_disabled());
        assert!(blocking_disallowed());
        counter.fetch_add(1, Ordering::SeqCst);
    }

    extern "C" fn deadlock_test_thread(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` points to a valid `Event` that outlives this thread (joined in
        // `deadlock_test` before `gate` goes out of scope).
        let gate = unsafe { &*(arg as *const Event) };
        let _ = gate.wait_infinite();

        let counter = AtomicI32::new(0);
        let _block_interrupts = InterruptDisableGuard::new();
        sync_exec(MpIpiTarget::AllButLocal, 0, || counter_task(&counter));
        0
    }

    fn deadlock_test() {
        // Test for a deadlock caused by multiple CPUs broadcasting concurrently.
        stack_pin_init!(let gate = Event::init_unsignaled());
        let gate_ptr = (&raw const *gate).cast_mut().cast::<c_void>();

        let mut threads: [Option<ThreadPtr>; 5] = [None; 5];
        for slot in &mut threads {
            // SAFETY: `deadlock_test_thread` and `gate_ptr` are valid, and all created threads
            // are joined below before `gate` goes out of scope.
            match unsafe {
                thread::create_with_priority(
                    c"sync_ipi_deadlock".as_ptr(),
                    deadlock_test_thread,
                    gate_ptr,
                    DEFAULT_PRIORITY,
                )
            } {
                Ok(t) => {
                    // SAFETY: `t` was just created and has not been resumed or joined yet.
                    unsafe { t.resume() };
                    *slot = Some(t);
                }
                Err(_) => {
                    tracef!("  failed to create thread\n");
                    break;
                }
            }
        }

        gate.signal();

        for thread in threads.into_iter().flatten() {
            // SAFETY: Each thread in `threads` was created above and has not been joined yet.
            let _ = unsafe { thread.join(InstantMono::INFINITE) };
        }
    }

    /// sync_ipi_tests
    #[test]
    fn sync_ipi_tests() {
        let num_cpus = max_num_cpus();
        let online = get_online_mask();
        let expected_online = 1u32.checked_shl(num_cpus).unwrap_or(0).wrapping_sub(1);
        if online != expected_online {
            kprintln!("Can only run test with all CPUs online");
            return true;
        }

        let runs = TEST_RUNS;

        // Test that we're actually blocking and only signaling the ones we target.
        for _ in 0..runs {
            ltracef!("Sequential test\n");
            let inorder_counter = AtomicI32::new(0);
            for j in 0..num_cpus {
                sync_exec(MpIpiTarget::Mask, 1u32 << j, || inorder_count_task(&inorder_counter));
                ltracef!("  Finished signaling CPU {}\n", j);
            }
        }

        // Test that we can signal multiple CPUs at the same time.
        for _ in 0..runs {
            ltracef!("Counter test ({} CPUs)\n", num_cpus);
            let counter = AtomicI32::new(0);

            {
                let _irqd = InterruptDisableGuard::new();

                sync_exec(MpIpiTarget::AllButLocal, 0, || counter_task(&counter));
            }

            ltracef!("  Finished signaling all but local ({})\n", counter.load(Ordering::SeqCst));
            unittest::assert_eq!(counter.load(Ordering::SeqCst) as u32, num_cpus - 1);
        }

        for _ in 0..runs {
            ltracef!("Deadlock test\n");
            deadlock_test();
            ltracef!("Deadlock test passed\n");
        }
    }
}
