// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Hypervisor guest trap map and blocking port packet allocator.

use core::convert::Infallible;
use core::ffi::{CStr, c_void};
use core::mem::{align_of, offset_of, size_of};
use core::pin::Pin;
use core::ptr::NonNull;
use fbl::{
    Recyclable, RefPtr, UniquePtr, WavlTree, WavlTreeContainable, WavlTreeKeyable, WavlTreeNode,
};
use ksync::{KCell, KSpinlock, LockClass, RawSpinlock};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use range_check::intersects_offset_len;
use zr::static_assert;
use zx_status::Status;
#[cfg(target_arch = "x86_64")]
use zx_types::ZX_GUEST_TRAP_IO;
use zx_types::{ZX_GUEST_TRAP_BELL, ZX_GUEST_TRAP_MEM, zx_gpaddr_t, zx_port_packet_t};

use crate::kernel::deadline::Deadline;
use crate::kernel::semaphore::Semaphore;
use crate::kernel::spin_tracing::MUTEX_NAME_TRACING_ENABLED;
use crate::object::{PortDispatcher, PortPacket};

use super::ktrace::{VcpuBlockOp, VcpuMeta, ktrace_vcpu};
use super::state_invalidator::StateInvalidator;
use super::trap_map_ffi::{
    cpp_port_packet_arena_delete, cpp_port_packet_arena_destroy, cpp_port_packet_arena_init,
    cpp_port_packet_arena_init_pool, cpp_port_packet_arena_new, cpp_rust_port_allocator_init,
};

const MAX_PACKETS_PER_RANGE: usize = 256;

#[allow(clippy::needless_bool)]
#[cfg_attr(not(target_arch = "x86_64"), allow(unused_variables))]
fn valid_range(kind: u32, addr: zx_gpaddr_t, len: usize) -> bool {
    if len == 0 {
        return false;
    }
    let Some(end) = addr.checked_add(len) else {
        return false;
    };
    #[cfg(target_arch = "x86_64")]
    if kind == ZX_GUEST_TRAP_IO && end > u16::MAX as usize {
        return false;
    }
    true
}

/// C++ `RustPortAllocator` base subobject holding the `PortAllocator` vtable pointer.
#[repr(C, align(8))]
pub(super) struct RustPortAllocator {
    _vptr: usize,
}

static_assert!(size_of::<RustPortAllocator>() == 8);
static_assert!(align_of::<RustPortAllocator>() == 8);

impl RustPortAllocator {
    fn new() -> impl PinInit<Self, Infallible> {
        zr::pin_init_ffi!(cpp_rust_port_allocator_init)
    }
}

const PORT_PACKET_ARENA_SIZE: usize = if MUTEX_NAME_TRACING_ENABLED { 280 } else { 272 };

/// Facade for C++ `fbl::TypedArena<PortPacket, Mutex>`.
#[pin_data(PinnedDrop)]
#[repr(C, align(8))]
pub(super) struct PortPacketArena {
    _storage: zr::OpaqueBytes<PORT_PACKET_ARENA_SIZE>,
}

static_assert!(size_of::<PortPacketArena>() == PORT_PACKET_ARENA_SIZE);
static_assert!(align_of::<PortPacketArena>() == 8);

zr::unsafe_pinned_drop_ffi!(PortPacketArena, cpp_port_packet_arena_destroy);

impl PortPacketArena {
    fn new() -> impl PinInit<Self, Infallible> {
        zr::pin_init_ffi!(cpp_port_packet_arena_init)
    }

    fn init(&self, name: &'static CStr, max_count: usize) -> Result<(), Status> {
        // SAFETY: `self` is a valid initialized `PortPacketArena` and `name` is a valid C string.
        let status = unsafe {
            cpp_port_packet_arena_init_pool(
                core::ptr::from_ref(self).cast_mut(),
                name.as_ptr(),
                max_count,
            )
        };
        Status::ok(status)
    }

