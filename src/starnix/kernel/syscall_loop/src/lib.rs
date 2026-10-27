// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "256"]

use anyhow::{Error, format_err};
use extended_pstate::ExtendedPstatePointer;
use starnix_core::arch::execution::new_syscall;
use starnix_core::signals::{SignalInfo, dequeue_signal, force_signal, prepare_to_restart_syscall};
use starnix_core::task::{CurrentTask, ExceptionResult, ExitStatus, SeccompStateValue, TaskFlags};
use starnix_logging::{
    CATEGORY_STARNIX, NAME_HANDLE_EXCEPTION, NAME_RESTRICTED_KICK, NAME_RUN_TASK, log_error,
    log_syscall, log_trace, log_warn, set_current_task_info,
};
use starnix_registers::RestrictedState;

use starnix_syscalls::SyscallResult;
use starnix_syscalls::decls::{Syscall, SyscallDecl};
use starnix_uapi::errno;
use starnix_uapi::errors::Errno;
use starnix_uapi::signals::{SIGKILL, SIGSEGV};
use zerocopy::FromZeros;

mod table;

pub fn enter(current_task: &mut CurrentTask) -> ExitStatus {
    // Zircon will populate this report on restricted exception exits. Initialize it to all zero
    // since we're just reserving storage.
    let mut exception_report = zx::sys::zx_exception_report_t::new_zeroed();
    match RestrictedState::bind_and_map(
        &mut current_task.thread_state.registers,
        &mut exception_report,
    ) {
        Ok(restricted_state) => {
            match run_task(current_task, restricted_state.bound_state.as_ptr(), &exception_report) {
                Ok(ok) => ok,
                Err(error) => {
                    log_warn!("Died unexpectedly from {error:?}! treating as SIGKILL");
                    ExitStatus::Kill(SignalInfo::kernel(SIGKILL))
                }
            }
        }
        Err(error) => {
            log_error!("failed to map mode state vmo, {error:?}! treating as SIGKILL");
            ExitStatus::Kill(SignalInfo::kernel(SIGKILL))
        }
    }
}

unsafe extern "C" {
    /// Enters restricted mode in a loop, invoking `restricted_exit_callback_c` on each restricted
    /// exit until the callback returns `false` or `zx_restricted_enter` fails.
    ///
    /// # Safety
    ///
    /// * `options` must be a valid `zx_restricted_enter` options value.
    /// * `restricted_exit_callback_context` must point to a valid [`RestrictedEnterContext`] that
    ///   is exclusively accessed by the current thread and outlives this call.
    /// * `restricted_state` must point to the current thread's bound and mapped
    ///   `zx_restricted_state_t`, which must outlive this call.
    /// * `extended_pstate_ptr_ptr` must point to a valid [`ExtendedPstatePointer`] whose backing
    ///   storage matches the architectural mode selected by `restricted_state` and outlives this
    ///   call.
    // rustc doesn't like RestrictedEnterContext for FFI but we're just passing it back to
    // ourselves with extra steps.
    #[allow(improper_ctypes)]
    fn restricted_enter_loop(
        options: u32,
        restricted_exit_callback_context: *mut RestrictedEnterContext<'_>,
        restricted_state: *mut zx::sys::zx_restricted_state_t,
        extended_pstate_ptr_ptr: *mut ExtendedPstatePointer,
    ) -> zx::sys::zx_status_t;
}

const RESTRICTED_ENTER_OPTIONS: u32 = 0;

struct RestrictedEnterContext<'a> {
    current_task: &'a mut CurrentTask,
    error_context: Option<ErrorContext>,
    exit_status: Result<ExitStatus, Error>,
    exception_report_raw: *const zx::sys::zx_exception_report_t,
}

