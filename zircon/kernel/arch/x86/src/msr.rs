// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use zx_status::Status;

use super::x86::{read_msr, read_msr_safe, write_msr, write_msr_safe};
use crate::kernel::mp::{MpIpiTarget, cpu_num_t, cpu_num_to_mask, is_cpu_online, sync_exec};
use crate::kernel::thread::{AutoPreemptDisabler, DEFAULT_PRIORITY, create_with_priority};
use crate::platform_rs::timer::InstantMono;

/// Reads MSR `msr_id` on CPU `cpu` and returns the 64-bit value.
///
/// Returns `Err(Status::BAD_STATE)` if `cpu` is offline or goes offline before executing the IPI
/// task.
///
/// # Safety
///
/// `msr_id` must be an implemented, readable MSR in `cpu`'s current operating mode (reading an
/// unimplemented MSR, a write-only MSR such as `IA32_PRED_CMD` or x2APIC `EOI`/`SELF_IPI`, or an
/// x2APIC MSR while x2APIC `EXTD` mode is disabled raises `#GP(0)`).
pub unsafe fn read_msr_on_cpu(cpu: cpu_num_t, msr_id: u32) -> Result<u64, Status> {
    if !is_cpu_online(cpu) {
        core::hint::cold_path();
        return Err(Status::BAD_STATE);
    }

    let executed = AtomicBool::new(false);
    let val = AtomicU64::new(0);
    let mask = cpu_num_to_mask(cpu);
    sync_exec(MpIpiTarget::Mask, mask, || {
        // SAFETY: Caller guarantees `msr_id` is an implemented, readable MSR on `cpu`.
        let msr_val = unsafe { read_msr(msr_id) };
        // Ordering::Relaxed is sufficient because `sync_exec` provides the cross-CPU
        // synchronization barrier before returning to the caller.
        val.store(msr_val, Ordering::Relaxed);
        executed.store(true, Ordering::Relaxed);
    });
    if !executed.into_inner() {
        core::hint::cold_path();
        return Err(Status::BAD_STATE);
    }
    Ok(val.into_inner())
}

/// Writes value `val` into MSR `msr_id` on CPU `cpu`.
///
/// Returns `Err(Status::BAD_STATE)` if `cpu` is offline or goes offline before executing the IPI
/// task.
///
/// # Safety
///
/// `msr_id` must be an implemented, writable MSR in `cpu`'s current operating mode, `val` must
/// satisfy all architectural bit and canonical-address constraints for `msr_id` (no reserved MBZ
/// bits or non-canonical addresses), and writing `val` must not violate memory safety or kernel
/// invariants.
pub unsafe fn write_msr_on_cpu(cpu: cpu_num_t, msr_id: u32, val: u64) -> Result<(), Status> {
    if !is_cpu_online(cpu) {
        core::hint::cold_path();
        return Err(Status::BAD_STATE);
    }

    let executed = AtomicBool::new(false);
    let mask = cpu_num_to_mask(cpu);
    sync_exec(MpIpiTarget::Mask, mask, || {
        // SAFETY: Caller guarantees `msr_id` is writable and `val` is valid on `cpu`.
        unsafe { write_msr(msr_id, val) };
        // Ordering::Relaxed is sufficient because `sync_exec` provides the cross-CPU
        // synchronization barrier before returning to the caller.
        executed.store(true, Ordering::Relaxed);
    });
    if !executed.into_inner() {
        core::hint::cold_path();
        return Err(Status::BAD_STATE);
    }
    Ok(())
}