    /// Allocates and constructs a `PortPacket` in this arena with `handle` and `allocator`.
    ///
    /// # Safety
    ///
    /// `handle` and `allocator` must remain valid for the lifetime of the allocated `PortPacket`
    /// while it is queued on any `PortDispatcher`, and `allocator` must point to the
    /// `RustPortAllocator` base subobject of a live `BlockingPortAllocator` with provenance over
    /// the full `BlockingPortAllocator`.
    unsafe fn new_packet(
        &self,
        handle: NonNull<c_void>,
        allocator: NonNull<RustPortAllocator>,
    ) -> Option<NonNull<PortPacket>> {
        // SAFETY: `self` is a valid initialized `PortPacketArena` (synchronized internally by its
        // embedded `Mutex`), and the caller guarantees `handle` and `allocator` are valid.
        let pkt = unsafe {
            cpp_port_packet_arena_new(
                core::ptr::from_ref(self).cast_mut(),
                handle.as_ptr(),
                allocator.as_ptr(),
            )
        };
        NonNull::new(pkt)
    }

    /// Destroys and frees `port_packet` back to this arena.
    ///
    /// # Safety
    ///
    /// `port_packet` must have been allocated from this `PortPacketArena` and not yet freed.
    unsafe fn delete(&self, port_packet: NonNull<PortPacket>) {
        // SAFETY: Guaranteed by caller; `PortPacketArena::Delete` is synchronized internally by
        // its embedded `Mutex`.
        unsafe {
            cpp_port_packet_arena_delete(
                core::ptr::from_ref(self).cast_mut(),
                port_packet.as_ptr(),
            );
        }
    }
}

/// Blocks on allocation if the arena is empty.
#[pin_data]
#[repr(C)]
pub struct BlockingPortAllocator {
    #[pin]
    vtable: RustPortAllocator,
    #[pin]
    semaphore: Semaphore,
    #[pin]
    arena: PortPacketArena,
}

static_assert!(offset_of!(BlockingPortAllocator, vtable) == 0);

impl BlockingPortAllocator {
    /// Creates an in-place pinned initializer for `BlockingPortAllocator`.
    pub fn new() -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            vtable <- RustPortAllocator::new(),
            semaphore <- Semaphore::new(MAX_PACKETS_PER_RANGE as i64),
            arena <- PortPacketArena::new(),
        })
    }

    /// Initializes the underlying packet arena.
    pub fn init(&self) -> Result<(), Status> {
        self.arena.init(c"hypervisor-packets", MAX_PACKETS_PER_RANGE)
    }

    /// Waits on the semaphore and allocates a `PortPacket` from the arena.
    pub fn alloc_blocking(&self) -> Option<NonNull<PortPacket>> {
        ktrace_vcpu(VcpuBlockOp::Block, VcpuMeta::Port);
        let result = self.semaphore.wait(&Deadline::infinite());
        ktrace_vcpu(VcpuBlockOp::Unblock, VcpuMeta::Port);
        if result.is_err() {
            return None;
        }
        self.alloc()
    }

    pub(super) fn alloc(&self) -> Option<NonNull<PortPacket>> {
        let this = NonNull::from(self);
        // SAFETY: `this` is a valid `BlockingPortAllocator` that cancels all queued packets in
        // `Trap::drop` before being destroyed. Deriving the `RustPortAllocator` pointer from
        // `NonNull::from(self)` (with `vtable` at offset 0) preserves provenance over the entire
        // `BlockingPortAllocator` when `RustPortAllocator::Free` casts `this` back to
        // `BlockingPortAllocator*`.
        unsafe { self.arena.new_packet(this.cast(), this.cast()) }
    }

    /// Frees `port_packet` back to the arena and posts to the semaphore.
    ///
    /// # Safety
    ///
    /// `port_packet` must have been allocated by this `BlockingPortAllocator` and not yet freed.
    pub unsafe fn free(&self, port_packet: NonNull<PortPacket>) {
        // SAFETY: Guaranteed by caller.
        unsafe {
            self.arena.delete(port_packet);
        }
        self.semaphore.post();
    }

    fn as_handle_ptr(&self) -> NonNull<c_void> {
        NonNull::from(self).cast()
    }
}

/// Describes a single trap within a guest.
#[derive(Recyclable, WavlTreeContainable)]
#[pin_data(PinnedDrop)]
pub struct Trap {
    #[wavl_node]
    wavl_node: WavlTreeNode<Trap>,
    kind: u32,
    addr: zx_gpaddr_t,
    len: usize,
    port: Option<RefPtr<PortDispatcher>>,
    key: u64, // Key for packets in this port range.
    #[pin]
    port_allocator: BlockingPortAllocator,
}