/// Runs the `current_task` to completion.
///
/// The high-level flow of this function looks as follows:
///
///   1. Write the restricted state for the current thread to set it up to enter into the restricted
///      (Linux) part of the address space.
///   2. Enter restricted mode.
///   3. Return from restricted mode, reading out the new state of the restricted mode execution.
///      This state contains the thread's restricted register state, which is used to determine
///      which system call to dispatch.
///   4. Dispatch the system call.
///   5. Handle pending signals.
///   6. Goto 1.
fn run_task(
    current_task: &mut CurrentTask,
    restricted_state_ptr: *mut zx::sys::zx_restricted_state_t,
    exception_report_raw: *const zx::sys::zx_exception_report_t,
) -> Result<ExitStatus, Error> {
    set_current_task_info(
        current_task.task.command(),
        current_task.task.thread_group().read().leader_command(),
        current_task.task.pid.id,
        current_task.tid.id,
    );

    fuchsia_trace::duration!(CATEGORY_STARNIX, NAME_RUN_TASK);

    // This tracks the last failing system call for debugging purposes.
    let error_context = None;

    // We need to check for exit once, before the task starts executing, in case
    // the task has already been sent a signal that will cause it to exit.
    if let Some(exit_status) = process_completed_restricted_exit(current_task, &error_context)? {
        return Ok(exit_status);
    }

    let mut restricted_enter_context = RestrictedEnterContext {
        current_task,
        error_context,
        exit_status: Err(errno!(ENOEXEC).into()),
        exception_report_raw,
    };

    loop {
        let mut extended_pstate_ptr =
            restricted_enter_context.current_task.thread_state.extended_pstate.as_ptr();
        // SAFETY:
        // * `RESTRICTED_ENTER_OPTIONS` is a valid `zx_restricted_enter` options value.
        // * `restricted_enter_context` is exclusively accessed by the current thread, outlives the
        //   call, and is passed as a raw pointer to avoid asserting an intermediate `&mut` borrow.
        // * `restricted_state_ptr` points to the current thread's bound and mapped
        //   `zx_restricted_state_t`, which outlives this function.
        // * `extended_pstate_ptr` is refreshed on each iteration from `current_task.thread_state`
        //   so its backing storage matches the architectural mode in `restricted_state_ptr`.
        let restricted_enter_status = zx::Status::ok(unsafe {
            restricted_enter_loop(
                RESTRICTED_ENTER_OPTIONS,
                &raw mut restricted_enter_context,
                restricted_state_ptr,
                &raw mut extended_pstate_ptr,
            )
        });
        match restricted_enter_status {
            Ok(()) => break,
            Err(zx::Status::BAD_STATE) => {
                // `zx_restricted_enter` returns `ZX_ERR_BAD_STATE` if the restricted register
                // state fails architectural pre-entry validation (e.g. an invalid user IP or flags).
                // Deliver a forced `SIGSEGV` (`SI_KERNEL`) and process the completed exit to
                // dispatch a signal handler or terminate the task.
                force_signal(
                    restricted_enter_context.current_task,
                    SignalInfo::forced(SIGSEGV),
                    /* restricted_exception = */ None,
                );
                if let Some(exit_status) = process_completed_restricted_exit(
                    restricted_enter_context.current_task,
                    &restricted_enter_context.error_context,
                )? {
                    return Ok(exit_status);
                }
            }
            Err(status) => {
                // If restricted_enter_loop failed, it means that we failed to satisfy
                // a prerequisite of zx_restricted_enter which should never happen.
                log_error!(
                    "restricted_enter_loop failed: {}, register state: {:?}",
                    status,
                    restricted_enter_context.current_task.thread_state.registers
                );
                break;
            }
        }
    }
    restricted_enter_context.exit_status
}

/// Entry point invoked from the architecture-specific `restricted_return_loop` assembly
/// trampoline upon each exit from restricted mode.
///
/// # Safety
///
/// * `context` must point to the valid [`RestrictedEnterContext`] passed to
///   `restricted_enter_loop` on the current thread.
/// * `extended_pstate_ptr_ptr` must point to the valid [`ExtendedPstatePointer`] passed to
///   `restricted_enter_loop` on the current thread.
#[allow(private_interfaces)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn restricted_exit_callback_c(
    context: *mut RestrictedEnterContext<'_>,
    reason_code: zx::sys::zx_restricted_reason_t,
    extended_pstate_ptr_ptr: *mut ExtendedPstatePointer,
) -> bool {
    // SAFETY:
    // `context` is a pointer to a `RestrictedEnterContext` that was passed to
    // `restricted_enter_loop`.
    //  `extended_pstate_ptr` is a pointer to the ExtendedPstatePointer instance
    //  that was passed to `restricted_enter_loop.`
    // Our restricted return assembly and Zircon together guarantee that this
    // thread has exclusive access to these variables.
    let (restricted_context, extended_pstate_ptr) =
        unsafe { (&mut *context, extended_pstate_ptr_ptr.as_mut_unchecked()) };
    restricted_exit_callback(
        reason_code,
        restricted_context.current_task,
        &mut restricted_context.error_context,
        &mut restricted_context.exit_status,
        extended_pstate_ptr,
        restricted_context.exception_report_raw,
    )
}

