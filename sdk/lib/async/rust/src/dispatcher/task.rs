// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod state;

use core::cell::UnsafeCell;
use core::fmt;
use core::future::Ready;
use core::hint::spin_loop;
use core::marker::PhantomData;
use core::mem::{ManuallyDrop, offset_of};
use core::pin::Pin;
use core::ptr::{NonNull, addr_of};
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use futures::task::AtomicWaker;
use libasync_sys::{async_dispatcher_t, async_state_t, async_task_t};
use zx::sys::{
    ZX_ERR_BAD_STATE, ZX_ERR_CANCELED, ZX_ERR_NOT_FOUND, ZX_ERR_NOT_SUPPORTED, ZX_OK,
    ZX_TIME_INFINITE, ZX_TIME_INFINITE_PAST, zx_status_t,
};

use crate::sys::{async_cancel_task, async_post_task};

use self::state::*;

// Most of the functions in here accept a `*const Header`. This is because they
// often need to cast back to a full `UnfinishedTask` or `FinishedTask`. If we
// dereferenced the header to a `&Header`, that would narrow the provenance of
// the reference to just the `Header` part of the overall task.
struct Header {
    refcount: AtomicUsize,
    state: State,
    waker: AtomicWaker,
    poll_task: async_task_t,
    shutdown_task: async_task_t,

    dispatcher: NonNull<async_dispatcher_t>,
    vtable: &'static VTable,
}

