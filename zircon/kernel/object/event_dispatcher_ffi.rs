// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::event_dispatcher::{
    EventDispatcher, EventDispatcherState, MemoryStallEventDispatcherState, MemoryStallKind,
};
use super::handle::KernelHandle;
use core::mem::MaybeUninit;
use zx_types::zx_status_t;

// C++ FFI declarations
unsafe extern "C" {
    /// Calls into C++ implementation to create an event dispatcher.
    ///
    /// # Safety
    ///
    /// `handle_out` must point to uninitialized memory for a `KernelHandle<EventDispatcher>`.
    pub(crate) fn cpp_event_dispatcher_create(
        options: u32,
        handle_out: *mut MaybeUninit<KernelHandle<EventDispatcher>>,
    ) -> zx_status_t;

    /// Calls into C++ to allocate a memory stall event dispatcher watching `kind`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `out_handle` is non-null, properly aligned, and points to
    /// writable uninitialized memory for a `KernelHandle<EventDispatcher>`, and that `kind` is a
    /// valid [`MemoryStallKind`] discriminant. On `ZX_OK`, `*out_handle` is initialized to a
    /// `KernelHandle<EventDispatcher>` owning a live `MemoryStallEventDispatcher`.
    pub(crate) fn cpp_memory_stall_event_dispatcher_create(
        kind: u32,
        out_handle: *mut MaybeUninit<KernelHandle<EventDispatcher>>,
    ) -> zx_status_t;
}

// FFI trampolines for C++ calling into Rust EventDispatcherState

crate::object::dispatcher::impl_dispatcher_state_init!(EventDispatcher, EventDispatcherState);

/// Destroys a `MemoryStallEventDispatcherState` in place.
///
/// # Safety
///
/// The caller must ensure that `state` is non-null, properly aligned, and points to a valid,
/// initialized `MemoryStallEventDispatcherState` embedded at
/// `kMemoryStallEventDispatcherStateOffset` inside a `MemoryStallEventDispatcher` whose base
/// `EventDispatcher` is still alive, and that `*state` is never accessed again after this call.
// SAFETY: Symbol name is globally unique and matches the `extern "C"` declaration in
// `zircon/kernel/object/event_dispatcher.cc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_memory_stall_event_dispatcher_state_destroy(
    state: *mut MemoryStallEventDispatcherState,
) {
    // SAFETY: By caller precondition, `state` is non-null, aligned, and points to a valid,
    // initialized `MemoryStallEventDispatcherState` that will not be used again; `PinnedDrop`
    // unregisters any observer under `observers_lock` before dropping the `Canary`.
    unsafe { core::ptr::drop_in_place(state) };
}

/// Initializes a `MemoryStallEventDispatcherState` in place.
///
/// # Safety
///
/// The caller must ensure that `ptr` is non-null, properly aligned, and points to writable
/// uninitialized storage for a `MemoryStallEventDispatcherState` at byte offset
/// `kMemoryStallEventDispatcherStateOffset` inside a live `MemoryStallEventDispatcher`, and that
/// the storage remains pinned until destroyed by
/// [`rust_memory_stall_event_dispatcher_state_destroy`].
///
/// # Panics
///
/// Panics if `kind` names neither stall kind. `EventDispatcher::create_memory_stall` is the only
/// thing that reaches the constructor this runs in, and it rejects such a `kind` before
/// allocating.
// SAFETY: Symbol name is globally unique and matches the `extern "C"` declaration in
// `zircon/kernel/object/event_dispatcher.cc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_memory_stall_event_dispatcher_state_init(
    ptr: *mut MemoryStallEventDispatcherState,
    kind: u32,
) {
    let kind = MemoryStallKind::from_raw(kind).expect("Impossible stall kind value");
    // SAFETY: By caller precondition, `ptr` is non-null, aligned, writable uninitialized storage
    // at `kMemoryStallEventDispatcherStateOffset` inside a live `MemoryStallEventDispatcher` and
    // remains pinned until dropped in place.
    unsafe {
        let _ = pin_init::PinInit::__pinned_init(MemoryStallEventDispatcherState::init(kind), ptr);
    }
}
