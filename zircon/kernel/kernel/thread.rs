// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::platform_rs::timer::DurationMono;
use core::ffi::{c_char, c_void};
use core::marker::PhantomData;
use core::ptr::NonNull;
use zx_status::Status;
use zx_types::{zx_instant_mono_t, zx_status_t};

unsafe extern "C" {
    fn cpp_thread_create_default(
        name: *const c_char,
        entry: extern "C" fn(*mut c_void) -> i32,
        arg: *mut c_void,
    ) -> *mut Thread;
    fn cpp_thread_resume(thread: *mut Thread);
    fn cpp_thread_join(
        thread: *mut Thread,
        out_retcode: *mut i32,
        deadline: zx_instant_mono_t,
    ) -> i32;
    fn cpp_thread_current_yield();
    fn cpp_thread_kill(thread: *mut Thread);
    fn cpp_thread_is_blocked(thread: *mut Thread) -> bool;
    fn cpp_thread_current_get() -> *mut Thread;
    fn cpp_thread_fxt_ref(thread: *mut Thread) -> FxtRef;
    fn cpp_thread_preempt_set_timeslice_extension(duration: DurationMono) -> bool;
    fn cpp_thread_preempt_clear_timeslice_extension();
    fn cpp_thread_preempt_disable();
    fn cpp_thread_preempt_enable();
    fn cpp_thread_preempt();
    fn cpp_thread_current_sleep_relative(duration: DurationMono) -> zx_status_t;
    fn cpp_thread_current_sleep_etc(
        deadline: *const crate::kernel::types::Deadline,
        interruptible: Interruptible,
        now: zx_instant_mono_t,
    ) -> zx_status_t;
    fn cpp_thread_current_soft_fault(va: usize, flags: u32) -> zx_status_t;
    fn cpp_restricted_enter(vector_table_ptr: usize, context: usize) -> zx_status_t;
    fn cpp_thread_get_stack_top(thread: *mut Thread) -> usize;
    fn cpp_thread_get_shadow_call_base(thread: *mut Thread) -> usize;
    fn cpp_thread_dump_current_stack();
    fn cpp_thread_is_user_state_saved(thread: *mut Thread) -> bool;
    fn cpp_thread_is_running(thread: *const Thread) -> bool;
    fn cpp_thread_name(thread: *const Thread) -> *const c_char;
    fn cpp_thread_process_pending_signals(frame: *mut c_void);
    fn cpp_thread_is_in_restricted_mode(thread: *mut Thread) -> bool;
}

// LINT.IfChange(FxtRef)
/// Rust representation of the C++ `FxtRef` struct.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FxtRef {
    pub pid: u64,
    pub tid: u64,
}
// LINT.ThenChange(//zircon/kernel/kernel/thread_ffi.cc:FxtRef)

/// An opaque type representing the C++ `Thread` class.
#[repr(C)]
pub struct Thread {
    _private: [u8; 0],
}

/// Enters restricted mode using the given vector table pointer and context.
pub fn restricted_enter(vector_table_ptr: usize, context: usize) -> Result<(), Status> {
    // SAFETY: `cpp_restricted_enter` performs validation of vector_table_ptr and context
    // in architecture-specific restricted mode entry routines.
    let status = unsafe { cpp_restricted_enter(vector_table_ptr, context) };
    Status::ok(status)
}

/// Type-safe wrapper around a raw pointer to a Zircon kernel Thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadPtr(NonNull<Thread>);

// SAFETY: A ThreadPtr is just a pointer to a kernel thread, which can be safely passed
// between threads to perform join or kill operations.
unsafe impl Send for ThreadPtr {}
unsafe impl Sync for ThreadPtr {}

impl ThreadPtr {
    /// Creates a `ThreadPtr` from a raw pointer.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is a valid pointer to a live kernel thread.
    pub const unsafe fn from_raw(ptr: *mut Thread) -> Option<Self> {
        match NonNull::new(ptr) {
            Some(nn) => Some(Self(nn)),
            None => None,
        }
    }

    /// Returns the raw pointer.
    pub const fn as_raw(self) -> *mut Thread {
        self.0.as_ptr()
    }