fn restricted_exit_callback(
    reason_code: zx::sys::zx_restricted_reason_t,
    current_task: &mut CurrentTask,
    error_context: &mut Option<ErrorContext>,
    exit_status: &mut Result<ExitStatus, Error>,
    extended_pstate_ptr: &mut ExtendedPstatePointer,
    exception_report_raw: *const zx::sys::zx_exception_report_t,
) -> bool {
    debug_assert_eq!(
        current_task.thread_state.restart_code, None,
        "restart_code should only ever be Some() in normal mode",
    );

    let ret = match process_restricted_exit(
        reason_code,
        current_task,
        error_context,
        exception_report_raw,
    ) {
        Ok(None) => {
            // Keep going!

            *extended_pstate_ptr = current_task.thread_state.extended_pstate.as_ptr();

            true
        }
        Ok(Some(completed_exit_status)) => {
            *exit_status = Ok(completed_exit_status);
            false
        }
        Err(error) => {
            *exit_status = Err(error);
            false
        }
    };

    debug_assert_eq!(
        current_task.thread_state.restart_code, None,
        "restart_code should only ever be Some() in normal mode",
    );

    ret
}

fn process_restricted_exit(
    reason_code: zx::sys::zx_restricted_reason_t,
    current_task: &mut CurrentTask,
    error_context: &mut Option<ErrorContext>,
    exception_report_raw: *const zx::sys::zx_exception_report_t,
) -> Result<Option<ExitStatus>, Error> {
    #[cfg(target_arch = "aarch64")]
    current_task.thread_state.registers.sync_stack_ptr();

    match reason_code {
        zx::sys::ZX_RESTRICTED_REASON_SYSCALL => {
            execute_syscall(current_task, error_context);
        }
        zx::sys::ZX_RESTRICTED_REASON_EXCEPTION => {
            if !current_task.is_exitted() {
                fuchsia_trace::duration!(CATEGORY_STARNIX, NAME_HANDLE_EXCEPTION);
                // SAFETY: `exception_report_raw` was written by Zircon during this restricted exit.
                let exception_report =
                    unsafe { zx::ExceptionReport::from_raw(*exception_report_raw) };
                let exception_result = current_task.process_exception(&exception_report);
                process_completed_exception(current_task, exception_result, exception_report);
            }
        }
        zx::sys::ZX_RESTRICTED_REASON_KICK => {
            fuchsia_trace::instant!(
                CATEGORY_STARNIX,
                NAME_RESTRICTED_KICK,
                fuchsia_trace::Scope::Thread
            );
            // Fall through to the post-syscall / post-exception handling logic. We were likely
            // kicked because a signal is pending deliver or the task has exited. Spurious kicks are
            // also possible.
        }
        _ => {
            return Err(format_err!("Received unexpected restricted reason code: {}", reason_code));
        }
    }

    if let Some(exit_status) = process_completed_restricted_exit(current_task, &error_context)? {
        return Ok(Some(exit_status));
    }

    Ok(None)
}

fn process_completed_exception(
    current_task: &mut CurrentTask,
    exception_result: ExceptionResult,
    restricted_exception: zx::ExceptionReport,
) {
    match exception_result {
        ExceptionResult::Handled => {}
        ExceptionResult::Signal(signal) => {
            force_signal(current_task, signal, Some(restricted_exception));
        }
    }
}

/// Contains context to track the most recently failing system call.
///
/// When a task exits with a non-zero exit code, this context is logged to help debugging which
/// system call may have triggered the failure.
#[derive(Debug)]
pub struct ErrorContext {
    /// The system call that failed.
    pub syscall: Syscall,

    /// The error that was returned for the system call.
    pub error: Errno,
}