impl Header {
    const RAW_WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
        Self::waker_clone,
        Self::waker_wake,
        Self::waker_wake_by_ref,
        Self::waker_drop,
    );

    #[inline]
    unsafe fn waker_clone(data: *const ()) -> RawWaker {
        // SAFETY: `data` always points to a valid `Header`, and wakers hold a
        // task refcount.
        unsafe {
            Self::inc_ref(data.cast());
        }
        Self::raw_waker(data.cast())
    }

    #[inline]
    unsafe fn waker_wake(data: *const ()) {
        // SAFETY: `data` always points to a valid `Header`, and wakers hold a
        // task refcount.
        unsafe {
            Self::wake(data.cast());
        }
        // SAFETY: `data` always points to a valid `Header`, and wakers hold a
        // task refcount. We are releasing our refcount here.
        unsafe {
            Self::dec_ref(data.cast());
        }
    }

    #[inline]
    unsafe fn waker_wake_by_ref(data: *const ()) {
        // SAFETY: `data` always points to a valid `Header`, and wakers hold a
        // task refcount.
        unsafe {
            Self::wake(data.cast());
        }
    }

    #[inline]
    unsafe fn waker_drop(data: *const ()) {
        // SAFETY: `data` always points to a valid `Header`, and wakers hold a
        // task refcount. We are releasing our refcount here.
        unsafe {
            Self::dec_ref(data.cast());
        }
    }

    #[inline]
    fn raw_waker(header: *const Self) -> RawWaker {
        RawWaker::new(header.cast(), &Self::RAW_WAKER_VTABLE)
    }

    /// # Safety
    ///
    /// `header` must point to a valid `Header`, and the caller must be holding
    /// a task refcount.
    #[inline]
    unsafe fn inc_ref(header: *const Self) {
        // SAFETY: The caller guaranteed that `header` points to a valid
        // `Header`.
        unsafe {
            (*header).refcount.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// # Safety
    ///
    /// `header` must point to a valid `Header`, and the caller must be holding
    /// a task refcount. Decrementing the task refcount to 0 causes the task to
    /// be dropped, so callers should usually assume that `header` may dangle
    /// after calling `dec_ref`.
    #[inline]
    unsafe fn dec_ref(header: *const Self) {
        // SAFETY: The caller guaranteed that `header` points to a valid
        // `Header`.
        let this = unsafe { &*header };

        let prev = this.refcount.fetch_sub(1, Ordering::Relaxed);
        if prev == 1 {
            // This was the last refcount, clean up the task.

            // SAFETY: We just decremented the task refcount to 0, and so we
            // know that we have exclusive ownership of it.
            unsafe {
                (this.vtable.dealloc)(header);
            }
        }
    }

    /// # Safety
    ///
    /// `header` must point to a valid `Header`.
    #[inline]
    unsafe fn poll_task_ptr(header: *const Header) -> *mut async_task_t {
        // SAFETY: `header` points to a valid `Header`, and so is safe to
        // dereference.
        unsafe { addr_of!((*header).poll_task).cast_mut() }
    }

    /// # Safety
    ///
    /// `poll_task_ptr` must be a pointer to the `poll_task` field of a
    /// `Header`, and must be derived from a pointer to a `Header` without
    /// intervening references (which would narrow the provenance of the
    /// pointer).
    unsafe fn from_poll_task_ptr(poll_task_ptr: *mut async_task_t) -> *const Header {
        // SAFETY: `poll_task_ptr` points to the `poll_task` field of a
        // `Header`, and has the provenance of the `Header` itself.
        unsafe { poll_task_ptr.cast::<u8>().sub(offset_of!(Header, poll_task)).cast() }
    }

    /// # Safety
    ///
    /// `header` must point to a valid `Header`.
    unsafe fn shutdown_task_ptr(header: *const Header) -> *mut async_task_t {
        // SAFETY: `header` points to a valid `Header`, and so is safe to
        // dereference.
        unsafe { addr_of!((*header).shutdown_task).cast_mut() }
    }

    /// # Safety
    ///
    /// `shutdown_task_ptr` must be a pointer to the `shutdown_task` field of a
    /// `Header`, and must be derived from a pointer to a `Header` without
    /// intervening references (which would narrow the provenance of the
    /// pointer).
    unsafe fn from_shutdown_task_ptr(shutdown_task_ptr: *mut async_task_t) -> *const Header {
        // SAFETY: `shutdown_task_ptr` points to the `poll_task` field of a
        // `Header`, and has the provenance of the `Header` itself.
        unsafe { shutdown_task_ptr.cast::<u8>().sub(offset_of!(Header, shutdown_task)).cast() }
    }

    /// # Safety
    ///
    /// `header` must point to a valid `Header`, and the caller must be holding
    /// a task refcount.
    unsafe fn wake(header: *const Self) {
        // SAFETY: The caller guaranteed that `header` points to a valid
        // `Header`.
        let this = unsafe { &*header };

        // Race to set `IS_READY_BIT`.
        let prev = this.state.set_is_ready(Ordering::Relaxed);
        if prev.is_ready()
            || matches!(prev.payload(), Payload::Polling)
            || prev.is_aborted()
            || matches!(prev.payload(), Payload::Output)
        {
            // We didn't mark the task ready, or the dispatcher thread is in
            // charge of posting it, or the task is unsuitable for polling.
            return;
        }

        // We won the race, and the task should be posted.
        let prev = this.state.inc_dispatcher_refcount(Ordering::Relaxed);
        if !prev.is_shutting_down() {
            // SAFETY:
            // - The caller guaranteed that `header` points to a valid `Header`
            // - The caller is holding a task refcount
            // - We are currently holding a dispatcher refcount acquired before
            //   the dispatcher began shutting down, so the task's dispatcher
            //   must remain valid for the entirety of `post`
            // - We set `IS_READY_BIT`, which means that `poll_task` is not
            //   currently posted to the dispatcher and we are the only thread
            //   that will post it
            unsafe {
                Header::post(header);
            }
        }
        let _ = this.state.dec_dispatcher_refcount(Ordering::Relaxed);
    }

    /// Posts the task to the dispatcher.
    ///
    /// # Safety
    ///
    /// - `header` must point to a valid `Header`
    /// - The caller must be holding a task refcount
    /// - The task's dispatcher must remain valid for the entirety of `post`
    /// - The task's `poll_task` must not currently be posted to the dispatcher.
    unsafe fn post(header: *const Self) {
        // SAFETY: The caller guaranteed that `header` points to a valid
        // `Header`.
        let this = unsafe { &*header };

        // Add a refcount for the dispatcher.

        // SAFETY: The caller guaranteed that `header` points to a valid
        // `Header`, and is holding a task refcount.
        unsafe {
            Header::inc_ref(header);
        }

        // SAFETY:
        // - The caller is holding a dispatcher refcount acquired before the
        //   dispatcher began shutting down, and so `this.dispatcher` must still
        //   point to a
        //   valid dispatcher
        // - The caller guaranteed that `header` points to a valid `Header`
        // - The caller guaranteed that `poll_task` is not currently posted to
        //   the dispatcher.
        let result =
            unsafe { async_post_task(this.dispatcher.as_ptr(), Header::poll_task_ptr(header)) };
        match result {
            ZX_OK => {
                // The poll task was successfully posted.
            }
            ZX_ERR_BAD_STATE => {
                // The dispatcher is shutting down. Remove the refcount we added
                // for the dispatcher.

                // SAFETY: We are currently holding a refcount.
                unsafe {
                    Header::dec_ref(header);
                }
            }
            ZX_ERR_NOT_SUPPORTED => panic!("dispatcher does not support async_post_task"),
            _ => unreachable!("async_post_task returned {result} unexpectedly"),
        }
    }

    /// # Safety
    ///
    /// `header` must point to a valid `Header`, and the caller must be holding
    /// a task refcount.
    unsafe fn abort_and_cancel(header: *const Self) {
        // SAFETY: The caller guaranteed that `header` points to a valid
        // `Header`, and is holding a task refcount.
        let was_aborted = unsafe { Header::abort(header) };
        if !was_aborted {
            return;
        }

        // We dropped the header, so we should also attempt to cancel the task.

        // SAFETY: The caller guaranteed that `header` points to a valid
        // `Header`.
        let this = unsafe { &*header };

        let prev = this.state.inc_dispatcher_refcount(Ordering::Relaxed);
        if !prev.is_shutting_down() {
            // SAFETY:
            // - The caller guaranteed that `header` points to a valid `Header`
            // - The caller is holding a task refcount
            // - We are currently holding a dispatcher refcount acquired before
            //   the dispatcher began shutting down, and so `this.dispatcher`
            //   must still point to a valid dispatcher.
            unsafe {
                Header::cancel_task(header, Header::poll_task_ptr(header));
                Header::cancel_task(header, Header::shutdown_task_ptr(header));
            }
        }
        let _ = this.state.dec_dispatcher_refcount(Ordering::Relaxed);
    }

    /// # Safety
    ///
    /// `header` must point to a valid `Header`, and the caller must be holding
    /// a task refcount.
    unsafe fn abort(header: *const Self) -> bool {
        // SAFETY: The caller guaranteed that `header` points to a valid
        // `Header`.
        let this = unsafe { &*header };

        // Acquire ordering is used here because we may read the waker and/or
        // payload depending on the previous state
        let prev = this.state.set_is_aborted(Ordering::Acquire);
        if prev.is_aborted() {
            // The task has already been aborted by another thread.
            return false;
        }

        this.waker.wake();

        if !matches!(prev.payload(), Payload::Polling) {
            // SAFETY: We aborted the task while it was not polling, and so have
            // permission to drop its payload.
            unsafe {
                (this.vtable.drop)(header, prev.payload());
            }
        }

        true
    }

    /// # Safety
    ///
    /// - `header` must point to a valid `Header`
    /// - The caller must be holding a task refcount
    /// - The task's dispatcher must remain valid for the entirety of `post`
    ///   `task` must point to either the `poll_task` or `shutdown_task` fields
    ///   of `header`
    unsafe fn cancel_task(header: *const Self, task: *mut async_task_t) {
        // SAFETY: The caller guaranteed that `header` points to a valid
        // `Header`.
        let this = unsafe { &*header };

        // SAFETY:
        // - The caller guaranteed that `this.dispatcher` points to a valid
        //   dispatcher for the entirety of `cancel_task`.
        // - The caller guaranteed that `header` points to a valid `Header`.
        // - The caller guaranteed that `task` points to one of the tasks of
        //   `header`, and so is valid to cancel
        let result = unsafe { async_cancel_task(this.dispatcher.as_ptr(), task) };
        match result {
            ZX_OK => {
                // The task was canceled successfully. Remove the refcount held
                // by the dispatcher.

                // SAFETY: The caller guaranteed that `header` points to a valid
                // `Header`, and the caller is holding a task refcount.
                unsafe {
                    Header::dec_ref(header);
                }
            }
            ZX_ERR_NOT_FOUND => {
                // The task was not found in the dispatcher. This means either:
                // - The dispatcher has already dequeued the shutdown task and
                //   that thread will release the refcount
                // - Or, the shutdown task was already canceled and so the
                //   refcount has already been released.
            }
            ZX_ERR_NOT_SUPPORTED => panic!("dispatcher does not support async_cancel_task"),
            _ => unreachable!("async_cancel_task returned {result} unexpectedly"),
        }
    }

    /// # Safety
    ///
    /// `header` must point to a valid `Header`, and the caller must be holding
    /// a task refcount.
    unsafe fn is_finished(header: *const Self) -> bool {
        // SAFETY: the caller guaranteed that `header` points to a valid
        // `Header`, and will be for the duration of `is_finished` because they
        // are holding a refcount.
        let state = unsafe { (*header).state.load(Ordering::Relaxed) };
        state.is_aborted() || matches!(state.payload(), Payload::Output)
    }
}

// `Task<F>` is constructed as a union of a `Header`, `UnfinishedTask<F>`, and
// `FinishedTask<F::Output>`. This is done so that we can read the output of a
// task knowing only the type of the output (and not the type of the future).
//
// To know whether a task is `unfinished` or `finished`, we check the `state` in
// `header`. It is `Payload::Future` when `unfinished` and `Payload::Output`
// when finished. If it's currently polling, it's `Payload::Polling` and only
// the polling thread should touch it.
#[repr(C)]
union Task<F: Future> {
    header: ManuallyDrop<Header>,
    unfinished: ManuallyDrop<UnfinishedTask<F>>,
    finished: ManuallyDrop<FinishedTask<F::Output>>,
}

#[repr(C)]
struct UnfinishedTask<F: Future> {
    header: Header,
    future: UnsafeCell<F>,
}

#[repr(C)]
struct FinishedTask<O> {
    header: Header,
    output: UnsafeCell<O>,
}

struct VTable {
    drop: unsafe fn(*const Header, Payload),
    dealloc: unsafe fn(*const Header),
}

impl<F: Future> Task<F> {
    const VTABLE: VTable = VTable { drop: Self::drop, dealloc: Self::dealloc };

    // The returned task is unfinished with three refcounts.
    fn alloc(future: F, dispatcher: NonNull<async_dispatcher_t>) -> *const Header {
        Box::into_raw(Box::new(Self {
            unfinished: ManuallyDrop::new(UnfinishedTask {
                header: Header {
                    // Refcount starts at three: JoinHandle, poll task, and
                    // shutdown task.
                    refcount: AtomicUsize::new(3),
                    state: State::new_ready(),
                    waker: AtomicWaker::new(),
                    poll_task: async_task_t {
                        state: async_state_t { reserved: [0; 2] },
                        handler: Some(Self::poll_task_handler),
                        deadline: ZX_TIME_INFINITE_PAST,
                    },
                    shutdown_task: async_task_t {
                        state: async_state_t { reserved: [0; 2] },
                        handler: Some(Self::shutdown_task_handler),
                        deadline: ZX_TIME_INFINITE,
                    },

                    dispatcher,
                    vtable: &Self::VTABLE,
                },
                future: UnsafeCell::new(future),
            }),
        }))
        .cast()
    }

    fn alloc_aborted() -> *const Header {
        Box::into_raw(Box::new(Self {
            header: ManuallyDrop::new(Header {
                // Refcount starts at one: JoinHandle
                refcount: AtomicUsize::new(1),
                state: State::new_aborted(),
                waker: AtomicWaker::new(),
                poll_task: async_task_t {
                    state: async_state_t { reserved: [0; 2] },
                    handler: None,
                    deadline: ZX_TIME_INFINITE_PAST,
                },
                shutdown_task: async_task_t {
                    state: async_state_t { reserved: [0; 2] },
                    handler: None,
                    deadline: ZX_TIME_INFINITE,
                },

                dispatcher: NonNull::dangling(),
                vtable: &Self::VTABLE,
            }),
        }))
        .cast()
    }

    unsafe extern "C" fn poll_task_handler(
        _: *mut async_dispatcher_t,
        async_task: *mut async_task_t,
        status: zx_status_t,
    ) {
        // SAFETY: The API of libasync guarantees that `poll_task_handler` will
        // only be called with the tasks we provide it. For `poll_task_handler`,
        // those tasks always point to the `poll_task` field of a `Header`.
        let header = unsafe { Header::from_poll_task_ptr(async_task) };

        // Poll the task.

        // SAFETY:
        // - We posted a task where `header` points to a valid `Header`
        // - We always increment the task refcount before posting tasks, and so
        //   are currently holding a task refcount
        // - `poll_task_handler` is only called from a dispatcher thread, and
        //   the dispatcher is guaranteed to remain valid for the entirety of
        //   `poll_task_handler`
        unsafe {
            Self::poll(header, status);
        }

        // Remove the refcount held by the dispatcher.

        // SAFETY: We posted a task where `header` points to a valid `Header`,
        // and are currently holding the refcount added for us before calling
        // `post`.
        unsafe {
            Header::dec_ref(header);
        }
    }

    /// # Safety
    ///
    /// - `header` must point to a valid `Header`
    /// - The caller must be holding a task refcount
    /// - The task's dispatcher must remain valid for the entirety of `poll`
    #[inline]
    unsafe fn poll(header: *const Header, status: zx_status_t) {
        // SAFETY: The caller guaranteed that `header` points to a valid
        // `Header`.
        let this = unsafe { &*header };

        // `poll` may have been called in response to the dispatcher shutting
        // down. Check whether we're polling or canceling:
        if status == ZX_ERR_CANCELED {
            // Let the shutdown task manage task cancelation.
            return;
        }

        // Begin polling by unsetting `IS_READY_BIT` and transitioning from
        // `Unfinished` to `Polling`:

        // Acquire ordering is used here because we may read the waker and/or
        // payload depending on the previous state
        let before_polling =
            this.state.unset_is_ready_and_transition_future_to_polling(Ordering::Acquire);
        if before_polling.is_aborted() {
            // Another thread aborted the task before polling began. Transition
            // back to `Unfinished`.
            this.state.transition_polling_to_future(Ordering::Relaxed);
            return;
        }

        // Poll the future:

        // SAFETY: We transitioned the payload to polling before the dispatcher
        // began shutting down. This confers exclusive ownership over the
        // payload, which is guaranteed to be an unfinished future.
        let task = unsafe { &*header.cast::<UnfinishedTask<F>>() };

        // We place the waker in a `ManuallyDrop` so that only its clones will
        // affect the task's refcount.

        // SAFETY: `Header::raw_waker` returns a valid `RawWaker` for `header`.
        let waker = unsafe { ManuallyDrop::new(Waker::from_raw(Header::raw_waker(header))) };
        let mut context = Context::from_waker(&waker);

        // SAFETY: Tasks are allocated on the heap, and thus are not moved until
        // they are dropped.
        let future = unsafe { Pin::new_unchecked(&mut *task.future.get()) };
        match future.poll(&mut context) {
            Poll::Pending => {
                // The future is not finished and registered a waker somewhere
                // else. Transition the task back to `Unfinished`:

                // Release ordering is used here because we may have modified
                // the future while polling
                let after_polling = this.state.transition_polling_to_future(Ordering::Release);
                if after_polling.is_aborted() {
                    // The task was aborted during polling, so it's our job to
                    // drop the future.

                    // SAFETY:
                    // - The caller guaranteed that `header` points to a valid
                    //   `Header`
                    // - The caller is holding a task refcount
                    // - The task payload is guaranteed to be `Future` because
                    //   it just returned `Pending`, and the task was aborted
                    //   while polling (and so cannot be posted)
                    // - The task was aborted while we were polling, giving us
                    //   permission to drop its payload
                    unsafe {
                        Self::drop(header, Payload::Future);
                    }
                } else if after_polling.is_ready() {
                    // The task was readied during polling, so it's our job to
                    // repost it. We don't care whether or not we succeed.

                    // SAFETY:
                    // - The caller guaranteed that `header` points to a valid
                    //   `Header`
                    // - The caller is holding a task refcount
                    // - The caller guaranteed that the task's dispatcher will
                    //   remain valid for the entirety of `poll`
                    // - The task was readied while we were polling, so it isn't
                    //   currently posted to the dispatcher.
                    unsafe {
                        Header::post(header);
                    }
                } else {
                    // The task wasn't aborted or woken during polling, so we're
                    // done.
                }
            }
            Poll::Ready(output) => {
                // The task is finished. We drop the future, store the result in
                // its place, and transition the task state to `Ready`:

                // SAFETY: We are currently polling and have permission to drop
                // the task's payload.
                unsafe {
                    task.future.get().drop_in_place();
                }
                // SAFETY: `header` points to a task, and so may be interpreted
                // as a `FinishedTask<F::Output>`
                let task = unsafe { &*header.cast::<FinishedTask<F::Output>>() };
                // SAFETY: We are currently polling and have permission to write
                // to the task's payload
                unsafe {
                    task.output.get().write(output);
                }

                // Release ordering is used here because we wrote to the payload
                // of the task
                let after_polling = this.state.transition_polling_to_output(Ordering::Release);
                if after_polling.is_aborted() {
                    // The task was aborted during polling, so it's our job to
                    // drop the output.

                    // SAFETY:
                    // - The caller guaranteed that `header` points to a valid
                    //   `Header`
                    // - The caller is holding a task refcount
                    // - The payload is guaranteed to be `Output` because the
                    //   payload never changes after being set to `Output`
                    // - The task was aborted while we were polling, giving us
                    //   permission to drop its payload
                    unsafe {
                        Self::drop(header, Payload::Output);
                    }
                }

                // Whether we readied or aborted, any wakers need to be woken
                task.header.waker.wake();

                // The task doesn't need to be canceled by dispatcher shutdown
                // any more.

                // SAFETY:
                // - The caller guaranteed that `header` points to a valid
                //   `Header`
                // - The caller is holding a task refcount
                // - The caller guaranteed that the task's dispatcher will
                //   remain valid for the entirety of `poll`
                // - We are calling with the `shutdown_task` pointer of the task
                unsafe {
                    Header::cancel_task(header, Header::shutdown_task_ptr(header));
                }
            }
        }
    }

    unsafe extern "C" fn shutdown_task_handler(
        _: *mut async_dispatcher_t,
        async_task: *mut async_task_t,
        status: zx_status_t,
    ) {
        // SAFETY: The API of libasync guarantees that `shutdown_task_handler`
        // will only be called with the tasks we provide it. For
        // `shutdown_task_handler`, those tasks always point to the
        // `shutdown_task` field of a `Header`.
        let header = unsafe { Header::from_shutdown_task_ptr(async_task) };

        // Shutdown the task.

        // SAFETY:
        // - We posted a task where `header` points to a valid `Header`
        // - We always increment the task refcount before posting tasks, and so
        //   are currently holding a task refcount
        // - `shutdown_task_handler` is only called from a dispatcher thread,
        //   and the dispatcher is guaranteed to remain valid for the entirety
        //   of `shutdown_task_handler`
        unsafe {
            Self::shutdown(header, status);
        }

        // Remove the refcount held by the dispatcher.

        // SAFETY: We posted a task where `header` points to a valid `Header`,
        // and are currently holding the refcount added for us before calling
        // `post`.
        unsafe {
            Header::dec_ref(header);
        }
    }

    /// # Safety
    ///
    /// - `header` must point to a valid `Header`
    /// - The caller must be holding a task refcount
    /// - The task's dispatcher must remain valid for the entirety of `poll`
    #[inline]
    unsafe fn shutdown(header: *const Header, status: zx_status_t) {
        // SAFETY: The caller guaranteed that `header` points to a valid
        // `Header`.
        let this = unsafe { &*header };

        // The shutdown task should ever be called with `ZX_ERR_CANCELED`
        // because its deadline should never come to pass.
        assert_eq!(status, ZX_ERR_CANCELED);

        // Drop the task before holding the dispatcher open.

        // SAFETY: `header` points to a valid `Header`, and the caller is
        // holding a task refcount.
        unsafe {
            Header::abort(header);
        }

        let mut prev = this.state.set_is_shutting_down(Ordering::Relaxed);
        loop {
            if prev.dispatcher_refcount() == 0 {
                break;
            }

            spin_loop();

            prev = this.state.load(Ordering::Relaxed);
        }
    }

    /// # Safety
    ///
    /// - `header` must point to a valid `Header`
    /// - The caller must be holding a task refcount, or currently be
    ///   deallocating the task
    /// - `payload` must be the payload of the task for the entirety of `drop`
    /// - The caller must have permission to drop the task's payload
    #[inline]
    unsafe fn drop(header: *const Header, payload: Payload) {
        match payload {
            Payload::Future => {
                // The task is unfinished.

                // SAFETY: When `payload` is `Future`, the task is an
                // `UnfinishedTask`. The caller guaranteed they have permission
                // drop the task's payload.
                unsafe {
                    (*header.cast::<UnfinishedTask<F>>()).future.get().drop_in_place();
                }
            }
            Payload::Polling => unreachable!("tasks must not be dropped while polling"),
            Payload::Output => {
                // The task is finished.

                // SAFETY: When `payload` is `Output`, the task is a
                // `FinishedTask`. The caller guaranteed they have permission
                // drop the task's payload.
                unsafe {
                    (*header.cast::<FinishedTask<F::Output>>()).output.get().drop_in_place();
                }
            }
        }
    }

    /// # Safety
    ///
    /// `header` must point to a valid `Task<F>`, and the caller must have
    /// decremented the task refcount to 0.
    unsafe fn dealloc(header: *const Header) {
        // `task` is dropped and deallocated at the end of `dealloc`

        // SAFETY: The caller guaranteed that `header` points to a valid
        // `Task<F>` which we have permission to deallocate.
        let mut task = unsafe { Box::from_raw(header.cast_mut().cast::<Self>()) };

        // SAFETY: `task.header` is always valid to access.
        let state = unsafe { task.header.state.load_mut() };
        if !state.is_aborted() {
            // SAFETY:
            // - The caller guaranteed that `header` points to a valid `Header`
            // - We are currently deallocating the task
            // - The task's payload is guaranteed to be `state.payload()`, and
            //   will not change for the entirety of `drop` because there are no
            //   other references to the task
            // - The caller dropped the task refcount to 0, and so has
            //   permission to drop the task's payload
            unsafe {
                Self::drop(header, state.payload());
            }
        }

        // SAFETY: The caller dropped the task refcount to 0, and so has
        // permission to drop the task. This is the only time the task header
        // will be dropped.
        unsafe {
            ManuallyDrop::drop(&mut task.header);
        }
    }
}

/// Returns a `JoinHandle` to an aborted task.
///
/// Because aborted tasks will never be posted, `spawn_aborted` does not require
/// a dispatcher to spawn on.
pub fn spawn_aborted<T>() -> JoinHandle<T> {
    let header = Task::<Ready<T>>::alloc_aborted();

    JoinHandle {
        // SAFETY: `header` is returned from `Task::alloc` and so is guaranteed
        // not to be null
        header: unsafe { NonNull::new_unchecked(header.cast_mut()) },
        _phantom: PhantomData,
    }
}

/// Returns a `JoinHandle` to a task spawned on the given dispatcher.
///
/// # Safety
///
/// `dispatcher` must be a valid async dispatcher for the duration of
/// `spawn_on`.
pub unsafe fn spawn_on_unchecked<F>(
    future: F,
    dispatcher: NonNull<async_dispatcher_t>,
) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let header = Task::alloc(future, dispatcher);

    // SAFETY:
    // - The caller guaranteed that `dispatcher` will remain valid for the
    //   duration of `spawn_on`
    // - `header` points to the `Header` of a freshly-allocated task
    // - The `shutdown_task` is not currently posted to the executor because it
    //   was just created
    let result = unsafe { async_post_task(dispatcher.as_ptr(), Header::shutdown_task_ptr(header)) };
    match result {
        ZX_OK => {
            // SAFETY:
            // - The caller guaranteed that `dispatcher` will remain valid for
            //   the duration of `spawn_on`
            // - `header` points to the `Header` of a freshly-allocated task
            // - The `poll_task` is not currently posted to the executor because
            //   it was just created
            let result =
                unsafe { async_post_task(dispatcher.as_ptr(), Header::poll_task_ptr(header)) };
            match result {
                ZX_OK => {
                    // The task was successfully posted.
                }
                ZX_ERR_BAD_STATE => {
                    // The dispatcher is shutting down. Remove the refcount we
                    // added for the post task.

                    // SAFETY: `header` points to a valid `Header` because we
                    // just allocated it. We are currently holding a task
                    // refcount which will be given to the returned
                    // `JoinHandle`.
                    unsafe {
                        Header::dec_ref(header);
                    }

                    // Make an attempt to cancel the shutdown task.

                    // SAFETY:
                    // - `header` points to a valid `Header` because we just
                    //   allocated it
                    // - We are currently holding a task refcount which will be
                    //   given to the returned `JoinHandle`
                    // - The caller guaranteed that `dispatcher` will remain
                    //   valid for the duration of `spawn_on`
                    // - `task` points to the `shutdown_task` field of `header`
                    unsafe {
                        Header::abort(header);
                        Header::cancel_task(header, Header::shutdown_task_ptr(header));
                    }
                }
                ZX_ERR_NOT_SUPPORTED => panic!("dispatcher does not support async_post_task"),
                _ => unreachable!("async_post_task returned {result} unexpectedly"),
            }
        }
        ZX_ERR_BAD_STATE => {
            // The dispatcher is shutting down. Remove the refcounts we added
            // for the tasks and mark the task as aborted.

            // SAFETY: `header` points to a valid `Header` because we just
            // allocated it. We are currently holding a task refcount which will
            // be given to the returned `JoinHandle`.
            unsafe {
                Header::abort(header);
                Header::dec_ref(header);
                Header::dec_ref(header);
            }
        }
        ZX_ERR_NOT_SUPPORTED => panic!("dispatcher does not support async_post_task"),
        _ => unreachable!("async_post_task returned {result} unexpectedly"),
    }

    JoinHandle {
        // SAFETY: `header` is returned from `Task::alloc` and so is guaranteed
        // not to be null
        header: unsafe { NonNull::new_unchecked(header.cast_mut()) },
        _phantom: PhantomData,
    }
}

