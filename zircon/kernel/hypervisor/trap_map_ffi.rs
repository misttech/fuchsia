// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{c_char, c_void};
use core::ptr::NonNull;
use fbl::RefPtr;
use pin_init::PinInit;
use zx_status::Status;
use zx_types::{ZX_OK, zx_gpaddr_t, zx_port_packet_t, zx_status_t};

use crate::object::{PortDispatcher, PortPacket};

use super::state_invalidator::StateInvalidator;
use super::trap_map::{BlockingPortAllocator, PortPacketArena, RustPortAllocator, Trap, TrapMap};

#[allow(improper_ctypes)]
unsafe extern "C" {
    pub(super) fn cpp_rust_port_allocator_init(allocator: *mut RustPortAllocator);
    pub(super) fn cpp_port_packet_arena_init(arena: *mut PortPacketArena);
    pub(super) fn cpp_port_packet_arena_destroy(arena: *mut PortPacketArena);
    pub(super) fn cpp_port_packet_arena_init_pool(
        arena: *mut PortPacketArena,
        name: *const c_char,
        max_count: usize,
    ) -> zx_status_t;
    pub(super) fn cpp_port_packet_arena_new(
        arena: *mut PortPacketArena,
        handle: *const c_void,
        allocator: *mut RustPortAllocator,
    ) -> *mut PortPacket;
    pub(super) fn cpp_port_packet_arena_delete(
        arena: *mut PortPacketArena,
        port_packet: *mut PortPacket,
    );
}

/// Allocates a `PortPacket` from the arena for C++ `PortAllocator::Alloc` callers.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub extern "C" fn rust_blocking_port_allocator_alloc(
    allocator: &BlockingPortAllocator,
) -> *mut PortPacket {
    allocator.alloc().map_or(core::ptr::null_mut(), NonNull::as_ptr)
}

/// Frees a `PortPacket` back to the arena and posts to the semaphore for C++ `PortAllocator::Free`
/// callers.
///
/// # Safety
///
/// `port_packet` must have been previously allocated from `allocator` and not yet freed.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_blocking_port_allocator_free(
    allocator: &BlockingPortAllocator,
    port_packet: NonNull<PortPacket>,
) {
    // SAFETY: Guaranteed by caller.
    unsafe {
        allocator.free(port_packet);
    }
}

/// Queues `packet` on a `Trap`'s port for C++ callers.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub extern "C" fn rust_trap_queue(
    trap: &Trap,
    packet: &zx_port_packet_t,
    invalidator: Option<&mut StateInvalidator>,
) -> zx_status_t {
    Status::result_into_raw(trap.queue(packet, invalidator))
}

/// Returns `true` if `trap` has an associated port.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub extern "C" fn rust_trap_has_port(trap: &Trap) -> bool {
    trap.has_port()
}

/// Returns the kind (`ZX_GUEST_TRAP_*`) of `trap`.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub extern "C" fn rust_trap_kind(trap: &Trap) -> u32 {
    trap.kind()
}

/// Returns the port packet key of `trap`.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub extern "C" fn rust_trap_key(trap: &Trap) -> u64 {
    trap.key()
}

/// Initializes a `TrapMap` in place for C++ callers.
///
/// # Safety
///
/// `trap_map` must be a valid, uninitialized, properly aligned pointer to a `TrapMap` that remains
/// pinned until `rust_trap_map_destroy`.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_trap_map_new(trap_map: *mut TrapMap) {
    // SAFETY: Guaranteed by caller.
    let _ = unsafe { TrapMap::new().__pinned_init(trap_map) };
}

/// Destroys a `TrapMap` in place for C++ callers.
///
/// # Safety
///
/// `trap_map` must be a valid initialized `TrapMap`.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_trap_map_destroy(trap_map: *mut TrapMap) {
    // SAFETY: Guaranteed by caller.
    unsafe {
        core::ptr::drop_in_place(trap_map);
    }
}

/// Inserts a trap into `trap_map` for C++ callers.
///
/// # Safety
///
/// `port` must be either null or a raw pointer exported from `fbl::RefPtr<PortDispatcher>`.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_trap_map_insert_trap(
    trap_map: &TrapMap,
    kind: u32,
    addr: zx_gpaddr_t,
    len: usize,
    port: *mut PortDispatcher,
    key: u64,
) -> zx_status_t {
    // SAFETY: `port` is either null or exported from `fbl::RefPtr<PortDispatcher>`.
    let port = unsafe { RefPtr::try_from_raw(port) };
    Status::result_into_raw(trap_map.insert_trap(kind, addr, len, port, key))
}

/// Finds a trap matching `(kind, addr)` in `trap_map` for C++ callers.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub extern "C" fn rust_trap_map_find_trap(
    trap_map: &TrapMap,
    kind: u32,
    addr: zx_gpaddr_t,
    out: &mut *mut Trap,
) -> zx_status_t {
    match trap_map.find_trap(kind, addr) {
        Ok(trap) => {
            *out = core::ptr::from_ref(trap).cast_mut();
            ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}
