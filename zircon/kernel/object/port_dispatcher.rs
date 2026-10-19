// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::KernelHandle;
use super::dispatcher::impl_dispatcher_facade;
use super::handle::HandleRef;
use crate::kernel::deadline::Deadline;
use core::mem::MaybeUninit;
use object_constants_rs as object_constants;
use pin_init::{PinInit, pin_data};
use zerocopy::{FromBytes, Immutable, IntoBytes};
use zr::OpaqueBytes;
use zx_status::Status;
use zx_types::{ZX_OBJ_TYPE_PORT, zx_instant_boot_t, zx_port_packet_t, zx_rights_t, zx_status_t};

/// Raw ABI-compatible representation of `zx_port_packet_t` with `packet_type` as a plain `u32` so
/// that copying arbitrary user bytes in `zx_port_queue` cannot construct an invalid Rust enum
/// discriminant.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, FromBytes, IntoBytes, Immutable)]
pub struct RawPortPacket {
    pub key: u64,
    pub packet_type: u32,
    pub status: i32,
    pub union: [u8; 32],
}

zr::static_assert_size_and_align!(
    RawPortPacket,
    core::mem::size_of::<zx_port_packet_t>(),
    core::mem::align_of::<zx_port_packet_t>(),
);
const _: () = {
    assert!(
        core::mem::offset_of!(RawPortPacket, key) == core::mem::offset_of!(zx_port_packet_t, key)
    );
    assert!(
        core::mem::offset_of!(RawPortPacket, packet_type)
            == core::mem::offset_of!(zx_port_packet_t, packet_type)
    );
    assert!(
        core::mem::offset_of!(RawPortPacket, status)
            == core::mem::offset_of!(zx_port_packet_t, status)
    );
    assert!(
        core::mem::offset_of!(RawPortPacket, union)
            == core::mem::offset_of!(zx_port_packet_t, union)
    );
};

#[allow(improper_ctypes)]
unsafe extern "C" {
    fn cpp_port_dispatcher_create(
        options: u32,
        handle_out: *mut MaybeUninit<KernelHandle<PortDispatcher>>,
        rights_out: *mut MaybeUninit<zx_rights_t>,
    ) -> zx_status_t;
    fn cpp_port_dispatcher_packet_init(packet: *mut PortInterruptPacket);
    fn cpp_port_dispatcher_packet_destroy(packet: *mut PortInterruptPacket);
    fn cpp_port_dispatcher_packet_get_key(packet: *const PortInterruptPacket) -> u64;
    fn cpp_port_dispatcher_packet_set_key(packet: *mut PortInterruptPacket, key: u64);
    fn cpp_port_dispatcher_packet_get_timestamp(
        packet: *const PortInterruptPacket,
    ) -> zx_instant_boot_t;
    fn cpp_port_dispatcher_packet_set_timestamp(
        packet: *mut PortInterruptPacket,
        timestamp: zx_instant_boot_t,
    );
    fn cpp_port_dispatcher_can_bind_to_interrupt(port: *const PortDispatcher) -> bool;
    fn cpp_port_dispatcher_queue_interrupt_packet(
        port: *mut PortDispatcher,
        packet: *mut PortInterruptPacket,
        timestamp: zx_instant_boot_t,
    ) -> bool;
    fn cpp_port_dispatcher_remove_interrupt_packet(
        port: *mut PortDispatcher,
        packet: *mut PortInterruptPacket,
    ) -> bool;
    fn cpp_port_dispatcher_queue_user(
        port: *mut PortDispatcher,
        packet: *const RawPortPacket,
    ) -> zx_status_t;
    fn cpp_port_dispatcher_dequeue(
        port: *mut PortDispatcher,
        deadline: *const Deadline,
        packet: *mut MaybeUninit<RawPortPacket>,
    ) -> zx_status_t;
    fn cpp_port_dispatcher_cancel_queued(
        port: *mut PortDispatcher,
        handle: *const core::ffi::c_void,
        key: u64,
    ) -> bool;
    fn cpp_port_dispatcher_cancel_key(port: *mut PortDispatcher, key: u64) -> zx_status_t;
    fn cpp_port_packet_set_packet(port_packet: *mut PortPacket, packet: *const zx_port_packet_t);
    fn cpp_port_dispatcher_queue(port: *mut PortDispatcher, packet: *mut PortPacket)
    -> zx_status_t;
}

/// Opaque handle to a C++ `PortPacket`.
#[repr(C)]
pub struct PortPacket {
    _opaque: zr::OpaqueFacade,
}