/// Executes the syscall for `current_task`.
///
/// Stores an `ErrorContext` in `error_context` if the system call returned an error, and leaves it
/// unchanged otherwise, so that the most recent failure is preserved across successful syscalls.
///
/// `error_context` is an out-parameter rather than an `Option<ErrorContext>` return value because
/// of that preservation requirement: a returned `None` must not overwrite the previous
/// `ErrorContext`, so the return value cannot be constructed directly in `*error_context`. It would
/// instead go through a separate hidden return slot that is written on every syscall and copied
/// into `*error_context` on error. With the out-parameter, the common `Ok` path does not touch
/// `error_context` at all.
#[inline(never)] // Inlining this function breaks the CFI directives used to unwind into user code.
pub fn execute_syscall(current_task: &mut CurrentTask, error_context: &mut Option<ErrorContext>) {
    current_task.thread_state.registers.save_registers_for_restart();

    current_task.ptrace().on_syscall_enter();
    // A tracee killed (e.g., via `SIGKILL` or `PTRACE_KILL`) while stopped in `PTRACE_SYSCALL`
    // entry is marked `EXITED` and woken from `on_syscall_enter` without executing the syscall.
    if current_task.is_exitted() {
        return;
    }

    let syscall_decl = SyscallDecl::from_number(
        current_task.thread_state.registers.syscall_register(),
        current_task.thread_state.arch_width(),
    );

    let _syscall_trace = starnix_core::task::begin_syscall_trace(|| syscall_decl.trace_name());
    let syscall = new_syscall(syscall_decl, current_task);

    log_syscall!(current_task, "{syscall:?}");

    let _lockup_detector_guard = starnix_core::task::ThreadLockupDetector::track();
    let result: Result<SyscallResult, Errno> =
        if current_task.seccomp_filter_state.get() != SeccompStateValue::None {
            // Inlined fast path for seccomp, so that we don't incur the cost
            // of a method call when running the filters.
            if let Some(res) = current_task.run_seccomp_filters(&syscall) {
                res
            } else {
                table::dispatch_syscall(current_task, &syscall)
            }
        } else {
            table::dispatch_syscall(current_task, &syscall)
        };

    current_task.trigger_delayed_releaser();

    let is_error = match result {
        Ok(return_value) => {
            log_syscall!(current_task, "-> {:#x}", return_value.value());
            current_task.thread_state.registers.set_return_register(return_value.value());
            false
        }
        Err(errno) => {
            log_syscall!(current_task, "!-> {errno}");
            if errno.is_restartable() {
                current_task.thread_state.restart_code = Some(errno.code);
            }
            current_task.thread_state.registers.set_return_register(errno.return_value());
            *error_context = Some(ErrorContext { error: errno, syscall });
            true
        }
    };

    current_task.ptrace().on_syscall_exit(is_error);
}

