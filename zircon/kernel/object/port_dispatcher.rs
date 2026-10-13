// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::KernelHandle;
use super::dispatcher::impl_dispatcher_facade;
use core::mem::MaybeUninit;
use object_constants_rs as object_constants;
use pin_init::{PinInit, pin_data};
use zr::OpaqueBytes;
use zx_status::Status;
use zx_types::{ZX_OBJ_TYPE_PORT, zx_instant_boot_t, zx_rights_t, zx_status_t};

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
                (self as *const Self).cast_mut(),
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
            cpp_port_dispatcher_remove_interrupt_packet(
                (self as *const Self).cast_mut(),
                packet.as_mut_ptr(),
            )
        }
    }
}