impl PortPacket {
    /// Sets the `zx_port_packet_t` payload on this `PortPacket`.
    #[inline]
    pub fn set_packet(&mut self, packet: &zx_port_packet_t) {
        // SAFETY: `self` and `packet` are valid references.
        unsafe { cpp_port_packet_set_packet(self, packet) }
    }
}

/// Opaque wrapper for the C++ `PortInterruptPacket` intrusive node.
#[repr(C, align(8))]
#[pin_data(PinnedDrop)]
pub struct PortInterruptPacket {
    _opaque: OpaqueBytes<{ object_constants::kPortInterruptPacketSize }>,
}

zr::static_assert_size_and_align!(
    PortInterruptPacket,
    object_constants::kPortInterruptPacketSize,
    object_constants::kPortInterruptPacketAlign,
);

impl PortInterruptPacket {
    /// Returns a pin-initializer that constructs the underlying C++ `PortInterruptPacket`.
    pub fn init() -> impl PinInit<Self, core::convert::Infallible> {
        zr::pin_init_ffi!(cpp_port_dispatcher_packet_init)
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut Self {
        self._opaque.get().cast()
    }

    /// Returns the port packet key.
    ///
    /// # Safety
    ///
    /// The caller must hold the owning `InterruptDispatcher`'s spinlock and ensure the packet
    /// is not currently queued in a `PortDispatcher`.
    pub unsafe fn key(&self) -> u64 {
        // SAFETY: Guaranteed by the caller; `as_mut_ptr()` accesses the `UnsafeCell` inside
        // `OpaqueBytes`.
        unsafe { cpp_port_dispatcher_packet_get_key(self.as_mut_ptr()) }
    }

    /// Sets the port packet key.
    ///
    /// # Safety
    ///
    /// The caller must hold the owning `InterruptDispatcher`'s spinlock and ensure the packet
    /// is not currently queued in a `PortDispatcher`.
    pub unsafe fn set_key(&self, key: u64) {
        // SAFETY: Guaranteed by the caller; `as_mut_ptr()` accesses the `UnsafeCell` inside
        // `OpaqueBytes`.
        unsafe { cpp_port_dispatcher_packet_set_key(self.as_mut_ptr(), key) }
    }

    /// Returns the port packet timestamp.
    ///
    /// # Safety
    ///
    /// The caller must hold the owning `InterruptDispatcher`'s spinlock and ensure the packet
    /// is not currently queued in a `PortDispatcher`.
    pub unsafe fn timestamp(&self) -> zx_instant_boot_t {
        // SAFETY: Guaranteed by the caller; `as_mut_ptr()` accesses the `UnsafeCell` inside
        // `OpaqueBytes`.
        unsafe { cpp_port_dispatcher_packet_get_timestamp(self.as_mut_ptr()) }
    }

    /// Sets the port packet timestamp.
    ///
    /// # Safety
    ///
    /// The caller must hold the owning `InterruptDispatcher`'s spinlock and ensure the packet
    /// is not currently queued in a `PortDispatcher`.
    pub unsafe fn set_timestamp(&self, timestamp: zx_instant_boot_t) {
        // SAFETY: Guaranteed by the caller; `as_mut_ptr()` accesses the `UnsafeCell` inside
        // `OpaqueBytes`.
        unsafe { cpp_port_dispatcher_packet_set_timestamp(self.as_mut_ptr(), timestamp) }
    }
}

zr::unsafe_pinned_drop_ffi!(PortInterruptPacket, cpp_port_dispatcher_packet_destroy);

impl_dispatcher_facade!(
    /// Facade for C++ `PortDispatcher`.
    pub struct PortDispatcher,
    ZX_OBJ_TYPE_PORT
);

impl PortDispatcher {
    #[inline]
    fn as_ffi_mut(&self) -> *mut Self {
        core::ptr::from_ref(self).cast_mut()
    }

    /// Creates a new `PortDispatcher` with `options`.
    pub fn create(options: u32) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        // SAFETY: `cpp_port_dispatcher_create` initializes both out parameters on `ZX_OK`.
        unsafe {
            KernelHandle::create_with_rights(|h, r| cpp_port_dispatcher_create(options, h, r))
        }
    }

    /// Returns `true` if this port was created with `ZX_PORT_BIND_TO_INTERRUPT`.
    pub fn can_bind_to_interrupt(&self) -> bool {
        // SAFETY: `self` is a valid `PortDispatcher` reference.
        unsafe { cpp_port_dispatcher_can_bind_to_interrupt(self) }
    }

