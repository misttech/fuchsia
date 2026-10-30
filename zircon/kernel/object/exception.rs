// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::dispatcher::Dispatcher;
use super::exception_dispatcher::ExceptionDispatcher;
use super::job_dispatcher::JobDispatcher;
use super::thread_dispatcher::{AutoBlocked, Blocked, ThreadDispatcher};
use crate::arch_rs::{
    ArchExceptionContext, arch_dump_exception_context, arch_exception_context_user_synth_code,
    arch_exception_context_user_synth_data,
};
use crate::kernel::thread::with_exception_context;
use core::ptr::NonNull;
use debug::ltracef;
use fbl::RefPtr;
use kprint::kprintln;
use zx_status::Status;
use zx_types::{
    ZX_ERR_INTERNAL_INTR_KILLED, ZX_EXCP_FATAL_PAGE_FAULT, ZX_EXCP_GENERAL, ZX_EXCP_HW_BREAKPOINT,
    ZX_EXCP_POLICY_ERROR, ZX_EXCP_PROCESS_STARTING, ZX_EXCP_SW_BREAKPOINT, ZX_EXCP_THREAD_EXITING,
    ZX_EXCP_THREAD_STARTING, ZX_EXCP_UNALIGNED_ACCESS, ZX_EXCP_UNDEFINED_INSTRUCTION,
    ZX_MAX_NAME_LEN, ZX_TASK_RETCODE_EXCEPTION_KILL, zx_status_t,
};

const LOCAL_TRACE: u32 = 0;

fn excp_type_to_string(type_: u32) -> &'static str {
    match type_ {
        ZX_EXCP_FATAL_PAGE_FAULT => "fatal page fault",
        ZX_EXCP_UNDEFINED_INSTRUCTION => "undefined instruction",
        ZX_EXCP_GENERAL => "general fault",
        ZX_EXCP_SW_BREAKPOINT => "software breakpoint",
        ZX_EXCP_HW_BREAKPOINT => "hardware breakpoint",
        ZX_EXCP_UNALIGNED_ACCESS => "alignment fault",
        ZX_EXCP_POLICY_ERROR => "policy error",
        ZX_EXCP_PROCESS_STARTING => "process starting",
        ZX_EXCP_THREAD_STARTING => "thread starting",
        ZX_EXCP_THREAD_EXITING => "thread exiting",
        _ => "unknown fault",
    }
}

#[inline]
fn has_restricted_in_thread_handler() -> bool {
    // SAFETY: `current_get()` returns the non-null `Thread*` of the currently executing thread.
    unsafe { crate::kernel::thread::in_restricted(crate::kernel::thread::current_get()) }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExceptionDeliveryMethod {
    FirstChanceDebugChannel,
    RestrictedModeVectoredException,
    ThreadChannel,
    ProcessChannel,
    SecondChanceDebugChannel,
    JobChannel,
}

/// Walks the ordered chain of exception handlers for `thread`.
///
/// This isn't an "iterator" in the pure Rust sense. We don't need all that
/// complexity. I just couldn't think of a better term.
///
/// Exception handlers are tried in the following order:
/// - debugger
/// - thread (or restricted mode in-thread vectored handler)
/// - process
/// - debugger (in dealing with a second-chance exception)
/// - job (first owning job, then its parent job, and so on up to root job)
struct ExceptionHandlerIterator<'a> {
    thread: &'a ThreadDispatcher,
    exception: RefPtr<ExceptionDispatcher>,
    next_method: ExceptionDeliveryMethod,
    next_job: Option<RefPtr<JobDispatcher>>,
}

impl<'a> ExceptionHandlerIterator<'a> {
    fn new(thread: &'a ThreadDispatcher, exception: RefPtr<ExceptionDispatcher>) -> Self {
        Self {
            thread,
            exception,
            next_method: ExceptionDeliveryMethod::FirstChanceDebugChannel,
            next_job: None,
        }
    }