/// A task failed to execute to completion.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct JoinError {
    _phantom: PhantomData<()>,
}

/// A handle to a spawned task.
///
/// `JoinHandle` detaches tasks when dropped.
pub struct JoinHandle<T> {
    header: NonNull<Header>,
    _phantom: PhantomData<T>,
}

// SAFETY: `JoinHandle<T>` is `Send` as long as `T` is also send because it may
// read the output of a task and cause it to cross threads.
unsafe impl<T: Send> Send for JoinHandle<T> {}
// SAFETY: `JoinHandle<T>` is always `Sync` because it may be aborted or checked
// for finishing from any thread.
unsafe impl<T> Sync for JoinHandle<T> {}

impl<T> Unpin for JoinHandle<T> {}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        // SAFETY: `JoinHandle` always points to a valid `Header` and holds a
        // task refcount.
        unsafe {
            Header::dec_ref(self.header.as_ptr());
        }
    }
}

impl<T> JoinHandle<T> {
    /// Aborts the spawned task.
    pub fn abort(&self) {
        // SAFETY: `JoinHandle` always points to a valid `Header` and holds a
        // task refcount.
        unsafe {
            Header::abort_and_cancel(self.header.as_ptr());
        }
    }

    /// Returns `true` if the spawned task is finished.
    pub fn is_finished(&self) -> bool {
        // SAFETY: `JoinHandle` always points to a valid `Header` and holds a
        // task refcount.
        unsafe { Header::is_finished(self.header.as_ptr()) }
    }
}

