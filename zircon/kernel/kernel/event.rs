// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::marker::{PhantomData, PhantomPinned};
use core::ops::{Deref, DerefMut};
use event_bindings as bindings;
use pin_init::pin_data;
use zr::Opaque;
use zx_status::Status;

use crate::kernel::deadline::Deadline;
use crate::kernel::thread::Interruptible;
use crate::platform_rs::timer::InstantMono;

/// A synchronization event that allows threads to wait and signal.
///
/// An `Event` can be signaled, waking all waiting threads, and remains signaled
/// until explicitly reset via [`Event::unsignal`].
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct Event {
    raw: Opaque<bindings::Event>,
    phantom: PhantomData<PhantomPinned>,
}

// SAFETY: `Event` is thread-safe as internal synchronization is handled in C++.
unsafe impl Send for Event {}
// SAFETY: `Event` methods operate on shared references `&self` concurrently across threads.
unsafe impl Sync for Event {}

zr::unsafe_pinned_drop_ffi!(Event, bindings::cpp_event_destroy);

impl Event {
    /// Domain-specific conversion: returns raw pointer for `Event`.
    pub fn as_raw(&self) -> *mut bindings::Event {
        self.raw.get()
    }

    /// Domain-specific conversion: constructs a `&Event` from a raw bindings pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid, non-null pointer to a live C++ `Event`.
    pub unsafe fn from_raw_ref<'a>(ptr: *const bindings::Event) -> &'a Self {
        let ptr: *const Self = ptr.cast();
        // SAFETY: `bindings::Event` is layout-compatible with `Event`.
        unsafe { ptr.as_ref_unchecked() }
    }

    /// Domain-specific conversion: constructs a `&mut Event` from a raw bindings pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid, non-null pointer to a live C++ `Event`.
    pub unsafe fn from_raw_mut<'a>(ptr: *mut bindings::Event) -> &'a mut Self {
        let ptr: *mut Self = ptr.cast();
        // SAFETY: `bindings::Event` is layout-compatible with `Event`.
        unsafe { ptr.as_mut_unchecked() }
    }

    /// Returns a raw `Event` pointer from an underlying bindings pointer.
    ///
    /// Provides additional type safety when used instead of a `.cast()`.
    pub fn ptr_from_raw(raw: *mut bindings::Event) -> *mut Event {
        raw.cast()
    }

    /// Returns a pointer to the underlying `Event` structure.
    ///
    /// This method is helpful when you don't have a reference to the `Event`. If you do, then
    /// use `Event::as_raw` instead.
    pub fn cast_raw(ptr: *mut Event) -> *mut bindings::Event {
        ptr.cast()
    }

    /// Creates an in-place initializer for `Event`.
    pub fn init(initial: bool) -> impl pin_init::PinInit<Self, core::convert::Infallible> {
        unsafe {
            pin_init::pin_init_from_closure(move |slot: *mut Self| {
                bindings::cpp_event_init(slot.cast(), initial);
                Ok(())
            })
        }
    }

    /// Creates an in-place initializer for an initially unsignaled `Event`.
    pub fn init_unsignaled() -> impl pin_init::PinInit<Self, core::convert::Infallible> {
        Self::init(false)
    }

    /// Creates an in-place initializer for an initially signaled `Event`.
    pub fn init_signaled() -> impl pin_init::PinInit<Self, core::convert::Infallible> {
        Self::init(true)
    }