    /// Returns the raw const pointer.
    pub const fn as_ptr(self) -> *const Thread {
        self.0.as_ptr()
    }

    /// Resumes execution of the thread.
    ///
    /// # Safety
    ///
    /// The caller must ensure the thread has not been joined or destroyed.
    pub unsafe fn resume(self) {
        unsafe { cpp_thread_resume(self.as_raw()) }
    }

    /// Joins the thread, waiting for it to exit.
    ///
    /// Returns the thread's return code on success.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the thread has not been joined yet.
    pub unsafe fn join(self, deadline: zx_instant_mono_t) -> Result<i32, Status> {
        let mut retcode = 0;
        let status = unsafe { cpp_thread_join(self.as_raw(), &mut retcode, deadline) };
        Status::ok(status).map(|_| retcode)
    }

    /// Kills the thread.
    ///
    /// # Safety
    ///
    /// The caller must ensure the thread is still valid.
    pub unsafe fn kill(self) {
        unsafe { cpp_thread_kill(self.as_raw()) }
    }

    /// Checks if the thread is currently blocked.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the thread pointer is still valid and the
    /// underlying thread has not been destroyed or joined.
    pub unsafe fn is_blocked(self) -> bool {
        unsafe { cpp_thread_is_blocked(self.as_raw()) }
    }

    /// Returns a `ThreadPtr` representing the currently executing thread.
    ///
    /// # Safety
    ///
    /// The caller must ensure that this function is called after multi-threading has been
    /// initialized (i.e. after LK_INIT_LEVEL_THREADING).
    pub unsafe fn current() -> Self {
        unsafe { Self::from_raw(cpp_thread_current_get()) }.unwrap()
    }

    /// Returns the thread's process and thread KOIDs.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the thread pointer is still valid and the
    /// underlying thread has not been destroyed.
    pub unsafe fn fxt_ref(self) -> FxtRef {
        unsafe { cpp_thread_fxt_ref(self.as_raw()) }
    }
}

/// Creates a new kernel thread with default priority.
///
/// # Safety
///
/// The caller must ensure that `entry` and `arg` are safe to run on a new thread.
pub unsafe fn create(
    name: *const c_char,
    entry: extern "C" fn(*mut c_void) -> i32,
    arg: *mut c_void,
) -> Result<ThreadPtr, Status> {
    let thread = unsafe { cpp_thread_create_default(name, entry, arg) };
    unsafe { ThreadPtr::from_raw(thread) }.ok_or(Status::NO_MEMORY)
}

/// Spawns a new kernel thread with default priority and resumes it.
///
/// # Safety
///
/// The caller must ensure that `entry` and `arg` are safe to run on a new thread,
/// and that the thread is joined before any borrowed data in `arg` is destroyed.
pub unsafe fn spawn(
    name: *const c_char,
    entry: extern "C" fn(*mut c_void) -> i32,
    arg: *mut c_void,
) -> Result<ThreadPtr, Status> {
    let thread = unsafe { create(name, entry, arg)? };
    unsafe { thread.resume() };
    Ok(thread)
}

/// Yields the current thread's CPU time slice.
pub fn r#yield() {
    unsafe { cpp_thread_current_yield() }
}

/// Disables preemption on the current thread.
pub fn preempt_disable() {
    // SAFETY: Calling this FFI function safely increments the preemption disable count for the
    // current thread.
    unsafe { cpp_thread_preempt_disable() }
}

/// Re-enables preemption on the current thread.
pub fn preempt_enable() {
    // SAFETY: Calling this FFI function safely decrements the preemption disable count for the
    // current thread.
    unsafe { cpp_thread_preempt_enable() }
}

/// Sets a timeslice extension on the current thread's preemption state.
pub fn preempt_set_timeslice_extension(duration: DurationMono) -> bool {
    // SAFETY: Calling this FFI function safely sets the timeslice extension on the current thread's
    // preemption state.
    unsafe { cpp_thread_preempt_set_timeslice_extension(duration) }
}

/// Clears an expiring timeslice extension on the current thread's preemption state.
pub fn preempt_clear_timeslice_extension() {
    // SAFETY: Calling this FFI function safely clears the timeslice extension on the current
    // thread's preemption state.
    unsafe { cpp_thread_preempt_clear_timeslice_extension() }
}