    /// Sends the exception to the next registered handler, starting with
    /// `ZX_EXCEPTION_CHANNEL_TYPE_DEBUGGER` (the process debug channel) on the first call.
    ///
    /// Returns `true` and fills `result` if the exception was sent to a handler, or returns
    /// `false` if there are no more to try (retaining the last non-`NEXT` delivery error in
    /// `result` if sending to a valid channel failed). Do not call this function again after it
    /// returns `false`.
    fn next(&mut self, result: &mut Result<(), Status>) -> bool {
        loop {
            let sent;

            match self.next_method {
                ExceptionDeliveryMethod::FirstChanceDebugChannel => {
                    (*result, sent) = self.thread.handle_exception(
                        self.thread.process().debug_exceptionate(),
                        &self.exception,
                    );
                    if has_restricted_in_thread_handler() {
                        self.next_method = ExceptionDeliveryMethod::RestrictedModeVectoredException;
                    } else {
                        self.next_method = ExceptionDeliveryMethod::ThreadChannel;
                    }
                }
                ExceptionDeliveryMethod::SecondChanceDebugChannel => {
                    (*result, sent) = self.thread.handle_exception(
                        self.thread.process().debug_exceptionate(),
                        &self.exception,
                    );
                    self.next_method = ExceptionDeliveryMethod::JobChannel;
                    self.next_job = self.thread.process().job();
                }
                ExceptionDeliveryMethod::RestrictedModeVectoredException => {
                    let rs_ptr = crate::kernel::thread::current_restricted_state();
                    let mut rs_ptr = NonNull::new(rs_ptr)
                        .expect("restricted exception without bound RestrictedState");
                    // SAFETY: `rs_ptr` is owned exclusively by the current thread.
                    let rs = unsafe { rs_ptr.as_mut() };

                    let report =
                        self.exception.fill_report().expect("failed to fill exception report");
                    // SAFETY: `has_restricted_in_thread_handler()` verified that the current user
                    // thread is in restricted mode, `rs` is its bound `RestrictedState`, and
                    // `dispatch_user_exception` installed the thread's exception context for the
                    // duration of `exception_handler_worker`.
                    unsafe {
                        crate::kernel::restricted::redirect_restricted_exception_to_normal_mode(
                            rs, &report,
                        );
                    }

                    // Handle the exception on behalf of restricted mode.
                    //
                    // Note this implies that restricted exceptions can never propagate
                    // further and as such are ineligible for subsequent handlers.
                    *result = Ok(());
                    return true;
                }
                ExceptionDeliveryMethod::ThreadChannel => {
                    (*result, sent) =
                        self.thread.handle_exception(self.thread.exceptionate(), &self.exception);
                    self.next_method = ExceptionDeliveryMethod::ProcessChannel;
                }
                ExceptionDeliveryMethod::ProcessChannel => {
                    (*result, sent) = self
                        .thread
                        .handle_exception(self.thread.process().exceptionate(), &self.exception);

                    // This state may change during the handling of the debugger exception
                    // channel. Accordingly, we check its value dynamically rather than caching it
                    // when the iterator is constructed.
                    if self.exception.is_second_chance() {
                        self.next_method = ExceptionDeliveryMethod::SecondChanceDebugChannel;
                    } else {
                        self.next_method = ExceptionDeliveryMethod::JobChannel;
                        self.next_job = self.thread.process().job();
                    }
                }
                ExceptionDeliveryMethod::JobChannel => {
                    let Some(job) = self.next_job.take() else {
                        // Reached the root job and there was no handler.
                        return false;
                    };
                    (*result, sent) =
                        self.thread.handle_exception(job.exceptionate(), &self.exception);
                    self.next_job = job.parent();
                }
            }

            // Return to the caller once a handler was activated.
            if sent {
                return true;
            }
        }
    }
}