    /// Waits for the event to be signaled, with a deadline.
    ///
    /// Returns `Ok(())` on success (signaled), or an error status (such as `ZX_ERR_TIMED_OUT`,
    /// `ZX_ERR_INTERNAL_INTR_KILLED`, or `ZX_ERR_INTERNAL_INTR_RETRY`).
    pub fn wait(&self, deadline: &Deadline) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` returns a valid `Event` pointer and `deadline` is valid.
        let status = unsafe {
            bindings::cpp_event_wait_deadline(self.as_raw(), (deadline as *const Deadline).cast())
        };
        Status::ok(status)
    }

    /// Same as Wait() but gives a mask of signals to ignore. The signal_mask only applies to
    /// existing signals, not future ones that might be signaled while waiting. The caller must be
    /// interruptible.
    pub fn wait_mask(&self, deadline: &Deadline, signal_mask: u32) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` returns a valid `Event` pointer and `deadline` is valid.
        let status = unsafe {
            bindings::cpp_event_wait_mask(
                self.as_raw(),
                (deadline as *const Deadline).cast(),
                signal_mask,
            )
        };
        Status::ok(status)
    }

    /// Waits for the event indefinitely without interruption.
    pub fn wait_infinite(&self) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` returns a valid `Event` pointer.
        let status = unsafe { bindings::cpp_event_wait_infinite(self.as_raw()) };
        Status::ok(status)
    }

    /// Wait until a InstantMono deadline.
    /// Interruptible arg allows it to return early with ZX_ERR_INTERNAL_INTR_KILLED if thread
    /// is signaled for kill or with ZX_ERR_INTERNAL_INTR_RETRY if the thread is suspended.
    pub fn wait_deadline(
        &self,
        deadline: InstantMono,
        interruptible: Interruptible,
    ) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` returns a valid `Event` pointer.
        let status = unsafe {
            bindings::cpp_event_wait_interruptible(
                self.as_raw(),
                deadline.0,
                interruptible.as_bool(),
            )
        };
        Status::ok(status)
    }

    /// Signals the event with `ZX_OK` and wakes all waiting threads.
    pub fn signal(&self) {
        self.signal_result(Ok(()));
    }

    /// Signals the event with a specific status result.
    pub fn signal_result(&self, wait_result: impl Into<Result<(), Status>>) {
        // SAFETY: `self.as_raw()` returns a valid `Event` pointer.
        unsafe {
            bindings::cpp_event_signal(self.as_raw(), Status::result_into_raw(wait_result.into()));
        }
    }

    /// Signals the event with a status and an optional `OwnedWaitQueue` to assign ownership.
    ///
    /// # Safety
    ///
    /// `queue_to_own` must either be null or point to a valid `OwnedWaitQueue`.
    pub unsafe fn signal_etc(
        &self,
        wait_result: impl Into<Result<(), Status>>,
        queue_to_own: *mut bindings::OwnedWaitQueue,
    ) {
        // SAFETY: `self.as_raw()` returns a valid `Event` pointer and caller guarantees
        // `queue_to_own` is valid or null.
        unsafe {
            bindings::cpp_event_signal_etc(
                self.as_raw(),
                Status::result_into_raw(wait_result.into()),
                queue_to_own,
            );
        }
    }

    /// Resets the event to the unsignaled state.
    pub fn unsignal(&self) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` returns a valid `Event` pointer.
        let status = unsafe { bindings::cpp_event_unsignal(self.as_raw()) };
        Status::ok(status)
    }

    /// Returns `true` if the event is currently signaled.
    pub fn is_signaled(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `Event` pointer.
        unsafe { bindings::cpp_event_is_signaled(self.as_raw()) }
    }
}

/// An autounsignal event where signaling releases at most one waiting thread before
/// atomically resetting to unsignaled.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct AutounsignalEvent {
    raw: Opaque<bindings::AutounsignalEvent>,
    phantom: PhantomData<PhantomPinned>,
}

// SAFETY: `AutounsignalEvent` is thread-safe as internal synchronization is handled in C++.
unsafe impl Send for AutounsignalEvent {}
// SAFETY: `AutounsignalEvent` methods operate on shared references `&self` concurrently across threads.
unsafe impl Sync for AutounsignalEvent {}

zr::unsafe_pinned_drop_ffi!(AutounsignalEvent, bindings::cpp_autounsignal_event_destroy);

impl AutounsignalEvent {
    /// Domain-specific conversion: returns raw pointer for `AutounsignalEvent`.
    pub fn as_raw(&self) -> *mut bindings::AutounsignalEvent {
        self.raw.get()
    }

    /// Domain-specific conversion: constructs a `&AutounsignalEvent` from a raw bindings pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid, non-null pointer to a live C++ `AutounsignalEvent`.
    pub unsafe fn from_raw_ref<'a>(ptr: *const bindings::AutounsignalEvent) -> &'a Self {
        let ptr: *const Self = ptr.cast();
        // SAFETY: `bindings::AutounsignalEvent` is layout-compatible with `AutounsignalEvent`.
        unsafe { ptr.as_ref_unchecked() }
    }

    /// Domain-specific conversion: constructs a `&mut AutounsignalEvent` from a raw bindings pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid, non-null pointer to a live C++ `AutounsignalEvent`.
    pub unsafe fn from_raw_mut<'a>(ptr: *mut bindings::AutounsignalEvent) -> &'a mut Self {
        let ptr: *mut Self = ptr.cast();
        // SAFETY: `bindings::AutounsignalEvent` is layout-compatible with `AutounsignalEvent`.
        unsafe { ptr.as_mut_unchecked() }
    }

    /// Returns a raw `AutounsignalEvent` pointer from an underlying bindings pointer.
    ///
    /// Provides additional type safety when used instead of a `.cast()`.
    pub fn ptr_from_raw(raw: *mut bindings::AutounsignalEvent) -> *mut AutounsignalEvent {
        raw.cast()
    }

    /// Returns a pointer to the underlying `AutounsignalEvent` structure.
    ///
    /// This method is helpful when you don't have a reference to the `AutounsignalEvent`. If you
    /// do, then use `AutounsignalEvent::as_raw` instead.
    pub fn cast_raw(ptr: *mut AutounsignalEvent) -> *mut bindings::AutounsignalEvent {
        ptr.cast()
    }

    /// Creates an in-place initializer for `AutounsignalEvent`.
    pub fn init(initial: bool) -> impl pin_init::PinInit<Self, core::convert::Infallible> {
        unsafe {
            pin_init::pin_init_from_closure(move |slot: *mut Self| {
                bindings::cpp_autounsignal_event_init(slot.cast(), initial);
                Ok(())
            })
        }
    }

    /// Creates an in-place initializer for an initially unsignaled `AutounsignalEvent`.
    pub fn init_unsignaled() -> impl pin_init::PinInit<Self, core::convert::Infallible> {
        Self::init(false)
    }

    /// Creates an in-place initializer for an initially signaled `AutounsignalEvent`.
    pub fn init_signaled() -> impl pin_init::PinInit<Self, core::convert::Infallible> {
        Self::init(true)
    }

    /// Returns a reference to the underlying [`Event`].
    pub fn as_event(&self) -> &Event {
        self.deref()
    }

    /// Returns a mutable reference to the underlying [`Event`].
    pub fn as_event_mut(&mut self) -> &mut Event {
        self.deref_mut()
    }
}

impl Deref for AutounsignalEvent {
    type Target = Event;

    fn deref(&self) -> &Self::Target {
        // SAFETY: `self.as_raw()` returns a valid `AutounsignalEvent` pointer.
        // `cpp_autounsignal_event_as_event_const` casts the derived type pointer to its
        // base `Event` pointer, valid for the lifetime of `self`.
        unsafe {
            let raw_event = bindings::cpp_autounsignal_event_as_event_const(self.as_raw());
            Event::from_raw_ref(raw_event)
        }
    }
}

impl DerefMut for AutounsignalEvent {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: `self.as_raw()` returns a valid `AutounsignalEvent` pointer.
        // `cpp_autounsignal_event_as_event` casts the derived type pointer to its
        // base `Event` pointer, valid for the lifetime of `self`.
        unsafe {
            let raw_event = bindings::cpp_autounsignal_event_as_event(self.as_raw());
            Event::from_raw_mut(raw_event)
        }
    }
}

zr::static_assert!(core::mem::size_of::<Event>() == 72);
zr::static_assert!(core::mem::align_of::<Event>() == 8);
zr::static_assert!(core::mem::size_of::<AutounsignalEvent>() == 72);
zr::static_assert!(core::mem::align_of::<AutounsignalEvent>() == 8);

/// Event unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "event_rust")]
mod tests {
    use crate::kernel::owned_wait_queue::OwnedWaitQueue;
    use crate::kernel::thread::{self, DEFAULT_PRIORITY};
    use crate::platform_rs::timer::{DurationMono, zx_usec};
    use core::ffi::c_void;
    use pin_init::stack_pin_init;
    use unittest::{assert_eq, assert_true, expect_err, subtest, unwrap_ok};

    // This tests that the result in an event_signal_etc call is propagated to the waiter
    // when the event is signaled before any thread waits on the event.
    /// test signaling event with result before waiting
    #[test]
    fn signal_result_before_wait_test() {
        stack_pin_init!(let event = Event::init_unsignaled());

        let signal_result = Status::err_from_raw(42);
        event.signal_result(signal_result);

        let wake_result = event.wait_infinite();

        expect_err!(wake_result, signal_result, "");
    }

    struct EventWaiterArgs<'a> {
        event: &'a Event,
        wake_result: Result<(), Status>,
    }

    extern "C" fn event_waiter_thread(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` points to a valid `EventWaiterArgs` that outlives this thread, and no
        // other thread accesses `wake_result` until after this thread is joined.
        let event_args = unsafe { &mut *arg.cast::<EventWaiterArgs<'_>>() };
        event_args.wake_result = event_args.event.wait_infinite();
        0
    }

    #[derive(Clone, Copy)]
    enum Signal {
        Only,
        Queue,
    }

    // This tests that the result in an SignalEtc call is propagated to the waiter
    // when the waiter enters a blocking state before the event is signaled.
    fn run_signal_result_after_wait_test(signal: Signal) -> bool {
        (subtest!(|signal: Signal| {
            stack_pin_init!(let event = Event::init_unsignaled());

            let signal_result = Status::err_from_raw(42);

            let mut args = EventWaiterArgs { event: &event, wake_result: Ok(()) };

            // SAFETY: `event_waiter_thread` and `&mut args` are valid, and `waiter` is joined
            // below before `args` or `event` go out of scope.
            let waiter = unwrap_ok!(unsafe {
                thread::create_with_priority(
                    c"event waiter thread".as_ptr(),
                    event_waiter_thread,
                    (&raw mut args).cast(),
                    DEFAULT_PRIORITY,
                )
            });
            // SAFETY: `waiter` was just created and has not been resumed or joined yet.
            unsafe { waiter.resume() };

            let mut wait_duration = DurationMono(zx_usec(1));
            loop {
                // Check if the waiter thread is in the blocked state, indicating that the event
                // has latched.
                // SAFETY: `waiter` has not been joined yet.
                if unsafe { waiter.is_blocked() } {
                    break;
                }
                // Nope - sleep and try again.
                let _ = thread::sleep_relative(wait_duration);
                wait_duration = wait_duration * 2;
            }

            match signal {
                Signal::Queue => {
                    stack_pin_init!(let queue = OwnedWaitQueue::init());
                    // SAFETY: `queue.as_raw()` points to a valid `OwnedWaitQueue`.
                    unsafe {
                        event.signal_etc(signal_result, queue.as_raw().cast());
                    }
                    queue.reset_owner_if_no_waiters();
                }
                Signal::Only => {
                    event.signal_result(signal_result);
                }
            }

            // SAFETY: `waiter` has not been joined yet.
            let thread_retcode = unwrap_ok!(unsafe { waiter.join(InstantMono::INFINITE) });

            assert_eq!(thread_retcode, 0, "");

            expect_err!(args.wake_result, signal_result, "");
        }))(signal)
    }

    /// test signaling event with result after waiting
    #[test]
    fn signal_result_after_wait_test() {
        assert_true!(run_signal_result_after_wait_test(Signal::Only));
    }

    /// test signaling event with result after waiting (using OwnedWaitQueue)
    #[test]
    fn signal_etc_after_wait_test() {
        assert_true!(run_signal_result_after_wait_test(Signal::Queue));
    }
}