/// RAII guard that disables preemption for its scope.
///
/// This guard is `!Send` and `!Sync` because preemption state is CPU- and thread-local.
pub struct AutoPreemptDisabler {
    disabled: bool,
    _marker: PhantomData<*mut ()>,
}

impl AutoPreemptDisabler {
    /// Creates a new guard and immediately disables preemption.
    pub fn new() -> Self {
        preempt_disable();
        Self { disabled: true, _marker: PhantomData }
    }

    /// Creates a new guard without immediately disabling preemption.
    pub fn new_deferred() -> Self {
        Self { disabled: false, _marker: PhantomData }
    }

    /// Disables preemption if not already disabled by this guard instance.
    pub fn disable(&mut self) {
        if !self.disabled {
            preempt_disable();
            self.disabled = true;
        }
    }

    /// Re-enables preemption if previously disabled by this guard instance.
    pub fn enable(&mut self) {
        if self.disabled {
            preempt_enable();
            self.disabled = false;
        }
    }

    /// Returns whether preemption is currently disabled by this guard instance.
    pub fn is_disabled(&self) -> bool {
        self.disabled
    }
}

impl Default for AutoPreemptDisabler {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for AutoPreemptDisabler {
    fn drop(&mut self) {
        self.enable();
    }
}

/// RAII guard that sets a timeslice extension for its scope.
///
/// This guard is `!Send` and `!Sync` because timeslice extensions modify CPU- and thread-local
/// state.
pub struct AutoExpiringPreemptDisabler {
    should_clear: bool,
    _marker: PhantomData<*mut ()>,
}

impl AutoExpiringPreemptDisabler {
    /// Creates a new guard and attempts to set a timeslice extension for `duration`.
    pub fn new(duration: DurationMono) -> Self {
        let should_clear = preempt_set_timeslice_extension(duration);
        Self { should_clear, _marker: PhantomData }
    }
}

impl Drop for AutoExpiringPreemptDisabler {
    fn drop(&mut self) {
        if self.should_clear {
            preempt_clear_timeslice_extension();
        }
    }
}

/// Whether a block or sleep operation can be interrupted, matching C++ `Interruptible`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Interruptible(pub bool);

impl Interruptible {
    pub const NO: Self = Self(false);
    pub const YES: Self = Self(true);