/// Subroutine of [`dispatch_user_exception`] to simplify the code.
/// One useful thing this does is guarantee [`ExceptionHandlerIterator`] is properly destructed.
///
/// Returns `(result, processed)`, where `processed` indicates if at least one handler processed
/// the exception, and `result` is:
/// * `Ok(())` if the thread has been resumed.
/// * `Err(Status::NEXT)` if we ran out of handlers before the thread resumed.
/// * `Err(Status(ZX_ERR_INTERNAL_INTR_KILLED))` if the thread was killed.
/// * `Err(Status::NO_MEMORY)` on allocation failure
///   (TODO(https://fxbug.dev/42108776): remove this case).
///
/// # Safety
///
/// `context` must point to a valid, initialized `arch_exception_context_t` for the current thread.
unsafe fn exception_handler_worker(
    exception_type: u32,
    context: *const ArchExceptionContext,
    thread: &ThreadDispatcher,
) -> (Result<(), Status>, bool) {
    debug_assert!(!context.is_null());

    let mut processed = false;

    // SAFETY: Caller guarantees `context` points to a valid `arch_exception_context_t`.
    let report = unsafe { ExceptionDispatcher::build_arch_report(exception_type, context) };

    // SAFETY: `report` and `context` outlive `_exception_cleaner` below, which calls
    // `exception.clear()` on all exit paths before `report` goes out of scope.
    let Some(exception) = (unsafe {
        ExceptionDispatcher::create(RefPtr::from_ref(thread), exception_type, &report, context)
    }) else {
        core::hint::cold_path();
        // No memory to create the exception, we just have to drop it which
        // will kill the process.
        kprintln!(
            "KERN: failed to allocate memory for {:s} exception in user thread {:u}.{:u}",
            excp_type_to_string(exception_type),
            thread.process().get_koid(),
            thread.get_koid(),
        );
        return (Err(Status::NO_MEMORY), processed);
    };

    // Most of the time we'll be holding the last reference to the exception
    // when this function exits, but if the task is killed we return HS_KILLED
    // without waiting for the handler which means someone may still have a
    // handle to the exception.
    //
    // For simplicity and to catch any unhandled status cases below, just clean
    // out the exception before returning no matter what.
    let _exception_cleaner = zr::defer(|| exception.clear());

    let mut iter = ExceptionHandlerIterator::new(thread, exception.clone());
    let mut result: Result<(), Status> = Err(Status::NEXT);
    while iter.next(&mut result) {
        ltracef!("handler returned {}\n", Status::result_into_raw(result));

        // ZX_ERR_NEXT means the handler wants to pass it up to the next in the
        // chain, keep looping but mark that at least one handler saw the exception.
        if result == Err(Status::NEXT) {
            processed = true;
            continue;
        }

        // ZX_ERR_STOP means the handler wants to kill the thread.
        if result == Err(Status::STOP) {
            processed = true;
            debug_assert!(thread.is_current());
            ThreadDispatcher::kill_current();
            return (Err(Status::err_from_raw(ZX_ERR_INTERNAL_INTR_KILLED)), processed);
        }

        // Anything other than ZX_ERR_NEXT means we're done.
        return (result, processed);
    }

    if let Err(status) = result
        && status != Status::NEXT
    {
        core::hint::cold_path();
        // If the iterator returned false, but status got changed from ZX_ERR_NEXT, then this means
        // it attempted to send to a handler, but the sending process failed. We still have no way
        // to handle the exception but we can at least make noise in the debuglog.
        kprintln!(
            concat!(
                "KERN: Failed to deliver {:s} exception to exception handler ",
                "in user thread {:u}.{:u} with status: {}",
            ),
            excp_type_to_string(exception_type),
            thread.process().get_koid(),
            thread.get_koid(),
            status.into_raw(),
        );
    }

    // If we got here we ran out of handlers and nobody resumed the thread.
    (Err(Status::NEXT), processed)
}

#[inline]
fn get_task_name(task: &Dispatcher) -> [u8; ZX_MAX_NAME_LEN] {
    let mut name = [0u8; ZX_MAX_NAME_LEN];
    let result = task.get_name(&mut name);
    // get_name cannot fail for a process or for a thread. We are processing an exception on the
    // current thread, so its state should remain RUNNING until we've decided to kill the thread
    // below.
    debug_assert!(result.is_ok());
    name
}

#[inline]
fn c_str_bytes(buf: &[u8; ZX_MAX_NAME_LEN]) -> &[u8] {
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    &buf[..len]
}

/// Logs the exception type, thread/process names and koids, and architecture-specific register
/// state to the kernel console.
///
/// # Safety
///
/// `arch_context` must point to a valid, initialized `arch_exception_context_t` for the current
/// thread.
unsafe fn dump_exception_context(
    exception_type: u32,
    thread: &ThreadDispatcher,
    pname: &[u8; ZX_MAX_NAME_LEN],
    arch_context: *const ArchExceptionContext,
) {
    let tname = get_task_name(thread);
    kprintln!(
        "KERN: {:s} in user thread '{:s}' ({:u}) in process '{:s}'",
        excp_type_to_string(exception_type),
        c_str_bytes(&tname),
        thread.get_koid(),
        c_str_bytes(pname),
    );
    // SAFETY: Caller guarantees `arch_context` points to a valid `arch_exception_context_t`.
    unsafe {
        arch_dump_exception_context(arch_context);
    }
}