impl<T> fmt::Debug for JoinHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("JoinHandle").finish()
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `JoinHandle` always points to a valid `Header` and holds a
        // task refcount.
        let header = unsafe { &*self.header.as_ptr() };

        header.waker.register(cx.waker());
        if !self.is_finished() {
            return Poll::Pending;
        }

        // SAFETY: `JoinHandle` always points to a valid `Header` and holds a
        // task refcount.
        let header = unsafe { &*self.header.as_ptr() };
        // Acquire ordering is used here because we may read the payload
        // depending on the previous state
        let prev = header.state.set_is_aborted(Ordering::Acquire);
        if !prev.is_aborted() {
            assert!(matches!(prev.payload(), Payload::Output));

            // Read out the result.
            let task_ptr = self.header.as_ptr().cast::<FinishedTask<T>>();

            // SAFETY: We just set `IS_ABORTED_BIT` before the dispatcher began
            // shutting down. We have permission to read and drop the payload.
            let output = unsafe { (*task_ptr).output.get().read() };
            Poll::Ready(Ok(output))
        } else {
            Poll::Ready(Err(JoinError { _phantom: PhantomData }))
        }
    }
}

#[cfg(test)]
mod tests {
    use core::pin::pin;

    use crate::OnDispatcher as _;
    use crate::test_dispatcher::TestDispatcher;

    #[test]
    fn basic_functionality() {
        let dispatcher = pin!(TestDispatcher::new());
        let result = dispatcher.run_until_stalled(async { 3 });
        assert_eq!(result, Some(3));
    }

    #[test]
    fn tag() {
        use futures::channel::oneshot;

        let dispatcher = pin!(TestDispatcher::new());

        let (a_send, a_recv) = oneshot::channel();
        let (b_send, b_recv) = oneshot::channel();
        (&dispatcher).spawn(async move {
            b_send.send(a_recv.await.unwrap()).unwrap();
        });
        dispatcher.run_to_completion(async move {
            a_send.send("hello").unwrap();
            assert_eq!(b_recv.await, Ok("hello"));
        });
    }

    #[test]
    fn abort() {
        let dispatcher = pin!(TestDispatcher::new());

        let handle = (&dispatcher).spawn(core::future::pending::<()>());
        handle.abort();

        dispatcher.run_to_completion(async move {
            assert!(handle.await.is_err());
        });
    }

    #[test]
    fn run_forever() {
        let dispatcher = pin!(TestDispatcher::new());

        let result = dispatcher.run_until_stalled(core::future::pending::<()>());
        assert!(result.is_none());
    }
}
