// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::event_dispatcher_ffi::{
    cpp_event_dispatcher_create, cpp_memory_stall_event_dispatcher_create,
};
use super::{DispatcherOps, KernelHandle};
use crate::counters::define_kcounter;
use crate::platform_rs::timer::DurationMono;
use crate::stall::{EventReceiver, StallAggregator, StallObserver};
use core::ptr::NonNull;
use fbl::Canary;
use ksync::{KMutex, RawCriticalMutex, guarded};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;
use zx_types::{
    ZX_DEFAULT_EVENT_RIGHTS, ZX_DEFAULT_SYSTEM_MEMORY_STALL_EVENT_RIGHTS, ZX_EVENT_SIGNALED,
    ZX_OBJ_TYPE_EVENT, ZX_SYSTEM_MEMORY_STALL_FULL, ZX_SYSTEM_MEMORY_STALL_SOME,
    zx_duration_mono_t, zx_rights_t,
};

use object_constants_rs as object_constants;

zr::static_assert_size_and_align!(
    EventDispatcherState,
    object_constants::kEventDispatcherStateSize,
    object_constants::kEventDispatcherStateAlign,
);

define_kcounter!(DISPATCHER_EVENT_CREATE_COUNT, "dispatcher.event.create", Sum);
define_kcounter!(DISPATCHER_EVENT_DESTROY_COUNT, "dispatcher.event.destroy", Sum);

#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct EventDispatcherState {
    canary: Canary<{ fbl::magic(b"EVTD") }>,

    #[mutex]
    lock: KMutex<RawCriticalMutex>,
}

impl EventDispatcherState {
    #[inline]
    pub fn init(
        _dispatcher: *const EventDispatcher,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        DISPATCHER_EVENT_CREATE_COUNT.add(1);
        pin_init!(Self {
            canary: Canary::new(),
            lock <- KMutex::init(),
        })
    }
}

#[pinned_drop]
impl PinnedDrop for EventDispatcherState {
    fn drop(self: core::pin::Pin<&mut Self>) {
        DISPATCHER_EVENT_DESTROY_COUNT.add(1);
    }
}

crate::object::dispatcher::impl_dispatcher_facade_with_state!(
    pub struct EventDispatcher,
    EventDispatcherState,
    ZX_OBJ_TYPE_EVENT,
    object_constants::kEventDispatcherStateOffset
);

impl EventDispatcher {
    /// Creates a new EventDispatcher via C++ and returns its kernel handle and rights.
    pub fn create() -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        // SAFETY: `cpp_event_dispatcher_create` initializes `handle` on success.
        let handle = unsafe { KernelHandle::create(|out| cpp_event_dispatcher_create(out)) }?;
        Ok((handle, ZX_DEFAULT_EVENT_RIGHTS))
    }

    /// Creates a memory stall watch event dispatcher.
    pub fn create_memory_stall(
        kind: u32,
        threshold: zx_duration_mono_t,
        window: zx_duration_mono_t,
    ) -> Result<(KernelHandle<EventDispatcher>, zx_rights_t), Status> {
        // Validate `kind` before allocating anything, so that there is nothing to clean up if it
        // is unrecognized.
        let kind = MemoryStallKind::from_raw(kind).ok_or(Status::INVALID_ARGS)?;

        // C++ owns the allocation; its constructor initializes the state below with `kind`.
        //
        // SAFETY: `KernelHandle::create` passes a valid, non-null, aligned `out` pointer to
        // uninitialized storage, `kind as u32` is a valid `MemoryStallKind` discriminant, and
        // `cpp_memory_stall_event_dispatcher_create` initializes `*out` with a valid
        // `KernelHandle<EventDispatcher>` wrapping a `MemoryStallEventDispatcher` on `ZX_OK`.
        let handle = unsafe {
            KernelHandle::create(|out| cpp_memory_stall_event_dispatcher_create(kind as u32, out))
        }?;

        // SAFETY: `handle` was just produced by `cpp_memory_stall_event_dispatcher_create`, so its
        // dispatcher is a live `MemoryStallEventDispatcher` whose state at
        // `kMemoryStallEventDispatcherStateOffset` has been initialized.
        let state = unsafe { handle.dispatcher().memory_stall_state() };

        // SAFETY: `state` is non-null, aligned, and points to the dispatcher's own
        // `MemoryStallEventDispatcherState` pinned inside its heap allocation (`opaque_storage_`).
        // The observer is registered in the `StallAggregator` list matching `state`'s `kind`
        // below, and `~MemoryStallEventDispatcher` unregisters and destroys the observer under
        // `observers_lock` before `state` or the base `EventDispatcher` goes away.
        let observer = unsafe {
            StallObserver::create(
                DurationMono::from_nanos(threshold),
                DurationMono::from_nanos(window),
                state,
            )
        }?;

        let aggregator = StallAggregator::get();
        match kind {
            MemoryStallKind::Some => aggregator.add_observer_some(observer),
            MemoryStallKind::Full => aggregator.add_observer_full(observer),
        }

        Ok((handle, ZX_DEFAULT_SYSTEM_MEMORY_STALL_EVENT_RIGHTS))
    }

    /// Returns a pointer to the memory stall state of a `MemoryStallEventDispatcher`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `self` points to a live `MemoryStallEventDispatcher` whose
    /// `MemoryStallEventDispatcherState` at `kMemoryStallEventDispatcherStateOffset` is
    /// initialized.
    unsafe fn memory_stall_state(&self) -> NonNull<MemoryStallEventDispatcherState> {
        // SAFETY: By caller precondition, `self` points to the base of a live
        // `MemoryStallEventDispatcher` allocation. The C++ constructor statically asserts via
        // `DISPATCHER_VERIFY_OFFSET` that `opaque_storage_` lies at
        // `kMemoryStallEventDispatcherStateOffset` within that allocation, and `OpaqueStorage`
        // guarantees non-null alignment matching `kMemoryStallEventDispatcherStateAlign`.
        unsafe {
            NonNull::new_unchecked(
                (self as *const Self)
                    .wrapping_byte_add(object_constants::kMemoryStallEventDispatcherStateOffset)
                    .cast::<MemoryStallEventDispatcherState>()
                    .cast_mut(),
            )
        }
    }
}