/// Dispatches an exception to the appropriate handler. Called by arch code when it cannot handle
/// an exception.
///
/// If this returns `Ok(())`, the caller is expected to resume the thread "as if" nothing happened;
/// the handler is expected to have modified state such that resumption is possible.
///
/// If this returns `Err(Status::BAD_STATE)`, the current thread is not a user thread (i.e., not
/// associated with a `ThreadDispatcher`).
///
/// Otherwise, we cause the current thread to exit and do not return at all.
///
/// # Safety
///
/// * `arch_context` must point to a valid, initialized `arch_exception_context_t` for the current
///   thread that outlives this call.
/// * If `(*arch_context).frame` is non-null, it must point to a valid, initialized, writable
///   `iframe_t` that outlives this call without conflicting Rust references (since
///   [`with_exception_context`] exposes it to debugger reads and writes).
/// * No thread lock, dispatcher lock, spinlock, or active `ChainLockTransaction` may be held.
// TODO(dje): Support unwinding from this exception and introducing a different exception?
pub unsafe fn dispatch_user_exception(
    exception_type: u32,
    arch_context: *const ArchExceptionContext,
) -> Result<(), Status> {
    ltracef!("type {}, context {:p}\n", exception_type, arch_context);

    let Some(thread) = ThreadDispatcher::try_get_current() else {
        core::hint::cold_path();
        // The current thread is not a user thread; bail.
        return Err(Status::BAD_STATE);
    };

    // From now until the exception is resolved the thread is in an exception.
    let _by = AutoBlocked::new(Blocked::EXCEPTION);

    // We're about to handle the exception. Install `arch_context` on the current thread to make
    // its user register state available to debuggers and exception handlers while the thread is
    // "in exception".
    // SAFETY: `thread` was obtained from `ThreadDispatcher::try_get_current()` above so the
    // current thread is a user thread, and the caller guarantees `arch_context` (and any non-null
    // `(*arch_context).frame`) is valid for the current thread without conflicting references and
    // that no thread lock, dispatcher lock, spinlock, or active `ChainLockTransaction` is held.
    let (result, processed) = unsafe {
        with_exception_context(arch_context, || {
            exception_handler_worker(exception_type, arch_context, &thread)
        })
    };

    let Err(status) = result else {
        return Ok(());
    };

    // If the thread wasn't resumed or explicitly killed, kill the whole process.
    // If the process is critical to the root job, any fatal exception merits
    // logging all the details available whether the handler decided to kill the
    // process, or no handler processed it at all.
    let process = thread.process();
    if status.into_raw() == ZX_ERR_INTERNAL_INTR_KILLED {
        // An exception handler has decided the process should be terminated.
        if process.critical_to_root_job() {
            // Terminating a critical process will likely reboot the system so dump some extra
            // diagnostics to assist in debugging.
            kprintln!("KERN: fatal exception in process critical to root job!");
            let pname = get_task_name(process);
            // SAFETY: Caller guarantees `arch_context` is valid for the current thread.
            unsafe {
                dump_exception_context(exception_type, &thread, &pname, arch_context);
            }
        }
    } else {
        let pname = get_task_name(process);

        if !processed {
            // The exception was not handled. Normally, at least crashsvc would handle the
            // exception and make a smarter decision about what to do with it, but in case it
            // didn't, dump some info to the kernel logs.
            kprintln!("KERN: exception_handler_worker returned {}", status.into_raw());
            // SAFETY: Caller guarantees `arch_context` is valid for the current thread.
            unsafe {
                dump_exception_context(exception_type, &thread, &pname, arch_context);
            }
        }

        // This log message is load-bearing server-side as it's used to identify the
        // critical component responsible for the reboot.
        // Please notify //src/developer/forensics/OWNERS upon changing.
        kprintln!(
            "KERN: terminating process '{:s}' ({:u})",
            c_str_bytes(&pname),
            process.get_koid(),
        );
        process.kill(ZX_TASK_RETCODE_EXCEPTION_KILL);
    }

    // Either the current thread or its whole process was killed, we can now stop
    // it from running.
    ThreadDispatcher::exit_current();
}

/// Prints the common architecture-independent fields in `arch_exception_context`.
///
/// # Safety
///
/// `context` must point to a valid, initialized `arch_exception_context_t`.
pub unsafe fn dump_common_exception_context(context: *const ArchExceptionContext) {
    // SAFETY: Caller guarantees `context` points to a valid `arch_exception_context_t`.
    let (synth_code, synth_data) = unsafe {
        (
            arch_exception_context_user_synth_code(context),
            arch_exception_context_user_synth_data(context),
        )
    };
    // Print the common fields in arch_exception_context.
    //
    // In case of a page fault exception, the error code is typecast from zx_status_t to uint32_t
    // when populating user_synth_code. So also log the value typecast to zx_status_t to make
    // debugging easier.
    kprintln!(
        "synth_code {:u} ({}), synth_data {:u}",
        synth_code,
        synth_code as zx_status_t,
        synth_data,
    );
}