// SAFETY: `Trap` is synchronized internally (`PortDispatcher` is thread-safe and
// `BlockingPortAllocator` synchronizes via `Semaphore` and `PortPacketArena`'s `Mutex`).
unsafe impl Send for Trap {}
// SAFETY: Shared references to `Trap` only read immutable fields or call thread-safe methods on
// `PortDispatcher` and `BlockingPortAllocator`.
unsafe impl Sync for Trap {}

impl WavlTreeKeyable<zx_gpaddr_t> for Trap {
    type Key<'a> = zx_gpaddr_t;

    fn get_key(&self) -> zx_gpaddr_t {
        self.addr
    }
}

#[pinned_drop]
impl PinnedDrop for Trap {
    fn drop(self: Pin<&mut Self>) {
        let Some(port) = self.port.as_ref() else {
            return;
        };
        // `self.port_allocator.as_handle_ptr()` is the allocator token used for ephemeral packets
        // queued by this `Trap`; the port only compares it, never dereferences it.
        let _ = port.cancel_queued_handle(self.port_allocator.as_handle_ptr(), self.key);
    }
}

#[allow(clippy::len_without_is_empty)]
impl Trap {
    /// Creates an in-place pinned initializer for `Trap`.
    pub fn new(
        kind: u32,
        addr: zx_gpaddr_t,
        len: usize,
        port: Option<RefPtr<PortDispatcher>>,
        key: u64,
    ) -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            wavl_node: WavlTreeNode::new(),
            kind,
            addr,
            len,
            port,
            key,
            port_allocator <- BlockingPortAllocator::new(),
        })
    }

    /// Initializes the trap's port allocator.
    pub fn init(&self) -> Result<(), Status> {
        self.port_allocator.init()
    }

    /// Queues `packet` on the trap's port, optionally invalidating `invalidator` first.
    pub fn queue(
        &self,
        packet: &zx_port_packet_t,
        invalidator: Option<&mut StateInvalidator>,
    ) -> Result<(), Status> {
        if let Some(invalidator) = invalidator {
            invalidator.invalidate();
        }
        let Some(port) = self.port.as_ref() else {
            return Err(Status::NOT_FOUND);
        };
        let mut port_packet = self.port_allocator.alloc_blocking().ok_or(Status::NO_MEMORY)?;
        // SAFETY: `port_packet` is uniquely owned by this caller prior to `port.queue`.
        unsafe { port_packet.as_mut() }.set_packet(packet);
        // SAFETY: `port_packet` was allocated from `self.port_allocator`, which cancels all queued
        // packets in `Trap::drop` before `port_allocator` is destroyed.
        let mut status = unsafe { port.queue(port_packet) };
        if let Err(ref mut err) = status {
            // SAFETY: `port.queue` failed, so ownership of `port_packet` remains with the caller.
            unsafe {
                self.port_allocator.free(port_packet);
            }
            if *err == Status::BAD_HANDLE {
                // If the last handle to the port has been closed, then we're in a bad state.
                *err = Status::BAD_STATE;
            }
        }
        status
    }

    /// Returns `true` if `val` falls within `[addr, addr + len)`.
    #[inline]
    pub fn contains(&self, val: zx_gpaddr_t) -> bool {
        val >= self.addr && val < self.addr + self.len
    }

    /// Returns `true` if this trap has an associated port.
    #[inline]
    pub fn has_port(&self) -> bool {
        self.port.is_some()
    }

    /// Returns the trap kind (`ZX_GUEST_TRAP_*`).
    #[inline]
    pub fn kind(&self) -> u32 {
        self.kind
    }

    /// Returns the starting address of the trap range.
    #[inline]
    pub fn addr(&self) -> zx_gpaddr_t {
        self.addr
    }

    /// Returns the length of the trap range.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns the port packet key for this trap range.
    #[inline]
    pub fn key(&self) -> u64 {
        self.key
    }
}

type TrapTree = WavlTree<zx_gpaddr_t, UniquePtr<Trap>>;

/// Lock class tag for [`TrapMap`]'s spinlock.
#[derive(Debug, Default, Copy, Clone)]
struct TrapMapLockClass;