zr::static_assert_size_and_align!(
    MemoryStallEventDispatcherState,
    object_constants::kMemoryStallEventDispatcherStateSize,
    object_constants::kMemoryStallEventDispatcherStateAlign,
);

/// Which of the two aggregated stall times a `MemoryStallEventDispatcher` watches.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryStallKind {
    Some = ZX_SYSTEM_MEMORY_STALL_SOME,
    Full = ZX_SYSTEM_MEMORY_STALL_FULL,
}

impl MemoryStallKind {
    /// Returns the kind `raw` names, or `None` if it names neither.
    pub(crate) fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            ZX_SYSTEM_MEMORY_STALL_SOME => Some(Self::Some),
            ZX_SYSTEM_MEMORY_STALL_FULL => Some(Self::Full),
            _ => None,
        }
    }
}

/// The state of a `MemoryStallEventDispatcher`, stored inside the C++ object.
///
/// There is a 1:1 correspondence between it and the `StallObserver` that decides whether we are
/// currently above or below the threshold; this is the receiver that observer notifies. The
/// observer itself is owned by the `StallAggregator` it is registered with.
///
/// # Invariants
///
/// Every initialized `MemoryStallEventDispatcherState` is embedded at byte offset
/// `kMemoryStallEventDispatcherStateOffset` (`opaque_storage_`) inside a live
/// `MemoryStallEventDispatcher` allocation.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct MemoryStallEventDispatcherState {
    canary: Canary<{ fbl::magic(b"MSED") }>,

    /// The kind of memory stall that our observer has been registered to (used by our destructor
    /// to unregister it).
    // Safety invariant: If an observer for `self` is registered with `StallAggregator`, it is
    // registered at most once and in the list (`some` or `full`) matching `kind`.
    kind: MemoryStallKind,

    /// Marks the state as `!Freeze` and `!Unpin` (zero-sized, alignment 1) because it sits inside
    /// an address-sensitive C++ `MemoryStallEventDispatcher` allocation with interior mutability.
    _facade: zr::OpaqueFacade,
}

// SAFETY: `MemoryStallEventDispatcherState` is immutable after initialization (`canary` and `kind`
// are only read until drop); `_facade` is a zero-sized marker for the enclosing C++ allocation,
// whose state transitions are synchronized by `EventDispatcherState::lock` and `observers_lock`.
unsafe impl Send for MemoryStallEventDispatcherState {}
// SAFETY: Shared references to `MemoryStallEventDispatcherState` only read immutable `canary` and
// `kind` fields or call `EventDispatcher::update_state`, which is synchronized by the dispatcher's
// lock.
unsafe impl Sync for MemoryStallEventDispatcherState {}

impl MemoryStallEventDispatcherState {
    /// Creates the state for a dispatcher watching `kind`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the returned initializer is used to initialize storage at byte
    /// offset `kMemoryStallEventDispatcherStateOffset` inside a live `MemoryStallEventDispatcher`
    /// allocation whose base `EventDispatcher` outlives this state.
    pub(crate) unsafe fn init(
        kind: MemoryStallKind,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self { canary: Canary::new(), kind, _facade: zr::OpaqueFacade::default() })
    }

    /// Returns the dispatcher this state is embedded in.
    fn dispatcher(&self) -> &EventDispatcher {
        self.canary.assert();
        // SAFETY: By the type invariant established in `MemoryStallEventDispatcherState::init`,
        // `self` only ever exists at byte offset `kMemoryStallEventDispatcherStateOffset` inside a
        // live `MemoryStallEventDispatcher` allocation whose base `EventDispatcher` at offset 0
        // outlives `self`.
        unsafe {
            &*(self as *const Self)
                .wrapping_byte_sub(object_constants::kMemoryStallEventDispatcherStateOffset)
                .cast::<EventDispatcher>()
        }
    }
}

impl EventReceiver for MemoryStallEventDispatcherState {
    fn on_above_threshold(&self) {
        self.dispatcher().update_state(0, ZX_EVENT_SIGNALED);
    }

    fn on_below_threshold(&self) {
        self.dispatcher().update_state(ZX_EVENT_SIGNALED, 0);
    }
}

#[pinned_drop]
impl PinnedDrop for MemoryStallEventDispatcherState {
    fn drop(self: core::pin::Pin<&mut Self>) {
        // Unregistering destroys the observer, and it happens with the aggregator's observer lock
        // held, which is what guarantees the sampling thread is not part way through one of our
        // callbacks.
        //
        // The result is discarded because not finding an observer is expected: creating one can
        // fail after the dispatcher has been allocated. Since a receiver identifies its observer
        // rather than holding a pointer to it, that case cannot be told apart from a bookkeeping
        // bug here.
        let aggregator = StallAggregator::get();
        let this = &*self;
        match this.kind {
            MemoryStallKind::Some => {
                aggregator.remove_observer_some(this);
            }
            MemoryStallKind::Full => {
                aggregator.remove_observer_full(this);
            }
        }
    }
}