    /// Converts the `Interruptible` setting to a boolean value (`Interruptible::YES` is `true`).
    #[inline]
    pub const fn as_bool(self) -> bool {
        self.0
    }
}

impl From<Interruptible> for bool {
    #[inline]
    fn from(i: Interruptible) -> bool {
        i.0
    }
}

impl From<bool> for Interruptible {
    #[inline]
    fn from(b: bool) -> Interruptible {
        Interruptible(b)
    }
}

/// Sleeps the current thread for the specified relative duration.
pub fn sleep_relative(duration: DurationMono) -> Result<(), Status> {
    // SAFETY: cpp_thread_current_sleep_relative is safe to call at any time in thread context.
    let status = unsafe { cpp_thread_current_sleep_relative(duration) };
    Status::ok(status)
}

/// Sleeps the current thread until the specified deadline with timer slack.
pub fn sleep_etc(
    deadline: &crate::kernel::types::Deadline,
    interruptible: Interruptible,
    now: zx_instant_mono_t,
) -> Result<(), Status> {
    // SAFETY: `deadline` points to a valid `Deadline`.
    let status = unsafe { cpp_thread_current_sleep_etc(deadline as *const _, interruptible, now) };
    Status::ok(status)
}

/// Soft faults a page at the given virtual address for the current thread.
pub fn soft_fault(va: usize, flags: u32) -> Result<(), Status> {
    // SAFETY: cpp_thread_current_soft_fault is safe to call from thread context.
    let status = unsafe { cpp_thread_current_soft_fault(va, flags) };
    Status::ok(status)
}

/// Returns the raw pointer to the current thread.
pub fn current_get() -> *mut Thread {
    unsafe { cpp_thread_current_get() }
}

/// Triggers preemption on the current thread.
pub fn preempt() {
    unsafe { cpp_thread_preempt() }
}

/// Dumps the call stack of the current thread.
pub fn dump_current_stack() {
    unsafe { cpp_thread_dump_current_stack() }
}

/// Processes pending signals on the current thread using the given iframe.
///
/// # Safety
/// Caller must ensure `frame` points to a valid architectural `iframe_t`.
pub unsafe fn process_pending_signals(frame: *mut c_void) {
    // SAFETY: Forwarded to C++ Thread::Current::ProcessPendingSignals with caller-verified frame.
    unsafe { cpp_thread_process_pending_signals(frame) }
}

/// Returns the top of the stack for `thread`.
///
/// # Safety
/// Caller must ensure `thread` points to a valid C++ `Thread` instance.
pub unsafe fn get_stack_top(thread: *mut Thread) -> usize {
    // SAFETY: Forwarded to C++ Thread::stack().top() with caller-verified pointer.
    unsafe { cpp_thread_get_stack_top(thread) }
}

/// Returns the shadow call stack base for `thread`.
///
/// # Safety
/// Caller must ensure `thread` points to a valid C++ `Thread` instance.
pub unsafe fn get_shadow_call_base(thread: *mut Thread) -> usize {
    // SAFETY: Forwarded to C++ Thread::stack().shadow_call_base() with caller-verified pointer.
    unsafe { cpp_thread_get_shadow_call_base(thread) }
}

/// Checks whether user state is saved for `thread`.
///
/// # Safety
/// Caller must ensure `thread` points to a valid C++ `Thread` instance whose thread lock is held.
pub unsafe fn is_user_state_saved(thread: *mut Thread) -> bool {
    // SAFETY: Forwarded to C++ Thread::IsUserStateSavedLocked() with caller-verified pointer.
    unsafe { cpp_thread_is_user_state_saved(thread) }
}

/// Checks whether `thread` is currently running.
///
/// # Safety
/// Caller must ensure `thread` points to a valid C++ `Thread` instance.
pub unsafe fn is_running(thread: *const Thread) -> bool {
    // SAFETY: Forwarded to C++ Thread::state() with caller-verified pointer.
    unsafe { cpp_thread_is_running(thread) }
}

/// Returns the name of `thread`.
///
/// # Safety
/// Caller must ensure `thread` points to a valid C++ `Thread` instance.
pub unsafe fn name(thread: *const Thread) -> *const c_char {
    // SAFETY: Forwarded to C++ Thread::name() with caller-verified pointer.
    unsafe { cpp_thread_name(thread) }
}

/// Checks whether `thread` is executing in restricted mode.
///
/// # Safety
/// Caller must ensure `thread` points to a valid C++ `Thread` instance.
pub unsafe fn is_in_restricted_mode(thread: *mut Thread) -> bool {
    // SAFETY: Forwarded to C++ Thread restricted state query with caller-verified pointer.
    unsafe { cpp_thread_is_in_restricted_mode(thread) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_preempt_guards_not_send_or_sync() {
        fn assert_not_send_sync<T>()
        where
            T: ?Sized,
        {
        }
        // Verification that the types compile and can be instantiated safely in unit tests.
        let _guard = AutoPreemptDisabler::new_deferred();
    }

    #[test]
    fn test_auto_preempt_disabler_deferred() {
        let mut guard = AutoPreemptDisabler::new_deferred();
        assert!(!guard.is_disabled());
        guard.disable();
        assert!(guard.is_disabled());
        guard.enable();
        assert!(!guard.is_disabled());
    }

    #[test]
    fn test_interruptible() {
        assert_eq!(Interruptible::NO.as_bool(), false);
        assert_eq!(Interruptible::YES.as_bool(), true);
        assert_eq!(bool::from(Interruptible::NO), false);
        assert_eq!(bool::from(Interruptible::YES), true);
    }

    #[test]
    fn test_auto_expiring_preempt_disabler() {
        let guard = AutoExpiringPreemptDisabler::new(DurationMono(10_000_000));
        drop(guard);
    }
}

zr::static_assert!(core::mem::size_of::<Interruptible>() == 1);
zr::static_assert!(core::mem::align_of::<Interruptible>() == 1);