ksync::declare_interned_string!(TRAP_MAP_LOCK_STRING_REG, "TrapMap::lock");

#[unsafe(link_section = "rust_lock_classes")]
#[used]
static TRAP_MAP_LOCK_REGISTRATION: ksync::LockClassRegistration =
    ksync::LockClassRegistration::with_flags(
        TRAP_MAP_LOCK_STRING_REG,
        <RawSpinlock as ksync::RawLock>::LOCK_FLAGS,
    );

impl LockClass for TrapMapLockClass {
    const ID: *mut c_void = TRAP_MAP_LOCK_REGISTRATION.get();
}

/// Contains all the traps within a guest.
#[pin_data]
#[repr(C)]
pub struct TrapMap {
    #[pin]
    lock: KSpinlock<TrapMapLockClass>,
    #[pin]
    mem_traps: KCell<TrapTree, TrapMapLockClass>,
    #[cfg(target_arch = "x86_64")]
    #[pin]
    io_traps: KCell<TrapTree, TrapMapLockClass>,
}

static_assert!(offset_of!(TrapMap, lock) == 0);
static_assert!(offset_of!(TrapMap, mem_traps) == size_of::<RawSpinlock>());
#[cfg(target_arch = "x86_64")]
static_assert!(offset_of!(TrapMap, io_traps) == size_of::<RawSpinlock>() + 24);
#[cfg(target_arch = "x86_64")]
static_assert!(size_of::<TrapMap>() == size_of::<RawSpinlock>() + 48);
#[cfg(not(target_arch = "x86_64"))]
static_assert!(size_of::<TrapMap>() == size_of::<RawSpinlock>() + 24);
static_assert!(align_of::<TrapMap>() == 8);

impl TrapMap {
    /// Creates an in-place pinned initializer for `TrapMap`.
    pub fn new() -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            lock <- KSpinlock::init(),
            mem_traps <- KCell::pin_init(TrapTree::new()),
            #[cfg(target_arch = "x86_64")]
            io_traps <- KCell::pin_init(TrapTree::new()),
        })
    }

    /// Inserts a new trap range into the map.
    pub fn insert_trap(
        &self,
        kind: u32,
        addr: zx_gpaddr_t,
        len: usize,
        port: Option<RefPtr<PortDispatcher>>,
        key: u64,
    ) -> Result<(), Status> {
        if !valid_range(kind, addr, len) {
            return Err(Status::OUT_OF_RANGE);
        }
        let traps = self.tree_of(kind).ok_or(Status::INVALID_ARGS)?;

        let uninit = kalloc::Box::<Trap>::try_new_uninit().map_err(|_| Status::NO_MEMORY)?;
        let ptr = kalloc::Box::into_raw(uninit).cast::<Trap>();
        // SAFETY: `ptr` is a valid, uninitialized, properly aligned heap allocation for `Trap`
        // that remains pinned until `UniquePtr` drops it in place via `Recyclable::recycle`, and
        // `UniquePtr::from_raw` adopts ownership of the initialized allocation.
        let range = unsafe {
            let _ = Trap::new(kind, addr, len, port, key).__pinned_init(ptr);
            UniquePtr::from_raw(ptr)
        };
        range.init()?;

        ksync::lock!(let mut guard = self.lock.lock());
        // SAFETY: `guard` holds `self.lock`, which guards `traps`, and we hold exclusive mutable
        // access to its lock token.
        let traps = unsafe { traps.get_mut(guard.as_mut().token_mut()) };
        let mut iter = traps.upper_bound(&addr);
        // If `upper_bound()` does not return `end()`, check if the range intersects.
        if let Some(trap) = iter.get()
            && intersects_offset_len(addr as u64, len as u64, trap.addr() as u64, trap.len() as u64)
        {
            return Err(Status::ALREADY_EXISTS);
        }
        // Decrement the iterator, and check if the next range intersects.
        iter.move_prev();
        if let Some(trap) = iter.get()
            && intersects_offset_len(addr as u64, len as u64, trap.addr() as u64, trap.len() as u64)
        {
            return Err(Status::ALREADY_EXISTS);
        }
        traps.insert(range);
        Ok(())
    }

    /// Finds the trap containing `addr` for the given `kind`.
    pub fn find_trap(&self, kind: u32, addr: zx_gpaddr_t) -> Result<&Trap, Status> {
        let traps = self.tree_of(kind).ok_or(Status::INVALID_ARGS)?;

        let found = {
            ksync::lock!(let mut guard = self.lock.lock());
            // SAFETY: `guard` holds `self.lock`, which guards `traps`, and we hold exclusive
            // mutable access to its lock token.
            let traps = unsafe { traps.get_mut(guard.as_mut().token_mut()) };
            let mut iter = traps.upper_bound(&addr);
            iter.move_prev();
            let Some(trap) = iter.get() else {
                return Err(Status::NOT_FOUND);
            };
            // SAFETY: Traps are never removed from `TrapMap` until `TrapMap` itself is dropped,
            // and `Trap` is never mutated through `&mut Trap` after insertion into the tree, so
            // extending the shared reference lifetime to `&self` is sound.
            unsafe { &*core::ptr::from_ref(trap) }
        };
        if !found.contains(addr) {
            return Err(Status::NOT_FOUND);
        }
        Ok(found)
    }

    fn tree_of(&self, kind: u32) -> Option<&KCell<TrapTree, TrapMapLockClass>> {
        match kind {
            ZX_GUEST_TRAP_BELL | ZX_GUEST_TRAP_MEM => Some(&self.mem_traps),
            #[cfg(target_arch = "x86_64")]
            ZX_GUEST_TRAP_IO => Some(&self.io_traps),
            _ => None,
        }
    }
}

