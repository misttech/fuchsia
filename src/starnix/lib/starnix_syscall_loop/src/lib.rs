// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, format_err};
use extended_pstate::ExtendedPstateState;
use starnix_core::arch::execution::new_syscall;
use starnix_core::signals::{
    SignalInfo, deliver_signal, dequeue_signal, prepare_to_restart_syscall,
};
use starnix_core::task::{
    CurrentTask, ExceptionResult, ExitStatus, SeccompStateValue, StopState, TaskFlags,
    ptrace_syscall_enter, ptrace_syscall_exit,
};
use starnix_logging::{
    CATEGORY_STARNIX, NAME_HANDLE_EXCEPTION, NAME_READ_RESTRICTED_STATE, NAME_RESTRICTED_KICK,
    NAME_RUN_TASK, NAME_WRITE_RESTRICTED_STATE, firehose_trace_duration, firehose_trace_instant,
    log_error, log_trace, log_warn, set_current_task_info,
};
use starnix_sync::{Locked, Unlocked};
use starnix_syscalls::SyscallResult;
use starnix_syscalls::decls::{Syscall, SyscallDecl};
use starnix_uapi::errno;
use starnix_uapi::errors::Errno;
use starnix_uapi::signals::SIGKILL;
use std::ptr::NonNull;

mod table;

pub fn enter(locked: &mut Locked<Unlocked>, current_task: &mut CurrentTask) -> ExitStatus {
    // Allocate a VMO and bind it to this thread.
    let mut out_vmo_handle = 0;
    #[allow(
        clippy::undocumented_unsafe_blocks,
        reason = "Force documented unsafe blocks in Starnix"
    )]
    let status =
        zx::Status::from_raw(unsafe { zx::sys::zx_restricted_bind_state(0, &mut out_vmo_handle) });
    match { status } {
        zx::Status::OK => {
            // We've successfully attached the VMO to the current thread. This VMO will be
            // mapped and used for the kernel to store restricted mode register state as it
            // enters and exits restricted mode.
        }
        _ => panic!("zx_restricted_bind_state failed with {status}!"),
    }
    #[allow(
        clippy::undocumented_unsafe_blocks,
        reason = "Force documented unsafe blocks in Starnix"
    )]
    let state_vmo = unsafe { zx::Vmo::from(zx::Handle::from_raw(out_vmo_handle)) };

    // Unbind when we leave this scope to avoid unnecessarily retaining the VMO via this
    // thread's binding.  Of course, we'll still have to remove any mappings and close any
    // handles that refer to the VMO to ensure it will be destroyed.  See note about
    // preventing resource leaks in this function's documentation.
    scopeguard::defer! {
            #[allow(
                clippy::undocumented_unsafe_blocks,
                reason = "Force documented unsafe blocks in Starnix"
            )]
        unsafe { zx::sys::zx_restricted_unbind_state(0); }
    }

    // Map the restricted state VMO and arrange for it to be unmapped later.
    // SAFETY: `state_vmo` is a VMO produced by `zx_restricted_bind_state`.
    match unsafe { RestrictedState::from_vmo(state_vmo) } {
        Ok(restricted_state) => match run_task(locked, current_task, restricted_state) {
            Ok(ok) => ok,
            Err(error) => {
                log_warn!("Died unexpectedly from {error:?}! treating as SIGKILL");
                ExitStatus::Kill(SignalInfo::default(SIGKILL))
            }
        },
        Err(error) => {
            log_error!("failed to map mode state vmo, {error:?}! treating as SIGKILL");
            ExitStatus::Kill(SignalInfo::default(SIGKILL))
        }
    }
}