fn run_safe_msr_on_cpu<F: FnOnce() -> Result<u64, Status> + Send>(
    cpu: cpu_num_t,
    op: F,
) -> Result<u64, Status> {
    struct SafeMsrTask<F> {
        cpu: cpu_num_t,
        op: Option<F>,
        result: Result<u64, Status>,
    }

    // `create_with_priority` requires a non-`unsafe` `extern "C" fn(*mut c_void) -> i32` start
    // routine; nesting `safe_msr_thread_entry` inside `run_safe_msr_on_cpu` ensures it can only
    // be passed to `create_with_priority` with `&raw mut task` below.
    extern "C" fn safe_msr_thread_entry<F: FnOnce() -> Result<u64, Status>>(
        arg: *mut c_void,
    ) -> i32 {
        // SAFETY: `arg` points to a valid `SafeMsrTask<F>` on `run_safe_msr_on_cpu`'s stack that
        // remains exclusively borrowed by this worker thread until `thrd.join` returns.
        let Some(task) = (unsafe { arg.cast::<SafeMsrTask<F>>().as_mut() }) else {
            return 0;
        };
        let _apd = AutoPreemptDisabler::new();
        if crate::arch_rs::curr_cpu_num() == task.cpu
            && let Some(op) = task.op.take()
        {
            task.result = op();
        }
        0
    }

    if crate::arch_rs::blocking_disallowed()
        || crate::arch_rs::ints_disabled()
        || !is_cpu_online(cpu)
    {
        core::hint::cold_path();
        return Err(Status::BAD_STATE);
    }

    // `read_msr_safe` and `write_msr_safe` must run in normal thread context
    // (`blocking_disallowed() == false`) with `AutoPreemptDisabler` held rather than inside
    // `sync_exec`, because `mp_sync_exec` sets `arch_blocking_disallowed() == true` and
    // `x86_exception_handler` treats `#GP(0)` while `blocking_disallowed` is set as fatal
    // interrupt-handler recursion before reaching `x86_gpf_handler`.
    let mut op = Some(op);
    {
        let _apd = AutoPreemptDisabler::new();
        if crate::arch_rs::curr_cpu_num() == cpu
            && let Some(f) = op.take()
        {
            return f();
        }
    }

    if !crate::kernel::thread::preempt_is_enabled()
        || !crate::kernel::scheduler::peek_is_active(cpu)
    {
        core::hint::cold_path();
        return Err(Status::BAD_STATE);
    }

    let mut task = SafeMsrTask { cpu, op, result: Err(Status::BAD_STATE) };
    // SAFETY: `safe_msr_thread_entry::<F>` is a valid thread entry function and `&raw mut task`
    // remains live on the stack until `thrd.join(InstantMono::INFINITE)` completes below.
    let thrd = unsafe {
        create_with_priority(
            c"msr_safe_on_cpu".as_ptr(),
            safe_msr_thread_entry::<F>,
            (&raw mut task).cast::<c_void>(),
            DEFAULT_PRIORITY,
        )
    }?;

    // Soft affinity avoids tripping the scheduler's active-mask debug assertion if `cpu` goes
    // inactive concurrently; `safe_msr_thread_entry` verifies `curr_cpu_num() == cpu` with
    // preemption disabled before executing `op`.
    // SAFETY: `thrd` was just created and has not been resumed or joined yet.
    unsafe {
        thrd.set_soft_cpu_affinity(cpu_num_to_mask(cpu));
        thrd.resume();
        let _ = thrd.join(InstantMono::INFINITE);
    }
    task.result
}

/// Safely reads MSR `msr_id` on CPU `cpu` with `#GP(0)` fault recovery.
///
/// Must be called from normal thread context with interrupts enabled (`!blocking_disallowed()` and
/// `!ints_disabled()`, plus `preempt_is_enabled()` when targeting a remote CPU). Returns
/// `Err(Status::BAD_STATE)` if those context requirements are not met or if `cpu` is offline or
/// inactive, or `Err(Status)` if reading `msr_id` raises a `#GP(0)` fault.
///
/// # Safety
///
/// While `#GP(0)` faults are caught, the caller must ensure that reading `msr_id` on `cpu` does
/// not trigger unintended hardware side effects.
pub unsafe fn read_msr_safe_on_cpu(cpu: cpu_num_t, msr_id: u32) -> Result<u64, Status> {
    run_safe_msr_on_cpu(cpu, || {
        // SAFETY: `#GP(0)` faults are caught by `read_msr_safe` in thread context; caller
        // guarantees reading `msr_id` does not trigger unintended hardware side effects.
        unsafe { read_msr_safe(msr_id) }
    })
}

/// Safely writes value `val` into MSR `msr_id` on CPU `cpu` with `#GP(0)` fault recovery.
///
/// Must be called from normal thread context with interrupts enabled (`!blocking_disallowed()` and
/// `!ints_disabled()`, plus `preempt_is_enabled()` when targeting a remote CPU). Returns
/// `Err(Status::BAD_STATE)` if those context requirements are not met or if `cpu` is offline or
/// inactive, or `Err(Status)` if writing `val` to `msr_id` raises a `#GP(0)` fault.
///
/// # Safety
///
/// While `#GP(0)` faults are caught, the caller must ensure that writing `val` to `msr_id` on
/// `cpu` does not violate memory safety or kernel invariants.
pub unsafe fn write_msr_safe_on_cpu(cpu: cpu_num_t, msr_id: u32, val: u64) -> Result<(), Status> {
    run_safe_msr_on_cpu(cpu, || {
        // SAFETY: `#GP(0)` faults are caught by `write_msr_safe` in thread context; caller
        // guarantees writing `val` to `msr_id` does not violate kernel invariants.
        unsafe { write_msr_safe(msr_id, val) }.map(|()| 0)
    })
    .map(|_| ())
}