/// Unit tests for `TrapMap`.
#[cfg(ktest)]
#[unittest::suite(name = "trap_map_rust")]
mod tests {
    use pin_init::stack_pin_init;
    use unittest::{expect_err, expect_ok};

    /// Verifies that `TrapMap::insert_trap` rejects intersecting ranges.
    #[test]
    fn trap_map_insert_trap_intersecting() {
        stack_pin_init!(let trap_map = TrapMap::new());
        // Add traps:
        // 1. [10, 19]
        // 2. [20, 29]
        // 3. [35, 5]
        expect_ok!(trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 10, 10, None, 0));
        expect_ok!(trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 20, 10, None, 0));
        expect_ok!(trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 35, 5, None, 0));
        // Trap at [0, 10] intersects with trap 1.
        expect_err!(
            trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 0, 11, None, 0),
            Status::ALREADY_EXISTS
        );
        // Trap at [10, 19] intersects with trap 1.
        expect_err!(
            trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 10, 10, None, 0),
            Status::ALREADY_EXISTS
        );
        // Trap at [11, 18] intersects with trap 1.
        expect_err!(
            trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 11, 8, None, 0),
            Status::ALREADY_EXISTS
        );
        // Trap at [15, 24] intersects with trap 1 and trap 2.
        expect_err!(
            trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 15, 10, None, 0),
            Status::ALREADY_EXISTS
        );
        // Trap at [30, 39] intersects with trap 3.
        expect_err!(
            trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 30, 10, None, 0),
            Status::ALREADY_EXISTS
        );
        // Trap at [36, 40] intersects with trap 3.
        expect_err!(
            trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 36, 5, None, 0),
            Status::ALREADY_EXISTS
        );

        // Add a trap at the beginning.
        expect_ok!(trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 0, 10, None, 0));
        // In the gap.
        expect_ok!(trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 30, 5, None, 0));
        // And at the end.
        expect_ok!(trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 40, 10, None, 0));
    }

    /// Verifies that `TrapMap::insert_trap` rejects out-of-range trap intervals.
    #[test]
    fn trap_map_insert_trap_out_of_range() {
        stack_pin_init!(let trap_map = TrapMap::new());
        expect_err!(trap_map.insert_trap(ZX_GUEST_TRAP_MEM, 0, 0, None, 0), Status::OUT_OF_RANGE);
        expect_err!(
            trap_map.insert_trap(ZX_GUEST_TRAP_MEM, u32::MAX as usize, usize::MAX, None, 0),
            Status::OUT_OF_RANGE
        );
        #[cfg(target_arch = "x86_64")]
        expect_err!(
            trap_map.insert_trap(ZX_GUEST_TRAP_IO, 0, u32::MAX as usize, None, 0),
            Status::OUT_OF_RANGE
        );
    }
}