extern "C" {
    // rustc doesn't like RestrictedEnterContext for FFI but we're just passing it back to
    // ourselves with extra steps.
    #[allow(improper_ctypes)]
    fn restricted_enter_loop(
        options: u32,
        restricted_exit_callback: extern "C" fn(*mut RestrictedEnterContext<'_>, u64) -> bool,
        restricted_exit_callback_context: *mut RestrictedEnterContext<'_>,
        restricted_state: *mut zx::sys::zx_restricted_exception_t,
        extended_pstate: *const ExtendedPstateState,
    ) -> zx::sys::zx_status_t;
}

/// `RestrictedState` manages accesses into the restricted state VMO.
///
/// See `zx_restricted_bind_state`.
pub struct RestrictedState {
    bound_state: NonNull<zx::sys::zx_restricted_exception_t>,
    state_size: usize,
}

impl RestrictedState {
    /// Wrap a restricted state VMO for use by the rest of Starnix.
    ///
    /// # Safety
    ///
    /// `state_vmo` must be produced from `zx_restricted_bind_state()`.
    pub unsafe fn from_vmo(state_vmo: zx::Vmo) -> Result<Self, zx::Status> {
        let state_size = state_vmo.get_size()? as usize;
        if state_size < std::mem::size_of::<zx::sys::zx_restricted_exception_t>() {
            return Err(zx::Status::INVALID_ARGS);
        }

        let state_address = fuchsia_runtime::vmar_root_self().map(
            0,
            &state_vmo,
            0,
            state_size,
            zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
        )?;

        // This memory is not managed by Rust's stack, heap, etc. so treat it as "foreign" memory
        // with no provenance.
        let state_address: *mut zx::sys::zx_restricted_exception_t =
            std::ptr::without_provenance_mut(state_address);
        assert!(state_address.is_aligned(), "Zircon must map restricted-state-aligned memory");
        let bound_state =
            NonNull::new(state_address).expect("Zircon must map non-null restricted-state");

        Ok(Self { state_size, bound_state })
    }

    pub fn write_state(&mut self, state: &zx::sys::zx_restricted_state_t) {
        firehose_trace_duration!(CATEGORY_STARNIX, NAME_WRITE_RESTRICTED_STATE);

        // SAFETY: `bound_state` is valid to write to as long as `RestrictedState` is live.
        unsafe {
            let state_ptr = std::ptr::addr_of_mut!((*self.bound_state.as_ptr()).state);
            state_ptr.write(*state);
        }
    }

    pub fn read_state(&self, state: &mut zx::sys::zx_restricted_state_t) {
        firehose_trace_duration!(CATEGORY_STARNIX, NAME_READ_RESTRICTED_STATE);

        // SAFETY: `bound_state` is valid to read from as long as `RestrictedState` is live.
        unsafe {
            let state_ptr = std::ptr::addr_of!((*self.bound_state.as_ptr()).state);
            *state = state_ptr.read();
        }
    }

    pub fn read_exception(&self) -> zx::ExceptionReport {
        // SAFETY: `bound_state` is valid to read from as long as `RestrictedState` is live.
        let raw = unsafe { self.bound_state.read() };

        // SAFETY: `raw` was written by Zircon during a restricted exit.
        unsafe { zx::ExceptionReport::from_raw(raw.exception) }
    }
}

impl std::ops::Drop for RestrictedState {
    fn drop(&mut self) {
        let mapping_addr = self.bound_state.as_ptr() as usize;
        // Safety: We are un-mapping the state VMO. This is safe because we route all access
        // into this memory region though this struct so it is safe to unmap on Drop.
        unsafe {
            fuchsia_runtime::vmar_root_self()
                .unmap(mapping_addr, self.state_size)
                .expect("Failed to unmap");
        }
    }
}

const RESTRICTED_ENTER_OPTIONS: u32 = 0;

struct RestrictedEnterContext<'a> {
    current_task: &'a mut CurrentTask,
    restricted_state: RestrictedState,
    state: zx::sys::zx_restricted_state_t,
    error_context: Option<ErrorContext>,
    exit_status: Result<ExitStatus, Error>,
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
    locked: &mut Locked<Unlocked>,
    current_task: &mut CurrentTask,
    mut restricted_state: RestrictedState,
) -> Result<ExitStatus, Error> {
    set_current_task_info(
        current_task.task.command(),
        current_task.task.thread_group().read().leader_command(),
        current_task.task.thread_group().leader,
        current_task.tid,
    );

    firehose_trace_duration!(CATEGORY_STARNIX, NAME_RUN_TASK);

    // This tracks the last failing system call for debugging purposes.
    let error_context = None;

    // We need to check for exit once, before the task starts executing, in case
    // the task has already been sent a signal that will cause it to exit.
    if let Some(exit_status) =
        process_completed_restricted_exit(locked, current_task, &error_context)?
    {
        return Ok(exit_status);
    }

    let state = zx::sys::zx_restricted_state_t::from(&*current_task.thread_state.registers);
    // Copy the initial register state into the mapped VMO.
    restricted_state.write_state(&state);

    let restricted_state_ptr = restricted_state.bound_state.as_ptr();
    let extended_pstate_ptr = &current_task.thread_state.extended_pstate as *const _;

    let mut restricted_enter_context = RestrictedEnterContext {
        current_task,
        restricted_state,
        state,
        error_context,
        exit_status: Err(errno!(ENOEXEC).into()),
    };

    #[allow(
        clippy::undocumented_unsafe_blocks,
        reason = "Force documented unsafe blocks in Starnix"
    )]
    unsafe {
        restricted_enter_loop(
            RESTRICTED_ENTER_OPTIONS,
            restricted_exit_callback_c,
            &mut restricted_enter_context,
            restricted_state_ptr,
            extended_pstate_ptr,
        );
    }
    restricted_enter_context.exit_status
}

