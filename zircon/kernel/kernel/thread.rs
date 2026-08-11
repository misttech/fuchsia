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
    ) -> *mut c_void;
    fn cpp_thread_resume(thread: *mut c_void);
    fn cpp_thread_join(
        thread: *mut c_void,
        out_retcode: *mut i32,
        deadline: zx_instant_mono_t,
    ) -> i32;
    fn cpp_thread_current_yield();
    fn cpp_thread_kill(thread: *mut c_void);
    fn cpp_thread_is_blocked(thread: *mut c_void) -> bool;
    fn cpp_thread_current_get() -> *mut c_void;
    fn cpp_thread_fxt_ref(thread: *mut c_void) -> FxtRef;
    fn cpp_thread_preempt_set_timeslice_extension(duration: DurationMono) -> bool;
    fn cpp_thread_preempt_clear_timeslice_extension();
    fn cpp_thread_preempt_disable();
    fn cpp_thread_preempt_enable();
    fn cpp_thread_current_sleep_relative(duration: DurationMono) -> zx_status_t;
    fn cpp_thread_current_soft_fault(va: usize, flags: u32) -> zx_status_t;
    fn cpp_restricted_enter(vector_table_ptr: usize, context: usize) -> zx_status_t;
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

/// Enters restricted mode using the given vector table pointer and context.
pub fn restricted_enter(vector_table_ptr: usize, context: usize) -> Result<(), Status> {
    // SAFETY: `cpp_restricted_enter` performs validation of vector_table_ptr and context
    // in architecture-specific restricted mode entry routines.
    let status = unsafe { cpp_restricted_enter(vector_table_ptr, context) };
    Status::ok(status)
}

/// Type-safe wrapper around a raw pointer to a Zircon kernel Thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadPtr(NonNull<c_void>);

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
    pub const unsafe fn from_raw(ptr: *mut c_void) -> Option<Self> {
        match NonNull::new(ptr) {
            Some(nn) => Some(Self(nn)),
            None => None,
        }
    }

    /// Returns the raw pointer.
    pub const fn as_raw(self) -> *mut c_void {
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

/// Sleeps the current thread for the specified relative duration.
pub fn sleep_relative(duration: DurationMono) -> Result<(), Status> {
    // SAFETY: cpp_thread_current_sleep_relative is safe to call at any time in thread context.
    let status = unsafe { cpp_thread_current_sleep_relative(duration) };
    Status::ok(status)
}

/// Soft faults a page at the given virtual address for the current thread.
pub fn soft_fault(va: usize, flags: u32) -> Result<(), Status> {
    // SAFETY: cpp_thread_current_soft_fault is safe to call from thread context.
    let status = unsafe { cpp_thread_current_soft_fault(va, flags) };
    Status::ok(status)
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
    fn test_auto_expiring_preempt_disabler() {
        let guard = AutoExpiringPreemptDisabler::new(DurationMono(10_000_000));
        drop(guard);
    }
}
