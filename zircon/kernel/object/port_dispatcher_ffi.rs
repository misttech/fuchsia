// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::dispatcher::Dispatcher;
use super::handle::{Handle, HandleRef, KernelHandle};
use super::port_dispatcher::{
    PortAllocator, PortDispatcher, PortDispatcherState, PortObserver, PortPacket,
};
use crate::kernel::owned_wait_queue::OwnedWaitQueue;
use core::mem::MaybeUninit;
use core::ptr::NonNull;
use zx_types::{zx_signals_t, zx_status_t};

crate::object::dispatcher::impl_dispatcher_state_init!(
    PortDispatcher,
    PortDispatcherState,
    options: u32,
);

// C++ FFI declarations
unsafe extern "C" {
    pub fn cpp_port_dispatcher_create(
        options: u32,
        handle_out: *mut MaybeUninit<KernelHandle<PortDispatcher>>,
        rights_out: *mut MaybeUninit<zx_types::zx_rights_t>,
    ) -> zx_status_t;

    pub fn cpp_port_packet_alloc() -> *mut PortPacket;
    pub fn cpp_port_packet_free(packet: *mut PortPacket);
    pub fn cpp_port_default_allocator() -> *mut PortAllocator;
    pub fn cpp_port_allocator_free(allocator: *mut PortAllocator, packet: *mut PortPacket);
    pub fn cpp_port_ephemeral_packet_freed_count() -> i64;
    pub fn cpp_port_observer_freed_count() -> i64;

    pub fn cpp_port_observer_alloc(
        options: u32,
        handle: *const Handle,
        port: *mut PortDispatcher,
        key: u64,
        signals: zx_signals_t,
    ) -> *mut PortObserver;
    pub fn cpp_port_observer_free(observer: *mut PortObserver);
    pub fn cpp_port_observer_cancel(observer: *mut PortObserver);
    pub fn cpp_port_observer_add_dispatcher_ref_locked(observer: *mut PortObserver);
    pub fn cpp_port_observer_unlink_dispatcher_locked(
        observer: *mut PortObserver,
    ) -> *mut Dispatcher;
    pub fn cpp_port_observer_matches_key(
        observer: *mut PortObserver,
        port: *const PortDispatcher,
        key: u64,
    ) -> bool;
    pub fn cpp_port_observer_remove_from_dispatcher(
        observer: *mut PortObserver,
        dispatcher: *mut Dispatcher,
    ) -> bool;
}

/// Handles zero handles condition for the port dispatcher.
///
/// # Safety
///
/// `disp` must be a valid, initialized `PortDispatcher`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_port_dispatcher_on_zero_handles(disp: &PortDispatcher) {
    disp.on_zero_handles();
}

/// Queues a port packet to the port.
///
/// # Safety
///
/// `disp` must be a valid, initialized `PortDispatcher`.
/// `port_packet` must be null or point to a valid, initialized `PortPacket` that is not in any
/// container; a null `port_packet` is rejected with `ZX_ERR_INVALID_ARGS` (the C++ declaration
/// takes a raw `PortPacket*`, so a non-null type must not be assumed at the ABI boundary).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_port_dispatcher_queue(
    disp: &PortDispatcher,
    port_packet: *mut PortPacket,
) -> zx_status_t {
    let Some(port_packet) = NonNull::new(port_packet) else {
        return zx_status::Status::INVALID_ARGS.into_raw();
    };
    // SAFETY: `disp` is valid and `port_packet` is non-null and valid per the caller's contract.
    zx_status::Status::result_into_raw(unsafe { disp.queue(port_packet) })
}

/// Queues a packet and removes the observer from the port.
///
/// # Safety
///
/// `disp` must be a valid, initialized `PortDispatcher`.
/// `port_packet` must point to a valid `PortPacket` that is not in any container.
/// `observer` must be null or point to a valid `PortObserver` created on `disp`; when non-null,
/// `port_packet` must be the packet embedded in `observer`.
/// `queue_to_own` may be null or point to a valid `OwnedWaitQueue` that remains live until this
/// call returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_port_dispatcher_queue_and_remove_observer(
    disp: &PortDispatcher,
    port_packet: *mut PortPacket,
    observed: zx_signals_t,
    observer: *mut PortObserver,
    queue_to_own: *mut OwnedWaitQueue,
) -> zx_status_t {
    // SAFETY: The caller guarantees `queue_to_own` is null or points to a valid `OwnedWaitQueue`
    // that remains live until this call returns; only a shared reference is formed.
    let queue_to_own = unsafe { queue_to_own.as_ref() };
    // SAFETY: caller guarantees validity of `port_packet` and `observer`.
    zx_status::Status::result_into_raw(unsafe {
        disp.queue_and_remove_observer(port_packet, observed, observer, queue_to_own)
    })
}

/// Reaps an observer from the port dispatcher.
///
/// # Safety
///
/// `disp` must be a valid, initialized `PortDispatcher`.
/// `observer` must point to a valid `PortObserver` created on `disp`.
/// `port_packet` must point to a valid `PortPacket` and be the packet embedded in `observer`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_port_dispatcher_maybe_reap(
    disp: &PortDispatcher,
    observer: *mut PortObserver,
    port_packet: *mut PortPacket,
) {
    // SAFETY: caller guarantees pointer validity.
    unsafe {
        disp.maybe_reap(observer, port_packet);
    }
}

/// Creates and initializes a `PortObserver` registered with `disp`, returning it through
/// `out_observer`. The C++ caller performs the `PortObserver` -> `SignalObserver` upcast.
///
/// # Safety
///
/// `disp` must be a valid, initialized `PortDispatcher`.
/// `handle` must point to a valid `Handle` and the caller must hold a read lock on the handle
/// table containing it for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_port_dispatcher_make_observer(
    disp: &PortDispatcher,
    options: u32,
    handle: &Handle,
    key: u64,
    signals: zx_signals_t,
    out_observer: &mut *mut PortObserver,
) -> zx_status_t {
    // SAFETY: `handle` is a live `Handle` and the caller guarantees the handle table lock is held
    // for the duration of this call, which bounds the borrow.
    let handle = unsafe { HandleRef::from_raw(NonNull::from(handle)) };
    zx_status::Status::result_into_raw(disp.make_observer(options, handle, key, signals).map(
        |observer| {
            *out_observer = observer.as_ptr();
        },
    ))
}

/// Cancels a specific queued port packet.
///
/// # Safety
///
/// `disp` must be a valid, initialized `PortDispatcher`.
/// `port_packet` must point to a valid `PortPacket` that is either not in any container or
/// queued on `disp` (never on another port).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_port_dispatcher_cancel_queued_packet(
    disp: &PortDispatcher,
    port_packet: *mut PortPacket,
) -> bool {
    // SAFETY: caller guarantees `port_packet` validity.
    unsafe { disp.cancel_queued_packet(port_packet) }
}

/// Cancels all packets and observers matching key.
///
/// # Safety
///
/// `disp` must be a valid, initialized `PortDispatcher`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_port_dispatcher_cancel_key(
    disp: &PortDispatcher,
    key: u64,
) -> zx_status_t {
    zx_status::Status::result_into_raw(disp.cancel_key(key))
}