extern "C" fn restricted_exit_callback_c(
    context: *mut RestrictedEnterContext<'_>,
    reason_code: zx::sys::zx_restricted_reason_t,
) -> bool {
    // SAFETY: `context` is a pointer to a `RestrictedEnterContext` that was passed to
    // `restricted_enter_loop`. Our restricted return assembly and Zircon together guarantee that
    // this thread has exclusive access to the restricted enter context.
    let restricted_context = unsafe { &mut *context };
    restricted_exit_callback(
        reason_code,
        restricted_context.current_task,
        &mut restricted_context.restricted_state,
        &mut restricted_context.state,
        &mut restricted_context.error_context,
        &mut restricted_context.exit_status,
    )
}

fn restricted_exit_callback(
    reason_code: zx::sys::zx_restricted_reason_t,
    current_task: &mut CurrentTask,
    restricted_state: &mut RestrictedState,
    state: &mut zx::sys::zx_restricted_state_t,
    error_context: &mut Option<ErrorContext>,
    exit_status: &mut Result<ExitStatus, Error>,
) -> bool {
    debug_assert_eq!(
        current_task.thread_state.restart_code, None,
        "restart_code should only ever be Some() in normal mode",
    );

    let ret = match process_restricted_exit(
        reason_code,
        current_task,
        restricted_state,
        state,
        error_context,
    ) {
        Ok(None) => {
            // Keep going!
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
    restricted_state: &mut RestrictedState,
    state: &mut zx::sys::zx_restricted_state_t,
    error_context: &mut Option<ErrorContext>,
) -> Result<Option<ExitStatus>, Error> {
    // We can't hold any locks entering restricted mode so we can't be holding any locks on exit.
    #[allow(
        clippy::undocumented_unsafe_blocks,
        reason = "Force documented unsafe blocks in Starnix"
    )]
    let locked = unsafe { Unlocked::new() };

    // Copy the register state out of the VMO.
    restricted_state.read_state(state);

    // Store the new register state in the current task before dispatching the exit.
    current_task.thread_state.registers =
        zx::sys::zx_thread_state_general_regs_t::from(&*state).into();

    match reason_code {
        zx::sys::ZX_RESTRICTED_REASON_SYSCALL => {
            let syscall_decl = SyscallDecl::from_number(
                current_task.thread_state.registers.syscall_register(),
                current_task.thread_state.arch_width,
            );

            if let Some(new_error_context) = execute_syscall(locked, current_task, syscall_decl) {
                *error_context = Some(new_error_context);
            }
        }
        zx::sys::ZX_RESTRICTED_REASON_EXCEPTION => {
            firehose_trace_duration!(CATEGORY_STARNIX, NAME_HANDLE_EXCEPTION);
            let restricted_exception = restricted_state.read_exception();
            let exception_result = current_task.process_exception(locked, &restricted_exception);
            process_completed_exception(
                locked,
                current_task,
                exception_result,
                restricted_exception,
            );
        }
        zx::sys::ZX_RESTRICTED_REASON_KICK => {
            firehose_trace_instant!(
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
    if let Some(exit_status) =
        process_completed_restricted_exit(locked, current_task, &error_context)?
    {
        return Ok(Some(exit_status));
    }

    // Copy the updated register state into the mapped VMO.
    let state = zx::sys::zx_restricted_state_t::from(&*current_task.thread_state.registers);
    restricted_state.write_state(&state);

    Ok(None)
}

fn process_completed_exception(
    locked: &mut Locked<Unlocked>,
    current_task: &mut CurrentTask,
    exception_result: ExceptionResult,
    restricted_exception: zx::ExceptionReport,
) {
    match exception_result {
        ExceptionResult::Handled => {}
        ExceptionResult::Signal(signal) => {
            // TODO: Verify that the rip is actually in restricted code.
            let mut registers = current_task.thread_state.registers;
            {
                let mut task_state = current_task.task.write();
                if task_state.ptrace_on_signal_consume() {
                    task_state.set_stopped(
                        StopState::SignalDeliveryStopping,
                        Some(signal),
                        Some(&current_task),
                        None,
                    );
                    return;
                }

                if let Some(status) = deliver_signal(
                    current_task,
                    current_task.thread_state.arch_width,
                    task_state,
                    signal.into(),
                    &mut registers,
                    &current_task.thread_state.extended_pstate,
                    Some(restricted_exception),
                ) {
                    current_task.thread_group_exit(locked, status);
                }
            }
            current_task.thread_state.registers = registers;
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

/// Executes the provided `syscall` in `current_task`.
///
/// Returns an `ErrorContext` if the system call returned an error.
#[inline(never)] // Inlining this function breaks the CFI directives used to unwind into user code.
pub fn execute_syscall(
    locked: &mut Locked<Unlocked>,
    current_task: &mut CurrentTask,
    syscall_decl: SyscallDecl,
) -> Option<ErrorContext> {
    firehose_trace_duration!(CATEGORY_STARNIX, syscall_decl.trace_name());
    let syscall = new_syscall(syscall_decl, current_task);

    current_task.thread_state.registers.save_registers_for_restart(syscall.decl.number);

    if current_task.trace_syscalls.load(std::sync::atomic::Ordering::Relaxed) {
        ptrace_syscall_enter(locked, current_task);
    }

    log_trace!("{:?}", syscall);

    let result: Result<SyscallResult, Errno> =
        if current_task.seccomp_filter_state.get() != SeccompStateValue::None {
            // Inlined fast path for seccomp, so that we don't incur the cost
            // of a method call when running the filters.
            if let Some(res) = current_task.run_seccomp_filters(locked, &syscall) {
                res
            } else {
                table::dispatch_syscall(locked, current_task, &syscall)
            }
        } else {
            table::dispatch_syscall(locked, current_task, &syscall)
        };

    current_task.trigger_delayed_releaser(locked);

    let return_value = match result {
        Ok(return_value) => {
            log_trace!("-> {:#x}", return_value.value());
            current_task.thread_state.registers.set_return_register(return_value.value());
            None
        }
        Err(errno) => {
            log_trace!("!-> {}", errno);
            if errno.is_restartable() {
                current_task.thread_state.restart_code = Some(errno.code);
            }
            current_task.thread_state.registers.set_return_register(errno.return_value());
            Some(ErrorContext { error: errno, syscall })
        }
    };

    if current_task.trace_syscalls.load(std::sync::atomic::Ordering::Relaxed) {
        ptrace_syscall_exit(locked, current_task, return_value.is_some());
    }

    return_value
}

/// Finishes `current_task` updates after a restricted mode exit such as a syscall, exception, or kick.
///
/// Returns an `ExitStatus` if the task is meant to exit.
pub fn process_completed_restricted_exit(
    locked: &mut Locked<Unlocked>,
    current_task: &mut CurrentTask,
    error_context: &Option<ErrorContext>,
) -> Result<Option<ExitStatus>, Errno> {
    let result;
    loop {
        // Checking for a signal might cause the task to exit, so check before processing exit
        {
            {
                if !current_task.is_exitted() {
                    dequeue_signal(locked, current_task);
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
            current_task.block_while_stopped(locked);
            // If ptrace_cont has sent a signal, process it immediately.  This
            // seems to match Linux behavior.

            let task_state = current_task.read();
            if task_state.ptrace.as_ref().is_some_and(|ptrace| {
                ptrace.stop_status == starnix_core::task::PtraceStatus::Continuing
            }) && task_state.is_any_signal_pending()
                && !current_task.is_exitted()
            {
                continue;
            }
            result = None;
            break;
        }
    }

    if let Some(exit_status) = &result {
        if current_task.flags().contains(TaskFlags::DUMP_ON_EXIT) {
            if let Some(pending_report) =
                current_task.kernel().crash_reporter.begin_crash_report(&current_task)
            {
                // Request a backtrace before reporting the crash to increase chance of a backtrace
                // in logs. This call is kept as far up in the call stack as possible to avoid
                // additional frames that are always the same and not relevant to users.
                // TODO(https://fxbug.dev/356732164) collect a backtrace ourselves
                debug::backtrace_request_current_thread();
                current_task.kernel().crash_reporter.handle_core_dump(
                    &current_task,
                    exit_status,
                    pending_report,
                );
            }
        }
    }
    return Ok(result);
}