/// Finishes `current_task` updates after a restricted mode exit such as a syscall, exception, or kick.
///
/// Returns an `ExitStatus` if the task is meant to exit.
pub fn process_completed_restricted_exit(
    current_task: &mut CurrentTask,
    error_context: &Option<ErrorContext>,
) -> Result<Option<ExitStatus>, Errno> {
    let result;
    loop {
        // Checking for a signal might cause the task to exit, so check before processing exit
        {
            {
                if !current_task.is_exitted() {
                    dequeue_signal(current_task);
                }
                // The syscall may need to restart for a non-signal-related
                // reason. This call does nothing if we aren't restarting.
                prepare_to_restart_syscall(&mut current_task.thread_state, None);
            }
        }

        let exit_status = current_task.exit_status();
        if let Some(exit_status) = exit_status {
            log_trace!("exiting with status {:?}", exit_status);
            if let Some(error_context) = error_context {
                match exit_status {
                    ExitStatus::Exit(value) if value == 0 => {}
                    _ => {
                        log_trace!(
                            "last failing syscall before exit: {:?}, failed with {:?}",
                            error_context.syscall,
                            error_context.error
                        );
                    }
                };
            }

            result = Some(exit_status);
            break;
        } else {
            // Block a stopped process after it's had a chance to handle signals, since a signal might
            // cause it to stop.
            if current_task.block_if_stopped() {
                // If the task was stopped and has now woken up (e.g., via SIGCONT or PTRACE_CONT),
                // loop back to process any pending signals before returning to userspace.
                continue;
            }
            // Restore signal mask if necessary, before returning to userspace.
            if current_task.flags().contains(TaskFlags::TEMPORARY_SIGNAL_MASK) {
                current_task.write().restore_signal_mask();
                if current_task.should_check_for_pending_signals() {
                    continue;
                }
            }

            result = None;
            break;
        }
    }

    if let Some(ExitStatus::CoreDump(signal_info)) = &result {
        if current_task.flags().contains(TaskFlags::DUMP_ON_EXIT) {
            // Avoid taking a backtrace if the signal was sent by the same task.
            if !signal_info.is_sent_by(&current_task.weak_task()) {
                // Request a backtrace before reporting the crash to increase chance of a backtrace
                // in logs. This call is kept as far up in the call stack as possible to avoid
                // additional frames that are always the same and not relevant to users.
                // TODO(https://fxbug.dev/356732164) collect a backtrace ourselves
                debug::backtrace_request_current_thread();
            }

            if let Some(pending_report) =
                current_task.kernel().crash_reporter.begin_crash_report(&current_task)
            {
                current_task.kernel().crash_reporter.handle_core_dump(
                    &current_task,
                    signal_info,
                    pending_report,
                );
            }
        }
    }
    return Ok(result);
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    use super::*;
    use core::arch::asm;
    use extended_pstate::{
        ExtendedAarch32PstateState, ExtendedPstatePointer, ExtendedPstateState,
        restore_extended_pstate, save_extended_aarch32_pstate, save_extended_pstate,
    };

    unsafe extern "C" {
        fn zx_vmar_root_self() -> zx::sys::zx_handle_t;
    }

    /// Seeds the hardware extended processor state from `seed_pstate_ptr`, invokes
    /// `restricted_enter_loop` with `options = u32::MAX` (so `.restricted_enter_loop_top`
    /// dispatches the CPSR-dependent extended processor state restore and `zx_restricted_enter`
    /// immediately returns `ZX_ERR_INVALID_ARGS`), and saves the resulting hardware extended
    /// processor state into `out_pstate_ptr`.
    ///
    /// # Safety
    ///
    /// * `seed_pstate_ptr` and `out_pstate_ptr` must point to valid `ExtendedPstatePointer`s
    ///   backed by 64-bit `ExtendedPstateState` storage.
    /// * `restricted_state` must point to a valid `zx_restricted_state_t`.
    /// * `in_pstate_ptr` must point to a valid `ExtendedPstatePointer` whose active union field
    ///   matches the architectural mode selected by `(*restricted_state).cpsr`.
    unsafe fn call_restricted_enter_loop_and_save_pstate(
        seed_pstate_ptr: *mut ExtendedPstatePointer,
        restricted_state: *mut zx::sys::zx_restricted_state_t,
        in_pstate_ptr: *mut ExtendedPstatePointer,
        out_pstate_ptr: *mut ExtendedPstatePointer,
    ) -> zx::sys::zx_status_t {
        let status: zx::sys::zx_status_t;
        // SAFETY:
        // * `restore_extended_pstate`, `restricted_enter_loop`, and `save_extended_pstate` are
        //   invoked back-to-back in a single assembly block so compiler-generated code cannot
        //   clobber volatile SIMD/SVE registers between `restricted_enter_loop` and
        //   `save_extended_pstate`.
        // * `x20`..`x24` are callee-saved in AAPCS64 and preserved across all three calls.
        // * All caller-saved registers (`clobber_abi("C")`) and callee-saved SIMD registers
        //   (`v8`..`v15`) are declared clobbered, and `fpcr`/`fpsr` are reset to zero before
        //   returning.
        unsafe {
            asm!(
                "mov x0, x20",
                "bl {restore_fn}",
                "mov w0, #-1",
                "mov x1, xzr",
                "mov x2, x21",
                "mov x3, x22",
                "bl {enter_loop_fn}",
                "mov w24, w0",
                "mov x0, x23",
                "bl {save_fn}",
                "msr fpcr, xzr",
                "msr fpsr, xzr",
                restore_fn = sym restore_extended_pstate,
                enter_loop_fn = sym restricted_enter_loop,
                save_fn = sym save_extended_pstate,
                in("x20") seed_pstate_ptr,
                in("x21") restricted_state,
                in("x22") in_pstate_ptr,
                in("x23") out_pstate_ptr,
                out("w24") status,
                clobber_abi("C"),
                out("v8") _, out("v9") _, out("v10") _, out("v11") _,
                out("v12") _, out("v13") _, out("v14") _, out("v15") _,
            );
        }
        status
    }

    #[test]
    fn test_restricted_enter_loop_restores_aarch64_and_aarch32_pstate() {
        assert_eq!(std::mem::offset_of!(zx::sys::zx_restricted_state_t, cpsr), 34 * 8);

        let mut qregs64 = [0u128; 32];
        for (i, q) in qregs64.iter_mut().enumerate() {
            *q = ((i as u128 + 1) << 64) | (0x0102_0304_0506_0708_u128 + i as u128);
        }
        // Architecturally defined RW bits: FZ (bit 24) in FPCR; IOC | OFC | IXC (0x15) in FPSR.
        let fpcr64: u32 = 1 << 24;
        let fpsr64: u32 = 0x15;
        let mut pregs64 = [0u16; 16];
        for (i, p) in pregs64.iter_mut().enumerate() {
            *p = 0x5a00 | (i as u16 + 1);
        }
        let ffr64: u16 = 0x00ff;

        let mut state64 = ExtendedPstateState::default();
        state64.set_arm64_state(&qregs64, fpsr64, fpcr64);
        if state64.has_arm64_sve() {
            state64.set_arm64_sve_state(&pregs64, ffr64);
        }

        let mut zero_seed = ExtendedPstateState::default();
        let mut zero_seed_ptr = ExtendedPstatePointer { extended_pstate: &raw mut zero_seed };
        let mut in_pstate64_ptr = ExtendedPstatePointer { extended_pstate: &raw mut state64 };
        let mut saved_after_64 = ExtendedPstateState::default();
        let mut saved_after_64_ptr =
            ExtendedPstatePointer { extended_pstate: &raw mut saved_after_64 };

        let mut restricted_state = zx::sys::zx_restricted_state_t::default();
        restricted_state.cpsr = 0;

        // SAFETY: All pointers reference valid stack-allocated state and `cpsr == 0` selects the
        // 64-bit `ExtendedPstateState` union arm.
        let status = unsafe {
            call_restricted_enter_loop_and_save_pstate(
                &raw mut zero_seed_ptr,
                &raw mut restricted_state,
                &raw mut in_pstate64_ptr,
                &raw mut saved_after_64_ptr,
            )
        };
        assert_eq!(status, zx::sys::ZX_ERR_INVALID_ARGS);
        assert_eq!(saved_after_64.get_arm64_qregs(), &qregs64);
        assert_eq!(saved_after_64.get_arm64_fpcr(), fpcr64);
        assert_eq!(saved_after_64.get_arm64_fpsr(), fpsr64);
        if state64.has_arm64_sve() {
            assert_eq!(saved_after_64.get_arm64_p_regs(), &pregs64);
            assert_eq!(saved_after_64.get_arm64_ffr(), ffr64);
        }

        // Allocate a 2-page VMAR where only the first page is mapped and the second page is an
        // unmapped guard page, placing `ExtendedAarch32PstateState` at the very end of the first
        // page (`page_end - size_of::<ExtendedAarch32PstateState>()`). If `.restricted_enter_loop_top`
        // mistakenly called the 64-bit `restore_extended_pstate` when `CPSR.M[4]` (`0x10`) is set,
        // it would read 283 bytes into the unmapped guard page and fault.
        // SAFETY: `zx_vmar_root_self()` returns the process's valid root VMAR handle.
        let root_vmar = unsafe { zx::Unowned::<zx::Vmar>::from_raw_handle(zx_vmar_root_self()) };
        let page_size = zx::system_get_page_size() as usize;
        let (sub_vmar, _sub_vmar_base) = root_vmar
            .allocate(
                0,
                2 * page_size,
                zx::VmarFlags::CAN_MAP_READ
                    | zx::VmarFlags::CAN_MAP_WRITE
                    | zx::VmarFlags::CAN_MAP_SPECIFIC,
            )
            .expect("allocate 2-page VMAR");
        let vmo = zx::Vmo::create(page_size as u64).expect("create 1-page VMO");
        let page_start = sub_vmar
            .map(
                0,
                &vmo,
                0,
                page_size,
                zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE | zx::VmarFlags::SPECIFIC,
            )
            .expect("map first page");
        let page_end = page_start + page_size;
        let state32_addr = page_end - std::mem::size_of::<ExtendedAarch32PstateState>();
        let state32_ptr: *mut ExtendedAarch32PstateState =
            std::ptr::without_provenance_mut(state32_addr);
        assert!(state32_ptr.is_aligned());

        let mut qregs32 = [0u128; 16];
        for (i, q) in qregs32.iter_mut().enumerate() {
            *q = 0xfedc_ba98_7654_3210_u128 ^ ((i as u128 + 1) * 0x1111_2222_3333_4444_u128);
        }
        // Architecturally defined RW bits: DN | RMode=RP in FPCR; DZC | UFC (0x0a) in FPSR.
        let fpcr32: u32 = (1 << 25) | (1 << 22);
        let fpsr32: u32 = 0x0a;

        let mut init32_from_64 = ExtendedPstateState::with_sve(false);
        let mut init32_qregs = [0u128; 32];
        init32_qregs[..16].copy_from_slice(&qregs32);
        init32_from_64.set_arm64_state(&init32_qregs, fpsr32, fpcr32);
        let mut init32_from_64_ptr =
            ExtendedPstatePointer { extended_pstate: &raw mut init32_from_64 };
        let mut in_pstate32_ptr = ExtendedPstatePointer { extended_aarch32_pstate: state32_ptr };

        // SAFETY: `state32_ptr` lies entirely within the mapped first page of `sub_vmar` and is
        // 16-byte aligned.
        unsafe {
            state32_ptr.write(ExtendedAarch32PstateState::default());
            asm!(
                "mov x0, x20",
                "bl {restore_fn}",
                "mov x0, x21",
                "bl {save_aarch32_fn}",
                "msr fpcr, xzr",
                "msr fpsr, xzr",
                restore_fn = sym restore_extended_pstate,
                save_aarch32_fn = sym save_extended_aarch32_pstate,
                in("x20") &raw mut init32_from_64_ptr,
                in("x21") &raw mut in_pstate32_ptr,
                clobber_abi("C"),
                out("v8") _, out("v9") _, out("v10") _, out("v11") _,
                out("v12") _, out("v13") _, out("v14") _, out("v15") _,
            );
            let state32_ref = &*state32_ptr;
            assert_eq!(state32_ref.get_arm32_qregs(), &qregs32);
            assert_eq!(state32_ref.get_arm32_fpcr(), fpcr32);
            assert_eq!(state32_ref.get_arm32_fpsr(), fpsr32);
        }

        restricted_state.cpsr = zx::sys::ZX_REG_CPSR_ARCH_32_MASK as u32;
        let mut saved_after_32 = ExtendedPstateState::default();
        let mut saved_after_32_ptr =
            ExtendedPstatePointer { extended_pstate: &raw mut saved_after_32 };

        // Seed `q0..q31` with `state64` immediately before invoking `restricted_enter_loop` in
        // AArch32 mode so we can verify that `restore_extended_aarch32_pstate` updates `q0..q15`,
        // `fpcr`, and `fpsr` while leaving `q16..q31` untouched and without faulting on the guard
        // page immediately after `state32_ptr`.
        // SAFETY: `in_pstate32_ptr` points to the valid `ExtendedAarch32PstateState` at the end of
        // the mapped page, matching `restricted_state.cpsr`'s `ZX_REG_CPSR_ARCH_32_MASK` bit.
        let status = unsafe {
            call_restricted_enter_loop_and_save_pstate(
                &raw mut in_pstate64_ptr,
                &raw mut restricted_state,
                &raw mut in_pstate32_ptr,
                &raw mut saved_after_32_ptr,
            )
        };
        assert_eq!(status, zx::sys::ZX_ERR_INVALID_ARGS);
        assert_eq!(&saved_after_32.get_arm64_qregs()[..16], &qregs32);
        assert_eq!(&saved_after_32.get_arm64_qregs()[16..], &qregs64[16..]);
        assert_eq!(saved_after_32.get_arm64_fpcr(), fpcr32);
        assert_eq!(saved_after_32.get_arm64_fpsr(), fpsr32);

        // SAFETY: `state32_ptr` is not accessed after destroying `sub_vmar`.
        unsafe {
            sub_vmar.destroy().expect("destroy 2-page VMAR");
        }
    }
}