    /// Queues `packet` on this port with `timestamp`.
    pub fn queue_interrupt_packet(
        &self,
        packet: &PortInterruptPacket,
        timestamp: zx_instant_boot_t,
    ) -> bool {
        // SAFETY: `self` and `packet` are valid pointers; `PortDispatcher::QueueInterruptPacket`
        // synchronizes access to `packet` using `PortDispatcher::spinlock_`.
        unsafe {
            cpp_port_dispatcher_queue_interrupt_packet(
                self.as_ffi_mut(),
                packet.as_mut_ptr(),
                timestamp,
            )
        }
    }

    /// Removes `packet` from this port if queued, returning `true` if removed.
    pub fn remove_interrupt_packet(&self, packet: &PortInterruptPacket) -> bool {
        // SAFETY: `self` and `packet` are valid pointers; `PortDispatcher::RemoveInterruptPacket`
        // synchronizes access to `packet` using `PortDispatcher::spinlock_`.
        unsafe {
            cpp_port_dispatcher_remove_interrupt_packet(self.as_ffi_mut(), packet.as_mut_ptr())
        }
    }

    /// Queues a user packet on this port.
    #[inline]
    pub fn queue_user(&self, packet: &RawPortPacket) -> Result<(), Status> {
        // SAFETY: `self` and `packet` are valid references, and `RawPortPacket` has the exact same
        // size and alignment as C++ `zx_port_packet_t`.
        let status = unsafe { cpp_port_dispatcher_queue_user(self.as_ffi_mut(), packet) };
        Status::ok(status)
    }

    /// Dequeues a packet from this port, waiting up to `deadline`.
    #[inline]
    pub fn dequeue<'a>(
        &self,
        deadline: &Deadline,
        packet: &'a mut MaybeUninit<RawPortPacket>,
    ) -> Result<&'a mut RawPortPacket, Status> {
        // SAFETY: `self`, `deadline`, and `packet` are valid pointers, and `RawPortPacket` has the
        // exact same size and alignment as C++ `zx_port_packet_t`.
        let status = unsafe { cpp_port_dispatcher_dequeue(self.as_ffi_mut(), deadline, packet) };
        Status::ok(status)?;
        // SAFETY: `cpp_port_dispatcher_dequeue` fully initializes `packet` on `ZX_OK`.
        Ok(unsafe { packet.assume_init_mut() })
    }

    /// Returns `true` if at least one packet matching `handle` and `key` was removed from the
    /// queue.
    #[inline]
    pub fn cancel_queued(&self, handle: HandleRef<'_>, key: u64) -> bool {
        // SAFETY: `self` is a valid `PortDispatcher` reference and `handle` is a valid `HandleRef`
        // protected by the handle table lock.
        unsafe { cpp_port_dispatcher_cancel_queued(self.as_ffi_mut(), handle.as_ptr().cast(), key) }
    }

    /// Cancels queued packets matching the opaque `handle` token and `key`.
    ///
    /// # Safety
    ///
    /// `handle` must be a valid non-null packet handle token (either a `Handle` pointer with the
    /// owning process's handle table lock held, or an allocator-owned token whose packets are
    /// ephemeral).
    #[inline]
    pub unsafe fn cancel_queued_handle(
        &self,
        handle: core::ptr::NonNull<core::ffi::c_void>,
        key: u64,
    ) -> bool {
        // SAFETY: Guaranteed by caller.
        unsafe { cpp_port_dispatcher_cancel_queued(self.as_ffi_mut(), handle.as_ptr(), key) }
    }

    /// Queues an ephemeral or allocator-backed `PortPacket` on this port.
    ///
    /// # Safety
    ///
    /// `packet` must be a valid `PortPacket` allocated from a `PortAllocator` that outlives any
    /// queued packets (or cancels them before destruction). On `Ok(())`, ownership of `packet` is
    /// transferred to the port; on `Err(...)`, the caller retains ownership of `packet`.
    #[inline]
    pub unsafe fn queue(&self, packet: core::ptr::NonNull<PortPacket>) -> Result<(), Status> {
        // SAFETY: Guaranteed by caller.
        let status = unsafe { cpp_port_dispatcher_queue(self.as_ffi_mut(), packet.as_ptr()) };
        Status::ok(status)
    }

    /// Cancels all pending waits and packets registered with `key`.
    #[inline]
    pub fn cancel_key(&self, key: u64) -> Result<(), Status> {
        // SAFETY: `self` is a valid `PortDispatcher` reference.
        let status = unsafe { cpp_port_dispatcher_cancel_key(self.as_ffi_mut(), key) };
        Status::ok(status)
    }
}
