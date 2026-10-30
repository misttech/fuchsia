// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod payload_allocator;
use payload_allocator::{NO_ALLOCATION, PayloadAllocator};

mod signal;
use fuchsia_sync::Mutex;
pub use signal::SIG_SHUTDOWN;
use signal::{
    EventSignal, SIG_DATA_AVAILABLE_0, SIG_DATA_AVAILABLE_1, SIG_SPACE_AVAILABLE_0,
    SIG_SPACE_AVAILABLE_1,
};
use std::fmt::{Debug, Formatter};
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use storage_ptr_slice::{MutPtrByteSlice, PtrByteSlice};
use zerocopy::{FromBytes, IntoBytes, KnownLayout};

struct MappedVmo {
    vmo: zx::Vmo,
    addr: usize,
    size: usize,
}

impl MappedVmo {
    fn new(vmo: zx::Vmo) -> Result<Self, zx::Status> {
        let size = vmo.get_size()? as usize;
        let addr = fuchsia_runtime::vmar_root_self().map(
            0,
            &vmo,
            0,
            size,
            zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
        )?;
        Ok(Self { vmo, addr, size })
    }

    fn vmo(&self) -> &zx::Vmo {
        &self.vmo
    }

    fn addr(&self) -> usize {
        self.addr
    }

    fn size(&self) -> usize {
        self.size
    }
}

impl Drop for MappedVmo {
    fn drop(&mut self) {
        // SAFETY: We are dropping the owner of the mapping, invalidating the address range.
        unsafe {
            // Unmap the VMO from the VMAR.
            let _ = fuchsia_runtime::vmar_root_self().unmap(self.addr, self.size);
        }
    }
}

// A 64-bit word that packs a 63-bit sequential index in bits 0..62 and a waiter flag in bit 63.
#[repr(transparent)]
#[derive(Copy, Clone)]
struct State(u64);

impl State {
    // The most significant bit. When set, indicates a peer is blocked waiting on this head.
    const HAS_WAITER: u64 = 1 << 63;
    // The lower 63 bits which holds the index of the next item to be written/read.
    const INDEX_MASK: u64 = !Self::HAS_WAITER;

    fn index(self) -> u64 {
        self.0 & Self::INDEX_MASK
    }

    fn has_waiter(self) -> bool {
        (self.0 & Self::HAS_WAITER) != 0
    }

    fn with_waiter(self) -> State {
        State(self.0 | Self::HAS_WAITER)
    }

    fn clear_waiter_and_increment(self) -> State {
        State(((self.0 & Self::INDEX_MASK) + 1) & Self::INDEX_MASK)
    }
}

// The header structure at the beginning of the VMO in `SharedQueue`.
//
// This metadata is shared between the writer and reader to track the state of the queue.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout)]
struct QueueHeader {
    // Bits 0-62: The index where the writer will write the next item.
    // Bit 63: Set if the reader is asleep and waiting for data.
    write_head: u64,
    // Bits 0-62: The index where the reader will read the next item.
    // Bit 63: Set if the writer is asleep and waiting for space.
    read_head: u64,
}

// Offset in bytes from the start of the VMO where the slots region begins.
// We allocate 64 bytes for the header to align the slots to a typical cache line boundary.
const HEADER_SIZE: usize = 64;

// A generic VMO-backed FIFO queue.
//
// **Note**: This queue is Single-Producer Single-Consumer.
//
// The VMO is conceptually divided into three regions:
// 1. **Header**: Contains the read and write indices (and peer asleep flags).
// 2. **Slots**: An array of `capacity` items of type `T`.
// 3. **Payload Region**: Any remaining space in the VMO after the slots. This region is
//     page-aligned and can be used to store data referenced by the queue entries.
struct SharedQueue<T> {
    mapping: MappedVmo,
    header: *mut QueueHeader,
    // Pointer to the start of the data region where queue items are stored.
    slots: *mut T,
    // Byte offset from the start of the VMO where the page-aligned payload region begins.
    payload_region_offset: usize,
    // The maximum number of items the queue can hold.
    // Must be a power of two: The queue index resets to zero at 2^63. The capacity must be a power
    // of two so that the overflow reset aligns with the modulo arithmetic boundary used for slot
    // calculation (`index & (capacity - 1)`).
    capacity: u32,

    _phantom: std::marker::PhantomData<T>,
}

// SAFETY: `SharedQueue` contains raw pointers to the VMO which strips `Send`/`Sync` by default. It
// is safe to transfer ownership (`Send`) because concurrent manipulations of the pointers are
// strictly synchronized via `AtomicU64`. It is safe to share by reference (`Sync`) because it
// exposes no `&self` methods that allow non-atomic mutation. We propagate the traits to `T` to
// ensure the underlying payload is not structurally thread-unsafe.
unsafe impl<T: Send> Send for SharedQueue<T> {}
unsafe impl<T: Sync> Sync for SharedQueue<T> {}

impl<T> SharedQueue<T>
where
    T: FromBytes + IntoBytes + KnownLayout + Copy,
{
    // Creates a new `SharedQueue` backed by the provided `vmo` with the given `capacity`.
    //
    // `capacity` specifies the maximum number of items the queue can hold. It must be a power of
    // two to ensure correct index wrap-around behavior.
    fn new(vmo: zx::Vmo, capacity: u32) -> Result<Self, zx::Status> {
        // Capacity must be a power of two to ensure smooth index wrap-around.
        if !capacity.is_power_of_two() {
            return Err(zx::Status::INVALID_ARGS);
        }

        // Map the VMO to allow direct memory access to the queue header and slots.
        let mapping = MappedVmo::new(vmo)?;

        // Ensure the VMO is large enough.
        let required_size = (capacity as usize)
            .checked_mul(std::mem::size_of::<T>())
            .and_then(|val| val.checked_add(HEADER_SIZE))
            .ok_or(zx::Status::INVALID_ARGS)?;
        if mapping.size < required_size {
            return Err(zx::Status::BUFFER_TOO_SMALL);
        }

        let header = mapping.addr as *mut QueueHeader;
        let slots = (mapping.addr + HEADER_SIZE) as *mut T;
        let page_size = zx::system_get_page_size() as usize;
        let payload_region_offset = (required_size + page_size - 1) & !(page_size - 1);

        Ok(Self {
            mapping,
            header,
            slots,
            payload_region_offset,
            capacity,
            _phantom: std::marker::PhantomData,
        })
    }

    fn vmo(&self) -> &zx::Vmo {
        self.mapping.vmo()
    }

    fn capacity(&self) -> u32 {
        self.capacity
    }

    fn addr(&self) -> usize {
        self.mapping.addr()
    }

    fn vmo_size(&self) -> usize {
        self.mapping.size()
    }

    // Returns the byte offset where the payload region starts (the region of the VMO immediately
    // following the queue entries region, rounded up to the nearest page).
    fn payload_region_offset(&self) -> usize {
        self.payload_region_offset
    }

    fn write_head_atomic(&self) -> &AtomicU64 {
        // SAFETY: `self.header` points to a valid memory-mapped VMO region of at least HEADER_SIZE
        // bytes. We use `addr_of_mut!` and `AtomicU64::from_ptr` to avoid creating a Rust reference
        // (`&QueueHeader`) to the entire struct in untrusted shared memory, preventing undefined
        // behavior from concurrent modifications or aliasing violations.
        unsafe {
            let ptr = std::ptr::addr_of_mut!((*self.header).write_head);
            AtomicU64::from_ptr(ptr)
        }
    }

    fn load_write_head(&self, ordering: Ordering) -> State {
        State(self.write_head_atomic().load(ordering))
    }

    fn write_index(&self) -> u64 {
        self.load_write_head(Ordering::Acquire).index()
    }

    fn read_index(&self) -> u64 {
        self.load_read_head(Ordering::Acquire).index()
    }

    fn is_full_at(&self, write_idx: u64, read_idx: u64) -> bool {
        (write_idx.wrapping_sub(read_idx) & State::INDEX_MASK) >= self.capacity as u64
    }

    fn is_full(&self) -> bool {
        self.is_full_at(self.write_index(), self.read_index())
    }

    fn is_empty(&self) -> bool {
        self.load_write_head(Ordering::Acquire).index()
            == self.load_read_head(Ordering::Acquire).index()
    }

    fn get_slot_ptr(&self, index: u64) -> *mut T {
        let slot_idx = (index & (self.capacity as u64 - 1)) as isize;
        // SAFETY: `slot_idx` is masked by `capacity - 1`, guaranteeing it stays within the
        // validated memory bounds of `self.slots`.
        unsafe { self.slots.offset(slot_idx) }
    }
}

impl<T> SharedQueue<T> {
    fn read_head_atomic(&self) -> &AtomicU64 {
        // SAFETY: `self.header` points to a valid memory-mapped VMO region of at least HEADER_SIZE.
        unsafe {
            let ptr = std::ptr::addr_of_mut!((*self.header).read_head);
            AtomicU64::from_ptr(ptr)
        }
    }

    fn load_read_head(&self, ordering: Ordering) -> State {
        State(self.read_head_atomic().load(ordering))
    }

    // Sets `HAS_WAITER` on `read_head` if it still equals `curr_read_head`, returning `true` if
    // the waiter bit was armed or `false` if the consumer advanced `read_head` concurrently.
    fn try_set_waiter(&self, curr_read_head: State) -> bool {
        let next_read_head = curr_read_head.with_waiter();
        self.read_head_atomic()
            .compare_exchange_weak(
                curr_read_head.0,
                next_read_head.0,
                // `Ordering::Relaxed`: We are just publishing a sleep signal.
                Ordering::Relaxed,
                // `Ordering::Acquire`: exchange failed (reader likely popped an item).
                Ordering::Acquire,
            )
            .is_ok()
    }

    // Returns a mutable pointer slice for `[offset .. offset + size)` in the payload region.
    //
    // # Safety
    //
    // `offset` must have been returned by `PayloadAllocator::allocate` for at least `size` bytes,
    // and the caller must bound `'a` to the lifetime of the reservation keeping `self` alive.
    unsafe fn payload_slice_mut<'a>(&self, offset: u32, size: usize) -> MutPtrByteSlice<'a> {
        let rel_offset = if offset == NO_ALLOCATION { 0 } else { offset as usize };
        // SAFETY:
        // 1. `self.addr()` is a valid base pointer to a mapped VMO memory space active for the
        //    entire lifetime of this instance.
        // 2. `PayloadAllocator` guarantees that `rel_offset + size` fits within the allocated
        //    payload boundaries, preventing out-of-bounds pointer derivation.
        // 3. Returning a `MutPtrByteSlice` wrapper instead of a native `&mut [u8]` avoids violating
        //    Rust's strict aliasing rules for shared memory, thereby preventing Undefined Behavior.
        unsafe {
            let abs_offset = self.payload_region_offset + rel_offset;
            let dest_ptr = (self.mapping.addr() + abs_offset) as *mut u8;
            MutPtrByteSlice::new(std::ptr::slice_from_raw_parts_mut(dest_ptr, size))
        }
    }

    // Trigger shutdown, waking up all waiters.
    fn shutdown(&self) -> Result<(), zx::Status> {
        self.mapping.vmo().signal(zx::Signals::empty(), SIG_SHUTDOWN)?;
        Ok(())
    }
}

// Shared state and queue/allocator operations used by both `SyncSender` and `AsyncSender`.
struct SenderState {
    allocator: PayloadAllocator,
    // Used by the consumer to wait for and by the producer to signal that the queue is non-empty.
    data_available: EventSignal,
    // Used by the producer to wait for and by the consumer to signal that space is available.
    space_available: EventSignal,
}

impl SenderState {
    fn new(payload_size: usize, capacity: usize, alignment: usize) -> Result<Self, zx::Status> {
        if alignment == 0 || !alignment.is_power_of_two() || payload_size < alignment {
            return Err(zx::Status::INVALID_ARGS);
        }
        Ok(Self {
            allocator: PayloadAllocator::new(payload_size, capacity, alignment),
            data_available: EventSignal::new(SIG_DATA_AVAILABLE_0, SIG_DATA_AVAILABLE_1),
            space_available: EventSignal::new(SIG_SPACE_AVAILABLE_0, SIG_SPACE_AVAILABLE_1),
        })
    }

    fn try_allocate<T: FromBytes + IntoBytes + KnownLayout + Copy>(
        &mut self,
        queue: &SharedQueue<T>,
        size: usize,
        headroom_bytes: usize,
    ) -> (Option<u32>, State) {
        // `Ordering::Acquire`: ensures consumer reads of retired payload ranges have completed
        // before those blocks are reclaimed and overwritten.
        let curr_read_head = queue.load_read_head(Ordering::Acquire);
        self.allocator.reclaim_consumed_slots(curr_read_head.index());
        (self.allocator.allocate(size, headroom_bytes), curr_read_head)
    }

    fn try_push_with_payload<T: FromBytes + IntoBytes + KnownLayout + Copy>(
        &mut self,
        queue: &SharedQueue<T>,
        msg: T,
        payload_offset: Option<u32>,
    ) -> Result<Result<u64, State>, zx::Status> {
        let mut curr_write_head = queue.load_write_head(Ordering::Relaxed);
        // `Ordering::Acquire`: guarantees the consumer finished reading the previous item in this
        // slot before we overwrite it.
        let curr_read_head = queue.load_read_head(Ordering::Acquire);
        self.allocator.reclaim_consumed_slots(curr_read_head.index());

        if queue.is_full_at(curr_write_head.index(), curr_read_head.index()) {
            return Ok(Err(curr_read_head));
        }

        let write_idx = curr_write_head.index();
        let slot_ptr = queue.get_slot_ptr(write_idx);
        // SAFETY: `slot_ptr` is within the mapped VMO slots region, callers serialize access to
        // `SenderState` so we exclusively own `write_idx`, and `T: IntoBytes` ensures all bytes of
        // `msg` are initialized.
        unsafe {
            std::ptr::write(slot_ptr, msg);
        }
        if let Some(offset) = payload_offset {
            self.allocator.commit_to_slot(write_idx, offset);
        }

        loop {
            let next_write_head = curr_write_head.clear_waiter_and_increment();
            match queue.write_head_atomic().compare_exchange_weak(
                curr_write_head.0,
                next_write_head.0,
                // `Ordering::Release`: publishes both payload writes and slot writes before the
                // consumer observes the incremented `write_head`.
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(prev_write_head) => {
                    if State(prev_write_head).has_waiter() {
                        let vmo = queue.mapping.vmo();
                        self.data_available.signal(vmo)?;
                    }
                    return Ok(Ok(write_idx));
                }
                Err(actual) => curr_write_head = State(actual),
            }
        }
    }

    // Releases an uncommitted payload allocation or attaches it to `last_committed_slot` if a
    // prefix was already committed and has not yet been retired by the consumer. Returns `true` if
    // the block was freed back to `self.allocator` immediately.
    fn release_uncommitted_payload<T: FromBytes + IntoBytes + KnownLayout + Copy>(
        &mut self,
        queue: &SharedQueue<T>,
        base_offset: u32,
        last_committed_slot: Option<u64>,
    ) -> bool {
        if let Some(last_slot) = last_committed_slot {
            if !self.allocator.is_slot_reclaimed(last_slot) {
                let curr_read_head = queue.load_read_head(Ordering::Acquire);
                self.allocator.reclaim_consumed_slots(curr_read_head.index());
            }
            if !self.allocator.is_slot_reclaimed(last_slot) {
                // A prefix of this buddy block was already pushed to `last_slot` and has not yet
                // been retired by the consumer; attach `base_offset` to `last_slot` so the buddy
                // block is freed once `read_head` advances past `last_slot`. Note that
                // `Receiver::retire_slot` advances `read_head` monotonically across contiguous
                // completed slots, so `read_head` cannot pass `last_slot` until `last_slot` and all
                // earlier prefix slots from this reservation have completed.
                self.allocator.commit_to_slot(last_slot, base_offset);
                return false;
            }
        }
        self.allocator.free(base_offset);
        true
    }
}

const PORT_KEY_SIGNAL: u64 = 1;
const PORT_KEY_WAKE: u64 = 2;

struct SyncSenderState {
    inner: SenderState,
    wait_async_armed: bool,
    shutting_down: bool,
    sleeping_writers: usize,
    pending_wakes: usize,
}

/// The synchronous sender endpoint for a VMO-backed FIFO queue, supporting both single-producer
/// borrowed reservations ([`SyncSender::reserve_payload`]) and concurrent multi-producer owned
/// reservations ([`SyncSender::reserve_owned_payload`]), out-of-order commits, and sub-range prefix
/// commits.
pub struct SyncSender<T> {
    inner: SharedQueue<T>,
    alignment: usize,
    payload_capacity: usize,
    max_allocation_size: usize,
    port: zx::Port,
    state: Mutex<SyncSenderState>,
}

impl<T> Debug for SyncSender<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncSender")
            .field("capacity", &self.inner.capacity)
            .field("alignment", &self.alignment)
            .field("payload_capacity", &self.payload_capacity)
            .field("max_allocation_size", &self.max_allocation_size)
            .finish()
    }
}

impl<T: FromBytes + IntoBytes + KnownLayout + Copy> SyncSender<T> {
    /// Creates a new `SyncSender`.
    ///
    /// * `vmo` - The shared VMO used for queue messages and payload buffers.
    /// * `alignment` - Power-of-two byte alignment (and minimum buddy block size) for payload
    ///                 allocations.
    /// * `queue_capacity` - Power-of-two maximum number of queue slots.
    pub fn new(vmo: zx::Vmo, alignment: usize, queue_capacity: u32) -> Result<Self, zx::Status> {
        let inner = SharedQueue::new(vmo, queue_capacity)?;
        let raw_payload_size = inner.vmo_size().saturating_sub(inner.payload_region_offset());
        let inner_state = SenderState::new(raw_payload_size, inner.capacity() as usize, alignment)?;
        let payload_capacity = inner_state.allocator.payload_capacity();
        let max_allocation_size = inner_state.allocator.max_allocation_size();
        let port = zx::Port::create();
        let state = SyncSenderState {
            inner: inner_state,
            wait_async_armed: false,
            shutting_down: false,
            sleeping_writers: 0,
            pending_wakes: 0,
        };
        Ok(Self {
            inner,
            alignment,
            payload_capacity,
            max_allocation_size,
            port,
            state: Mutex::new(state),
        })
    }

    /// Returns the maximum number of items the queue can hold.
    pub fn capacity(&self) -> u32 {
        self.inner.capacity()
    }

    /// Returns the total usable byte capacity of the aligned payload region.
    pub fn payload_capacity(&self) -> usize {
        self.payload_capacity
    }

    /// Returns the maximum contiguous allocation size in bytes supported by this payload region.
    pub fn max_allocation_size(&self) -> usize {
        self.max_allocation_size
    }

    pub fn is_full(&self) -> bool {
        self.inner.is_full()
    }

    pub fn index(&self) -> u64 {
        self.inner.write_index()
    }

    /// Returns a reference to the backing VMO.
    pub fn vmo(&self) -> &zx::Vmo {
        self.inner.vmo()
    }

    /// Pushes a standalone message onto the queue without an associated payload range, blocking if
    /// all queue slots are currently occupied.
    pub fn push(&self, msg: T) -> Result<u64, zx::Status> {
        self.push_with_payload(msg, None)
    }

    /// Reserves capacity in the payload region for a payload of `size` bytes.
    ///
    /// Returns a [`PayloadBuffer`] referencing the payload region so that data can be copied or
    /// written directly into it. Dropping the buffer without committing automatically releases the
    /// reservation.
    pub fn reserve_payload(
        &mut self,
        size: usize,
    ) -> Result<PayloadBuffer<T, &mut Self>, zx::Status> {
        let aligned_len =
            size.checked_next_multiple_of(self.alignment).ok_or(zx::Status::NO_MEMORY)?;
        let (base_offset, current_offset, slice) = self.allocate_payload(aligned_len, size, 0)?;
        Ok(PayloadBuffer {
            sender: self,
            last_committed_slot: None,
            remaining_len: size,
            slice,
            base_offset,
            current_offset,
            owns_allocation: base_offset != NO_ALLOCATION,
        })
    }

    /// Reserves `size` bytes (rounded up to `alignment`) in the payload region while leaving at
    /// least `headroom_bytes` unallocated for higher-priority reservations (such as metadata
    /// registration).
    ///
    /// Blocks without holding internal locks if space is temporarily unavailable, allowing
    /// already-reserved [`OwnedPayloadBuffer`]s on other threads to commit or drop concurrently.
    pub fn reserve_owned_payload(
        self: &Arc<Self>,
        size: usize,
        headroom_bytes: usize,
    ) -> Result<OwnedPayloadBuffer<T>, zx::Status> {
        let aligned_len =
            size.checked_next_multiple_of(self.alignment).ok_or(zx::Status::NO_MEMORY)?;
        let (base_offset, current_offset, slice) =
            self.allocate_payload(aligned_len, aligned_len, headroom_bytes)?;
        Ok(PayloadBuffer {
            sender: Arc::clone(self),
            last_committed_slot: None,
            remaining_len: aligned_len,
            slice,
            base_offset,
            current_offset,
            owns_allocation: base_offset != NO_ALLOCATION,
        })
    }

    fn allocate_payload(
        &self,
        aligned_len: usize,
        slice_len: usize,
        headroom_bytes: usize,
    ) -> Result<(u32, u32, MutPtrByteSlice<'static>), zx::Status> {
        if aligned_len > self.max_allocation_size {
            return Err(zx::Status::NO_MEMORY);
        }
        let mut state = self.state.lock();
        if !state.inner.allocator.is_within_capacity(aligned_len, headroom_bytes) {
            return Err(zx::Status::NO_MEMORY);
        }

        let base_offset = loop {
            match state.inner.try_allocate(&self.inner, aligned_len, headroom_bytes) {
                (Some(offset), _) => break offset,
                (None, curr_read_head) => {
                    state = self.wait_for_space_locked(state, curr_read_head)?;
                }
            }
        };
        drop(state);

        let current_offset = if base_offset == NO_ALLOCATION { 0 } else { base_offset };
        // SAFETY: `base_offset` was allocated by `try_allocate` for `aligned_len >= slice_len`
        // bytes, and the `PayloadBuffer` returned by the caller holds either `&mut SyncSender` or
        // `Arc<SyncSender>`, keeping the VMAR mapping alive for the buffer's entire lifetime.
        let slice = unsafe { self.inner.payload_slice_mut(base_offset, slice_len) };
        Ok((base_offset, current_offset, slice))
    }

    fn push_with_payload(&self, msg: T, payload_offset: Option<u32>) -> Result<u64, zx::Status> {
        let mut state = self.state.lock();
        loop {
            match state.inner.try_push_with_payload(&self.inner, msg, payload_offset) {
                Ok(Ok(slot)) => return Ok(slot),
                Err(_) => {
                    // `data_available.signal` failed only if the VMO handle is invalid; the item is
                    // already published in `write_head`.
                    return Ok(self.inner.write_index().wrapping_sub(1) & State::INDEX_MASK);
                }
                Ok(Err(curr_read_head)) => {
                    state = self.wait_for_space_locked(state, curr_read_head)?;
                }
            }
        }
    }

    // Arms `HAS_WAITER` + `vmo.wait_async` if not yet armed, drops `state` while waiting on
    // `self.port`, and returns the re-acquired lock guard once woken (or immediately if `read_head`
    // raced ahead before arming).
    fn wait_for_space_locked<'a>(
        &'a self,
        mut state: fuchsia_sync::MutexGuard<'a, SyncSenderState>,
        curr_read_head: State,
    ) -> Result<fuchsia_sync::MutexGuard<'a, SyncSenderState>, zx::Status> {
        if state.shutting_down {
            return Err(zx::Status::CANCELED);
        }

        if !state.wait_async_armed {
            if !self.inner.try_set_waiter(curr_read_head) {
                return Ok(state);
            }

            let vmo = self.inner.mapping.vmo();
            let mask = state.inner.space_available.current_mask(SIG_SHUTDOWN);
            vmo.wait_async(&self.port, PORT_KEY_SIGNAL, mask, zx::WaitAsyncOpts::empty())?;
            state.wait_async_armed = true;
        }

        state.sleeping_writers += 1;
        drop(state);

        let wait_res = self.port.wait(zx::MonotonicInstant::INFINITE);
        state = self.state.lock();
        state.sleeping_writers -= 1;
        let packet = wait_res?;

        if packet.key() == PORT_KEY_WAKE {
            state.pending_wakes = state.pending_wakes.saturating_sub(1);
        } else if packet.key() == PORT_KEY_SIGNAL {
            state.wait_async_armed = false;
            if let zx::PacketContents::SignalOne(signals) = packet.contents() {
                let observed = signals.observed();
                if observed.contains(state.inner.space_available.current_mask(zx::Signals::empty()))
                {
                    state.inner.space_available.advance();
                }
                if observed.contains(SIG_SHUTDOWN) {
                    state.shutting_down = true;
                }
            }
            self.wake_sleeping_writers(&mut state);
        }

        if state.shutting_down {
            return Err(zx::Status::CANCELED);
        }
        Ok(state)
    }

    fn release_uncommitted_payload(&self, base_offset: u32, last_committed_slot: Option<u64>) {
        let mut state = self.state.lock();
        if state.inner.release_uncommitted_payload(&self.inner, base_offset, last_committed_slot) {
            self.wake_sleeping_writers(&mut state);
        }
    }
}

impl<T> SyncSender<T> {
    /// Signals shutdown on the underlying VMO and wakes any blocked writers.
    pub fn shutdown(&self) -> Result<(), zx::Status> {
        let mut state = self.state.lock();
        state.shutting_down = true;
        self.wake_sleeping_writers(&mut state);
        drop(state);
        self.inner.shutdown()
    }

    fn wake_sleeping_writers(&self, state: &mut SyncSenderState) {
        let to_wake = state.sleeping_writers.saturating_sub(state.pending_wakes);
        for _ in 0..to_wake {
            if self
                .port
                .queue(&zx::Packet::from_user_packet(
                    PORT_KEY_WAKE,
                    0,
                    zx::UserPacket::from_u8_array([0; 32]),
                ))
                .is_ok()
            {
                state.pending_wakes += 1;
            }
        }
    }
}

impl<T> Drop for SyncSender<T> {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// The asynchronous sender endpoint for a VMO-backed FIFO queue.
pub struct AsyncSender<T> {
    inner: SharedQueue<T>,
    state: SenderState,
}

impl<T: FromBytes + IntoBytes + KnownLayout + Copy> AsyncSender<T> {
    /// Creates a new `AsyncSender`.
    ///
    /// * `vmo` - The shared VMO used for queue messages and payload buffers.
    /// * `alignment` - Power-of-two byte alignment (and minimum buddy block size) for payload
    ///                 allocations.
    /// * `queue_capacity` - The maximum number of queue slots.
    pub fn new(vmo: zx::Vmo, alignment: usize, queue_capacity: u32) -> Result<Self, zx::Status> {
        let inner = SharedQueue::new(vmo, queue_capacity)?;
        let payload_size = inner.vmo_size().saturating_sub(inner.payload_region_offset());
        let state = SenderState::new(payload_size, inner.capacity() as usize, alignment)?;
        Ok(Self { inner, state })
    }

    /// Returns the maximum number of items the queue can hold.
    pub fn capacity(&self) -> u32 {
        self.inner.capacity()
    }

    pub fn is_full(&self) -> bool {
        self.inner.is_full()
    }

    pub fn vmo(&self) -> &zx::Vmo {
        self.inner.vmo()
    }

    /// Pushes a standalone message onto the queue, yielding to the executor if the queue is full.
    pub async fn push(&mut self, msg: T) -> Result<u64, zx::Status> {
        self.push_with_payload(msg, None).await
    }

    async fn push_with_payload(
        &mut self,
        msg: T,
        payload_offset: Option<u32>,
    ) -> Result<u64, zx::Status> {
        loop {
            match self.state.try_push_with_payload(&self.inner, msg, payload_offset)? {
                Ok(slot) => return Ok(slot),
                Err(curr_read_head) => {
                    if self.inner.try_set_waiter(curr_read_head) {
                        let vmo = self.inner.mapping.vmo();
                        self.state.space_available.wait_async(vmo, SIG_SHUTDOWN).await?;
                    }
                }
            }
        }
    }

    /// Reserves a payload buffer in the VMO. Yields to the executor if there is insufficient
    /// payload capacity available. Returns an `AsyncPayloadBuffer` that commits on success
    /// or rolls back on drop.
    pub async fn reserve_payload(
        &mut self,
        size: usize,
    ) -> Result<AsyncPayloadBuffer<'_, T>, zx::Status> {
        if !self.state.allocator.is_within_capacity(size, 0) {
            return Err(zx::Status::NO_MEMORY);
        }
        let offset = loop {
            match self.state.try_allocate(&self.inner, size, 0) {
                (Some(offset), _) => break offset,
                (None, curr_read_head) => {
                    if self.inner.try_set_waiter(curr_read_head) {
                        let vmo = self.inner.mapping.vmo();
                        self.state.space_available.wait_async(vmo, SIG_SHUTDOWN).await?;
                    }
                }
            }
        };

        // SAFETY: `offset` was allocated by `try_allocate` for `size` bytes.
        let slice = unsafe { self.inner.payload_slice_mut(offset, size) };
        Ok(AsyncPayloadBuffer { sender: self, offset, slice })
    }
}

impl<T> Drop for AsyncSender<T> {
    fn drop(&mut self) {
        let _ = self.inner.shutdown();
    }
}

/// An asynchronous RAII reservation in the payload VMO of an [`AsyncSender`] that yields when
/// committing if the queue is full. Dropping this buffer automatically rolls back the payload
/// allocation if not committed.
pub struct AsyncPayloadBuffer<'a, T: FromBytes + IntoBytes + KnownLayout + Copy> {
    sender: &'a mut AsyncSender<T>,
    offset: u32,
    slice: MutPtrByteSlice<'a>,
}

impl<'a, T: FromBytes + IntoBytes + KnownLayout + Copy> AsyncPayloadBuffer<'a, T> {
    /// Returns a mutable slice to the reserved payload bytes.
    pub fn data(&mut self) -> &mut MutPtrByteSlice<'a> {
        &mut self.slice
    }

    /// Returns the byte offset of the payload within the VMO's payload region.
    pub fn offset(&self) -> u32 {
        if self.offset == NO_ALLOCATION { 0 } else { self.offset }
    }

    /// Pushes the given message to the FIFO and commits this payload allocation, linking the
    /// payload to the queue slot. Yields if the queue is full.
    pub async fn commit(mut self, msg: T) -> Result<(), zx::Status> {
        let offset = std::mem::replace(&mut self.offset, NO_ALLOCATION);
        let payload_offset = (offset != NO_ALLOCATION).then_some(offset);
        if let Err(e) = self.sender.push_with_payload(msg, payload_offset).await {
            self.sender.state.allocator.free(offset);
            return Err(e);
        }
        Ok(())
    }
}

impl<'a, T: FromBytes + IntoBytes + KnownLayout + Copy> Drop for AsyncPayloadBuffer<'a, T> {
    fn drop(&mut self) {
        self.sender.state.allocator.free(self.offset);
    }
}

/// An RAII reservation in the payload VMO of a [`SyncSender`].
///
/// Supports both borrowed reservations (`S = &mut SyncSender<T>`, returned by
/// [`SyncSender::reserve_payload`]) and owned `Arc`-backed reservations
/// ([`OwnedPayloadBuffer<T>`], returned by [`SyncSender::reserve_owned_payload`]), as well as
/// splitting sub-range prefixes across multiple FIFO messages via
/// [`PayloadBuffer::commit_prefix`]. Any uncommitted allocation remaining when `PayloadBuffer` is
/// dropped is automatically released.
pub struct PayloadBuffer<
    T: FromBytes + IntoBytes + KnownLayout + Copy,
    S: Deref<Target = SyncSender<T>> = Arc<SyncSender<T>>,
> {
    sender: S,
    last_committed_slot: Option<u64>,
    remaining_len: usize,
    slice: MutPtrByteSlice<'static>,
    base_offset: u32,
    current_offset: u32,
    owns_allocation: bool,
}

/// An owned `Arc`-backed [`PayloadBuffer`] that does not borrow or lock the [`SyncSender`] while
/// data is being read, pre-committed, or decompressed into the VMO.
pub type OwnedPayloadBuffer<T> = PayloadBuffer<T, Arc<SyncSender<T>>>;

// SAFETY: `PayloadBuffer` holds `sender: S` keeping the VMAR mapping alive for its entire lifetime
// and exclusively owns the disjoint payload sub-range
// `[current_offset .. current_offset + remaining_len)`. Mutable access to `slice` is only exposed
// through `&mut self`, while `&self` methods only read scalar metadata.
unsafe impl<
    T: FromBytes + IntoBytes + KnownLayout + Copy + Send + Sync,
    S: Deref<Target = SyncSender<T>> + Send,
> Send for PayloadBuffer<T, S>
{
}
unsafe impl<
    T: FromBytes + IntoBytes + KnownLayout + Copy + Send + Sync,
    S: Deref<Target = SyncSender<T>> + Sync,
> Sync for PayloadBuffer<T, S>
{
}

impl<T: FromBytes + IntoBytes + KnownLayout + Copy, S: Deref<Target = SyncSender<T>>> Debug
    for PayloadBuffer<T, S>
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PayloadBuffer")
            .field("offset", &self.current_offset)
            .field("remaining_len", &self.remaining_len)
            .finish()
    }
}

impl<T: FromBytes + IntoBytes + KnownLayout + Copy> OwnedPayloadBuffer<T> {
    /// Returns a reference to the owning [`SyncSender`].
    pub fn sender(&self) -> &Arc<SyncSender<T>> {
        &self.sender
    }
}

impl<T: FromBytes + IntoBytes + KnownLayout + Copy, S: Deref<Target = SyncSender<T>>>
    PayloadBuffer<T, S>
{
    /// Returns the physical byte offset within the payload region for the uncommitted range.
    pub fn offset(&self) -> u32 {
        self.current_offset
    }

    /// Returns the remaining uncommitted byte length of this reservation.
    pub fn remaining_len(&self) -> usize {
        self.remaining_len
    }

    /// Returns the process virtual address of the start of the remaining uncommitted reservation.
    pub fn vaddr(&self) -> usize {
        self.slice.as_mut_ptr() as usize
    }

    /// Returns a mutable reference to the pointer slice covering the remaining uncommitted bytes.
    pub fn data(&mut self) -> &mut MutPtrByteSlice<'static> {
        &mut self.slice
    }

    /// Returns a reborrowed mutable pointer slice covering the remaining uncommitted bytes.
    pub fn data_mut(&mut self) -> MutPtrByteSlice<'_> {
        self.slice.reborrow()
    }

    /// Commits the leading `aligned_len` bytes of this reservation alongside `msg`, advancing the
    /// reservation's offset and remaining slice to the uncommitted suffix.
    ///
    /// # Panics
    ///
    /// Panics if `aligned_len > self.remaining_len()` or if `aligned_len` is not a multiple of the
    /// sender's alignment (unless committing the entire remaining buffer via [`Self::commit`]).
    pub fn commit_prefix(&mut self, aligned_len: usize, msg: T) -> Result<(), zx::Status> {
        assert!(
            aligned_len <= self.remaining_len,
            "commit_prefix length exceeds remaining reservation"
        );
        let is_final = aligned_len == self.remaining_len;
        assert!(
            is_final || aligned_len & (self.sender.alignment - 1) == 0,
            "commit_prefix length must be a multiple of alignment"
        );
        // Because a buddy block cannot be freed in fragments, we attach `self.base_offset` only to
        // the final committed slot of this reservation. `Receiver::retire_slot` advances
        // `read_head` monotonically across contiguous completed slots, guaranteeing all earlier
        // prefix slots have also been retired by the time `read_head` advances past the final slot.
        let payload_offset = (is_final && self.owns_allocation).then_some(self.base_offset);
        let slot = self.sender.push_with_payload(msg, payload_offset)?;
        if is_final {
            self.owns_allocation = false;
        }
        if aligned_len > 0 {
            self.current_offset += aligned_len as u32;
            self.remaining_len -= aligned_len;
            // SAFETY: `aligned_len <= self.remaining_len` prior to subtraction, so advancing the
            // slice pointer by `aligned_len` stays within (or at one-past-the-end of) the mapped
            // VMO region.
            unsafe {
                let next_ptr = self.slice.as_mut_ptr().add(aligned_len);
                self.slice = MutPtrByteSlice::new(std::ptr::slice_from_raw_parts_mut(
                    next_ptr,
                    self.remaining_len,
                ));
            }
            self.last_committed_slot = Some(slot);
        }
        Ok(())
    }

    /// Commits the entire remaining reservation alongside `msg`.
    pub fn commit(mut self, msg: T) -> Result<(), zx::Status> {
        let len = self.remaining_len;
        self.commit_prefix(len, msg)
    }
}

impl<T: FromBytes + IntoBytes + KnownLayout + Copy, S: Deref<Target = SyncSender<T>>> Drop
    for PayloadBuffer<T, S>
{
    fn drop(&mut self) {
        if self.owns_allocation {
            self.sender.release_uncommitted_payload(self.base_offset, self.last_committed_slot);
        }
    }
}

struct ReceiverState {
    claim_index: u64,
    committed_read_index: u64,
    last_claim_write_index: u64,
    completed_slots: Box<[bool]>,
    data_available: EventSignal,
    space_available: EventSignal,
    wait_async_armed: bool,
    shutting_down: bool,
    sleeping_workers: usize,
    active_workers: usize,
}

impl ReceiverState {
    // Creates a new `ReceiverState` with the given slot `capacity` and starting `initial_index`.
    fn new(capacity: u32, initial_index: u64) -> Self {
        Self {
            claim_index: initial_index,
            committed_read_index: initial_index,
            last_claim_write_index: initial_index,
            completed_slots: vec![false; capacity as usize].into_boxed_slice(),
            data_available: EventSignal::new(SIG_DATA_AVAILABLE_0, SIG_DATA_AVAILABLE_1),
            space_available: EventSignal::new(SIG_SPACE_AVAILABLE_0, SIG_SPACE_AVAILABLE_1),
            wait_async_armed: false,
            shutting_down: false,
            sleeping_workers: 0,
            active_workers: 0,
        }
    }
}

/// A receiver endpoint for a VMO-backed FIFO queue.
pub struct Receiver<T> {
    inner: SharedQueue<T>,
    port: zx::Port,
    state: Mutex<ReceiverState>,
}

pub struct Message<'a, T: FromBytes + IntoBytes + KnownLayout + Copy> {
    receiver: &'a Receiver<T>,
    slot_index: u64,
    claimed: bool,
    popped: bool,
    item: T,
}

impl<'a, T: FromBytes + IntoBytes + KnownLayout + Copy> std::ops::Deref for Message<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.item
    }
}

impl<'a, T: FromBytes + IntoBytes + KnownLayout + Copy + Debug> Debug for Message<'a, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&self.item, f)
    }
}

impl<'a, T: FromBytes + IntoBytes + KnownLayout + Copy> Message<'a, T> {
    pub fn payload_region_offset(&self) -> usize {
        self.receiver.payload_region_offset()
    }

    pub fn payload_slice(&self, vmo_offset: u32, len: u32) -> PtrByteSlice<'_> {
        self.receiver.payload_slice(vmo_offset, len)
    }

    /// Returns a raw mutable slice pointer to the specified payload region in the shared VMO.
    ///
    /// # Panics
    ///
    /// Panics if `vmo_offset + len` exceeds the bounds of the VMO.
    pub fn payload_mut_slice(&mut self, vmo_offset: u32, len: u32) -> MutPtrByteSlice<'_> {
        // SAFETY: Each `Message` represents an exclusive claim over its payload range until
        // `pop()` retires the slot.
        unsafe { self.receiver.payload_mut_slice_unchecked(vmo_offset, len) }
    }

    /// Commit the read, freeing the slot.
    pub fn pop(mut self) -> Result<(), zx::Status> {
        self.pop_internal()
    }

    // Marks this message as popped and retires `self.slot_index` in the receiver.
    fn pop_internal(&mut self) -> Result<(), zx::Status> {
        if self.popped {
            return Ok(());
        }
        self.popped = true;
        self.receiver.retire_slot(self.slot_index, self.claimed)
    }
}

impl<'a, T: FromBytes + IntoBytes + KnownLayout + Copy> Drop for Message<'a, T> {
    fn drop(&mut self) {
        if self.claimed && !self.popped {
            let _ = self.pop_internal();
        }
    }
}

impl<T: FromBytes + IntoBytes + KnownLayout + Copy> Receiver<T> {
    pub fn new(vmo: zx::Vmo, capacity: u32) -> Result<Self, zx::Status> {
        let inner = SharedQueue::new(vmo, capacity)?;
        let port = zx::Port::create();
        Ok(Self { inner, port, state: Mutex::new(ReceiverState::new(capacity, 0)) })
    }

    // Creates a Receiver that initializes its cached index from the provided VMO. This is solely
    // used for tests. Receiver should normally not support a populated VMO.
    #[cfg(test)]
    fn new_from_populated_vmo(vmo: zx::Vmo, capacity: u32) -> Result<Self, zx::Status> {
        let inner = SharedQueue::new(vmo, capacity)?;
        let initial_index = inner.read_index();
        let port = zx::Port::create();
        Ok(Self { inner, port, state: Mutex::new(ReceiverState::new(capacity, initial_index)) })
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn capacity(&self) -> u32 {
        self.inner.capacity()
    }

    pub fn index(&self) -> u64 {
        self.inner.read_index()
    }

    pub fn vmo(&self) -> &zx::Vmo {
        self.inner.vmo()
    }

    pub fn payload_region_offset(&self) -> usize {
        self.inner.payload_region_offset()
    }

    /// Peek at the next message without incrementing the read index.
    pub fn peek(&mut self) -> Result<Message<'_, T>, zx::Status> {
        self.next_internal(false, |_| ()).map(|(msg, ())| msg)
    }

    /// Claims the next message from the queue for concurrent processing across a pool of threads.
    pub fn claim(&self) -> Result<Message<'_, T>, zx::Status> {
        self.next_internal(true, |_| ()).map(|(msg, ())| msg)
    }

    /// Claims the next message from the queue and invokes `on_claim` before releasing the queue's
    /// claim lock, allowing ordered setup steps (such as blob registration) to complete before
    /// subsequent messages are claimed by other worker threads.
    pub fn claim_with<F, R>(&self, on_claim: F) -> Result<(Message<'_, T>, R), zx::Status>
    where
        F: FnOnce(&mut Message<'_, T>) -> R,
    {
        self.next_internal(true, on_claim)
    }

    /// Notifies the receiver that the current worker thread (after popping its message) is about to
    /// perform an operation that may block waiting for a subsequent message on this queue. If all
    /// other workers are sleeping and unread messages were already queued before the waiter flag
    /// was armed, wakes a sleeping worker to process them.
    pub fn will_maybe_reenter(&self) {
        let state = self.state.lock();
        if state.active_workers == 0
            && state.sleeping_workers > 0
            && state.claim_index != state.last_claim_write_index
        {
            let _ = self.port.queue(&zx::Packet::from_user_packet(
                PORT_KEY_WAKE,
                0,
                zx::UserPacket::from_u8_array([0; 32]),
            ));
        }
    }

    // Waits for the next available message in the FIFO, optionally advancing `claim_index` and
    // running `on_claim` while holding `self.state`.
    fn next_internal<F, R>(
        &self,
        claim: bool,
        on_claim: F,
    ) -> Result<(Message<'_, T>, R), zx::Status>
    where
        F: FnOnce(&mut Message<'_, T>) -> R,
    {
        let mut state = self.state.lock();
        loop {
            // `Ordering::Acquire`: guarantees the writer has finished writing the new data to the
            // VMO before the reader attempts to read it.
            let mut curr_write_head = self.inner.load_write_head(Ordering::Acquire);
            if state.claim_index != curr_write_head.index() {
                let slot_index = state.claim_index;
                let slot_ptr = self.inner.get_slot_ptr(slot_index);
                // SAFETY: `slot_ptr` enforces VMO bounds. The `Acquire` ordering guarantees the
                // sender has fully completed writing before we read. Furthermore, `T: FromBytes`
                // ensures any arbitrary bits provided by the peer safely map to a valid struct.
                let item = unsafe { std::ptr::read(slot_ptr) };
                if claim {
                    state.claim_index = (state.claim_index + 1) & State::INDEX_MASK;
                    state.active_workers += 1;
                }

                let mut msg =
                    Message { receiver: self, slot_index, claimed: claim, popped: false, item };
                let res = on_claim(&mut msg);

                // If another worker thread is still asleep, re-arm `HAS_WAITER` and `wait_async`
                // so that if more items are pushed while this thread is busy processing `msg`,
                // the sender will signal the VMO and wake the sleeping worker.
                if claim {
                    while state.sleeping_workers > 0
                        && !state.wait_async_armed
                        && !state.shutting_down
                    {
                        let asleep_write_head = curr_write_head.with_waiter();
                        match self.inner.write_head_atomic().compare_exchange_weak(
                            curr_write_head.0,
                            asleep_write_head.0,
                            Ordering::Relaxed,
                            Ordering::Acquire,
                        ) {
                            Ok(_) => {
                                let vmo = self.inner.mapping.vmo();
                                let mask = state.data_available.current_mask(SIG_SHUTDOWN);
                                vmo.wait_async(
                                    &self.port,
                                    PORT_KEY_SIGNAL,
                                    mask,
                                    zx::WaitAsyncOpts::empty(),
                                )?;
                                state.wait_async_armed = true;
                            }
                            Err(actual) => curr_write_head = State(actual),
                        }
                    }
                    state.last_claim_write_index = curr_write_head.index();
                }

                return Ok((msg, res));
            }

            if state.shutting_down {
                return Err(zx::Status::CANCELED);
            }

            // The queue is empty. Inform the writer that we are going to sleep and waiting for a
            // signal when new items are pushed.
            if !state.wait_async_armed {
                let asleep_write_head = curr_write_head.with_waiter();
                if self
                    .inner
                    .write_head_atomic()
                    .compare_exchange_weak(
                        curr_write_head.0,
                        asleep_write_head.0,
                        // `Ordering::Relaxed`: We are just publishing a sleep signal.
                        Ordering::Relaxed,
                        // `Ordering::Acquire`: exchange failed (writer likely pushed an item).
                        Ordering::Acquire,
                    )
                    .is_err()
                {
                    continue;
                }

                let vmo = self.inner.mapping.vmo();
                let mask = state.data_available.current_mask(SIG_SHUTDOWN);
                vmo.wait_async(&self.port, PORT_KEY_SIGNAL, mask, zx::WaitAsyncOpts::empty())?;
                state.wait_async_armed = true;
            }

            state.sleeping_workers += 1;
            drop(state);

            let wait_res = self.port.wait(zx::MonotonicInstant::INFINITE);
            state = self.state.lock();
            state.sleeping_workers -= 1;
            let packet = wait_res?;

            if packet.key() == PORT_KEY_SIGNAL {
                state.wait_async_armed = false;
                if let zx::PacketContents::SignalOne(signals) = packet.contents() {
                    let observed = signals.observed();
                    if observed.contains(state.data_available.current_mask(zx::Signals::empty())) {
                        state.data_available.advance();
                    }
                    if observed.contains(SIG_SHUTDOWN) && !state.shutting_down {
                        state.shutting_down = true;
                        for _ in 0..state.sleeping_workers {
                            let _ = self.port.queue(&zx::Packet::from_user_packet(
                                PORT_KEY_WAKE,
                                0,
                                zx::UserPacket::from_u8_array([0; 32]),
                            ));
                        }
                    }
                }
            }
        }
    }

    // Records `slot_index` as completed and advances the shared `read_head` across all contiguous
    // completed slots from `committed_read_index`.
    fn retire_slot(&self, slot_index: u64, claimed: bool) -> Result<(), zx::Status> {
        let mut state = self.state.lock();
        if claimed {
            state.active_workers -= 1;
        } else if state.claim_index == slot_index {
            state.claim_index = (state.claim_index + 1) & State::INDEX_MASK;
        }
        let mask = (self.inner.capacity() - 1) as u64;
        state.completed_slots[(slot_index & mask) as usize] = true;
        let mut advanced = false;
        loop {
            let committed_slot = (state.committed_read_index & mask) as usize;
            if !state.completed_slots[committed_slot] {
                break;
            }
            state.completed_slots[committed_slot] = false;
            state.committed_read_index = (state.committed_read_index + 1) & State::INDEX_MASK;
            advanced = true;
        }

        if advanced {
            let target_read_index = state.committed_read_index;
            let mut curr_read_head = self.inner.load_read_head(Ordering::Relaxed);
            loop {
                let next_read_head = State(target_read_index);
                match self.inner.read_head_atomic().compare_exchange_weak(
                    curr_read_head.0,
                    next_read_head.0,
                    // `Ordering::Release`: ensure the reader has finished reading the old data from
                    // the VMO before the writer sees this new index and overwrites the slot.
                    Ordering::Release,
                    // `Ordering::Relaxed`: swap failed (most likely due to WAITER flag being
                    // modified).
                    Ordering::Relaxed,
                ) {
                    Ok(prev_read_head) => {
                        // Wake the writer if they fell asleep waiting for a free slot.
                        if State(prev_read_head).has_waiter() {
                            let vmo = self.inner.mapping.vmo();
                            state.space_available.signal(vmo)?;
                        }
                        break;
                    }
                    Err(actual) => curr_read_head = State(actual),
                }
            }
        }
        Ok(())
    }

    /// Returns a raw slice pointer to the specified payload region.
    ///
    /// # Panics
    ///
    /// Panics if the `vmo_offset + len` exceeds the bounds of the VMO.
    // TODO(https://fxbug.dev/530494057): Refactor to encode the payload layout as part of the FIFO
    // messaging envelope to safely resolve bounds.
    pub fn payload_slice(&self, vmo_offset: u32, len: u32) -> PtrByteSlice<'_> {
        // SAFETY:
        // 1. `self.inner.addr()` is a valid base pointer to a mapped VMO memory space active
        //    for the entire lifetime of this instance.
        // 2. The explicit assertion guarantees that `offset + len` physically fits within
        //    the bounded VMO boundaries, preventing out-of-bounds pointer derivation.
        // 3. Returning a `PtrByteSlice` wrapper instead of a native `&[u8]` avoids violating
        //    Rust's strict aliasing rules for shared memory, thereby preventing Undefined Behavior.
        unsafe {
            let offset = self.inner.payload_region_offset() + vmo_offset as usize;
            assert!(offset + len as usize <= self.inner.vmo_size(), "Payload out of bounds");
            let dest_ptr = (self.inner.addr() + offset) as *const u8;
            let slice = std::ptr::slice_from_raw_parts(dest_ptr, len as usize);
            PtrByteSlice::new(slice)
        }
    }

    /// Returns a raw mutable slice pointer to the specified payload region.
    ///
    /// # Panics
    ///
    /// Panics if the `vmo_offset + len` exceeds the bounds of the VMO.
    pub fn payload_mut_slice(&mut self, vmo_offset: u32, len: u32) -> MutPtrByteSlice<'_> {
        // SAFETY: `&mut self` guarantees exclusive access on the receiver side.
        unsafe { self.payload_mut_slice_unchecked(vmo_offset, len) }
    }

    // Returns a raw mutable slice pointer to `[vmo_offset .. vmo_offset + len)` in the payload
    // region. The caller must ensure exclusive access to that payload subrange.
    unsafe fn payload_mut_slice_unchecked(&self, vmo_offset: u32, len: u32) -> MutPtrByteSlice<'_> {
        // SAFETY:
        // 1. `self.inner.addr()` is a valid base pointer to a mapped VMO memory space active
        //    for the entire lifetime of this instance.
        // 2. The explicit assertion guarantees that `offset + len` physically fits within
        //    the bounded VMO boundaries, preventing out-of-bounds pointer derivation.
        // 3. Returning a `MutPtrByteSlice` wrapper instead of a native `&mut [u8]` avoids violating
        //    Rust's strict aliasing rules for shared memory, thereby preventing Undefined Behavior.
        unsafe {
            let offset = self.inner.payload_region_offset() + vmo_offset as usize;
            assert!(offset + len as usize <= self.inner.vmo_size(), "Payload out of bounds");
            let dest_ptr = (self.inner.addr() + offset) as *mut u8;
            let slice = std::ptr::slice_from_raw_parts_mut(dest_ptr, len as usize);
            MutPtrByteSlice::new(slice)
        }
    }
}

impl<T> Receiver<T> {
    pub fn shutdown(&self) -> Result<(), zx::Status> {
        self.inner.shutdown()
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[derive(FromBytes, IntoBytes, KnownLayout, Clone, Copy, Debug, PartialEq)]
    #[repr(C)]
    struct TestMessage {
        val: u32,
    }

    #[fuchsia::test]
    fn test_push_pop() {
        let vmo = zx::Vmo::create(8192).expect("VMO creation failed");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate_handle failed");

        let sender = SyncSender::<TestMessage>::new(vmo, 1, 4).expect("Sender new failed");
        let mut receiver = Receiver::<TestMessage>::new(vmo_dup, 4).expect("Receiver new failed");

        assert!(receiver.is_empty());
        assert!(!sender.is_full());

        let expected = TestMessage { val: 42 };
        sender.push(expected).expect("push failed");

        assert!(!receiver.is_empty());

        let next_msg = receiver.peek().expect("peek failed");
        assert_eq!(*next_msg, expected);

        next_msg.pop().expect("pop failed");
        assert!(receiver.is_empty());
    }

    #[fuchsia::test]
    fn test_blocking_push_pop() {
        let vmo = zx::Vmo::create(8192).expect("VMO creation failed");
        let vmo_writer =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("VMO duplicate_handle failed");

        let mut receiver = Receiver::<TestMessage>::new(vmo, 2).expect("receiver creation failed");
        let sender =
            SyncSender::<TestMessage>::new(vmo_writer, 8, 2).expect("sender creation failed");

        let handle = std::thread::spawn(move || {
            let msg1 = TestMessage { val: 1 };
            let msg2 = TestMessage { val: 2 };
            let msg3 = TestMessage { val: 3 };

            sender.push(msg1).expect("push failed");
            sender.push(msg2).expect("push failed");
            // This third push blocks because capacity is 2; it remains blocked until the receiver
            // commits a pop.
            sender.push(msg3).expect("push failed");
        });

        // Allow time for the writer thread to fill the queue and fall asleep waiting for space.
        thread::sleep(Duration::from_millis(100));

        let r1 = receiver.peek().expect("peek failed");
        assert_eq!(r1.val, 1);
        // Pop frees a slot, waking the blocked sender.
        r1.pop().expect("pop failed");

        let r2 = receiver.peek().expect("peek failed");
        assert_eq!(r2.val, 2);
        r2.pop().expect("pop failed");

        let r3 = receiver.peek().expect("peek failed");
        assert_eq!(r3.val, 3);
        r3.pop().expect("pop failed");

        handle.join().expect("writer thread panicked");
    }

    #[fuchsia::test]
    fn test_index_wrap_around() {
        let vmo = zx::Vmo::create(8192).expect("VMO creation failed");

        // Seed the write and read indexes to near INDEX_MASK.
        let start_idx = State::INDEX_MASK - 1;
        // `write_index` is at byte offset 0
        vmo.write(&start_idx.to_ne_bytes(), 0).expect("write failed");
        // `read_index` is at byte offset 8
        vmo.write(&start_idx.to_ne_bytes(), 8).expect("write failed");

        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate_handle failed");

        let sender = SyncSender::<TestMessage>::new(vmo, 8, 4).expect("Sender new failed");
        let mut receiver = Receiver::<TestMessage>::new_from_populated_vmo(vmo_dup, 4)
            .expect("Receiver new_from_populated_vmo failed");

        // Verify they started at the seeded value.
        assert_eq!(receiver.index(), start_idx);
        assert_eq!(sender.index(), start_idx);

        // Push some items to trigger wrap-around.
        sender.push(TestMessage { val: 10 }).expect("push failed");
        assert_eq!(sender.index(), State::INDEX_MASK);

        sender.push(TestMessage { val: 20 }).expect("push failed");
        assert_eq!(sender.index(), 0);

        sender.push(TestMessage { val: 30 }).expect("push failed");
        assert_eq!(sender.index(), 1);

        // Pop them and verify read_index also wraps around.
        let next_msg = receiver.peek().expect("peek failed");
        assert_eq!(next_msg.val, 10);
        next_msg.pop().expect("pop failed");
        assert_eq!(receiver.index(), State::INDEX_MASK);

        let next_msg = receiver.peek().expect("peek failed");
        assert_eq!(next_msg.val, 20);
        next_msg.pop().expect("pop failed");
        assert_eq!(receiver.index(), 0);

        let next_msg = receiver.peek().expect("peek failed");
        assert_eq!(next_msg.val, 30);
        next_msg.pop().expect("pop failed");
        assert_eq!(receiver.index(), 1);

        assert!(receiver.is_empty());
    }

    #[fuchsia::test]
    fn test_receiver_wakes_on_sender_drop() {
        let vmo = zx::Vmo::create(8192).expect("VMO creation failed");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("VMO duplicate_handle failed");

        let sender = SyncSender::<TestMessage>::new(vmo, 8, 2).expect("Sender creation failed");
        let mut receiver =
            Receiver::<TestMessage>::new(vmo_dup, 2).expect("Receiver creation failed");

        let handle = std::thread::spawn(move || {
            // Receiver blocks because the queue is empty
            assert_eq!(receiver.peek().unwrap_err(), zx::Status::CANCELED);
        });

        // Give the receiver thread a moment to block on wait()
        thread::sleep(Duration::from_millis(100));

        // Dropping sender should trigger shutdown
        drop(sender);

        handle.join().expect("thread panicked");
    }

    #[fuchsia::test]
    fn test_sender_wakes_on_receiver_drop() {
        let vmo = zx::Vmo::create(8192).expect("VMO creation failed");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("VMO duplicate_handle failed");

        let sender = SyncSender::<TestMessage>::new(vmo, 8, 2).expect("Sender creation failed");
        let receiver = Receiver::<TestMessage>::new(vmo_dup, 2).expect("Receiver creation failed");

        // Fill up the queue so the next push blocks
        sender.push(TestMessage { val: 1 }).expect("push failed");
        sender.push(TestMessage { val: 2 }).expect("push failed");

        let handle = std::thread::spawn(move || {
            // Sender blocks because the queue is full
            sender.push(TestMessage { val: 3 })
        });

        // Give the sender thread a moment to block on wait()
        thread::sleep(Duration::from_millis(100));

        // Dropping receiver should trigger shutdown
        drop(receiver);

        let result = handle.join().expect("thread panicked");
        assert_eq!(result.unwrap_err(), zx::Status::CANCELED);
    }

    // Wire Protocol Struct - this is the struct sent to queue
    #[derive(FromBytes, IntoBytes, KnownLayout, Clone, Copy, Debug, PartialEq)]
    #[repr(C)]
    struct WireTestCommand {
        opcode: u32,
        vmo_offset: u32,
        len: u32, // Used by WithPayload (opcode 0)
        _pad: u32,
        val: u64, // Used by NoPayload (opcode 1)
    }

    #[derive(Debug, PartialEq)]
    enum TestCommand {
        WithPayload { vmo_offset: u32, len: u32 },
        NoPayload { val: u64 },
    }

    impl TestCommand {
        fn to_wire(&self) -> WireTestCommand {
            match self {
                TestCommand::WithPayload { vmo_offset, len } => WireTestCommand {
                    opcode: 0,
                    vmo_offset: *vmo_offset,
                    len: *len,
                    _pad: 0,
                    val: 0,
                },
                TestCommand::NoPayload { val } => {
                    WireTestCommand { opcode: 1, vmo_offset: 0, len: 0, _pad: 0, val: *val }
                }
            }
        }

        fn from_wire(wire: WireTestCommand) -> Self {
            match wire.opcode {
                0 => TestCommand::WithPayload { vmo_offset: wire.vmo_offset, len: wire.len },
                _ => TestCommand::NoPayload { val: wire.val },
            }
        }
    }

    #[fuchsia::test]
    fn test_payload_drop_releases_allocation() {
        let page_size = zx::system_get_page_size() as u64;
        let vmo = zx::Vmo::create(page_size * 2).expect("VMO creation failed");

        let mut sender =
            SyncSender::<WireTestCommand>::new(vmo, 8, 4).expect("Sender creation failed");

        let buffer = sender.reserve_payload(10).expect("reserve failed");
        assert_eq!(buffer.offset(), 0);

        // Roll it back instead of committing
        drop(buffer);

        // Allocate again, ensure it gives the same offset
        let buffer2 = sender.reserve_payload(10).expect("reserve failed");
        assert_eq!(buffer2.offset(), 0);
    }

    #[fuchsia::test]
    fn test_payload_buffer_drops_uncommitted_allocations() {
        let page_size = zx::system_get_page_size() as u64;
        let vmo = zx::Vmo::create(page_size * 2).expect("VMO creation failed");

        let mut sender =
            SyncSender::<WireTestCommand>::new(vmo, 8, 4).expect("Sender creation failed");

        // Simulate a function that reserves a payload but returns early (e.g., from encountering
        // an error).
        let _ = (|| -> Result<(), ()> {
            let _buffer = sender.reserve_payload(10).expect("reserve failed");

            // Assuming an error is encountered before `_buffer` can be committed,
            // the early return causes the buffer to be dropped, triggering cancellation.
            Err(())
        })();

        // Verifies that the early return successfully dropped `_buffer` and cancelled the
        // allocation. The queue is now empty, placing this allocation at offset 0.
        let buffer_after_bail = sender.reserve_payload(10).expect("reserve failed");
        assert_eq!(buffer_after_bail.offset(), 0);
    }

    #[fuchsia::test]
    fn test_with_payload_commands() {
        let page_size = zx::system_get_page_size() as u64;
        let vmo = zx::Vmo::create(page_size * 2).expect("VMO creation failed");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("VMO duplicate_handle failed");

        let mut sender =
            SyncSender::<WireTestCommand>::new(vmo, 8, 4).expect("Sender creation failed");
        let mut receiver =
            Receiver::<WireTestCommand>::new(vmo_dup, 4).expect("Receiver creation failed");

        let mut buffer = sender.reserve_payload(10).expect("reserve failed");
        buffer.data().fill(0xAA);
        let wire_msg = TestCommand::WithPayload { vmo_offset: buffer.offset(), len: 10 }.to_wire();
        buffer.commit(wire_msg).expect("commit failed");

        let standalone = TestCommand::NoPayload { val: 456 };
        sender.push(standalone.to_wire()).expect("push failed");

        let msg1 = receiver.peek().expect("peek failed");
        let app_msg1 = TestCommand::from_wire(*msg1);
        if let TestCommand::WithPayload { vmo_offset, len } = app_msg1 {
            assert_eq!(len, 10);
            let read_data = msg1.payload_slice(vmo_offset, len);
            assert_eq!(read_data.to_vec(), vec![0xAA; 10]);
        } else {
            panic!("Expected TestCommand::WithPayload");
        }
        msg1.pop().expect("pop failed");

        let msg2 = receiver.peek().expect("peek failed");
        let app_msg2 = TestCommand::from_wire(*msg2);
        assert_eq!(app_msg2, TestCommand::NoPayload { val: 456 });
        msg2.pop().expect("pop failed");
    }

    #[fuchsia::test]
    fn test_pop_handles_canceled_race_condition() {
        // Run ten thousand times to force thread-scheduler starvation naturally
        for _ in 0..10000 {
            let vmo = zx::Vmo::create(8192).unwrap();
            let vmo_dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
            let sender = SyncSender::<u32>::new(vmo, 4, 1).unwrap();
            let mut receiver = Receiver::<u32>::new(vmo_dup, 1).unwrap();

            let handle = std::thread::spawn(move || {
                // Spin loop until the receiver sets the WAITER flag indicating it is blocked.
                loop {
                    let write_head = sender.inner.load_write_head(Ordering::Acquire);
                    if write_head.has_waiter() {
                        break;
                    }
                    std::thread::yield_now();
                }

                // Immediately push (which asserts SIG_DATA_AVAILABLE and wakes the receiver).
                sender.push(42).expect("push failed");

                // Drop the sender immediately (which asserts SIG_SHUTDOWN on the VMO). The receiver
                // will occasionally observe both signals, and must not incorrectly return
                // ZX_ERR_CANCELED since valid data is waiting in the queue.
                drop(sender);
            });

            // This call will set the WAITER flag, go to sleep, sometimes wakes up with both data
            // and shutdown signals, and should successfully pop the payload.
            let val = receiver.peek().expect("peek failed on race condition");
            assert_eq!(*val, 42);
            val.pop().expect("pop failed");

            // The queue is now genuinely empty and the peer has dropped.
            assert_eq!(receiver.peek().unwrap_err(), zx::Status::CANCELED);

            handle.join().unwrap();
        }
    }

    #[fuchsia::test]
    async fn test_async_push_yields_when_full() {
        let page_size = zx::system_get_page_size() as u64;
        let vmo = zx::Vmo::create(page_size * 2).expect("failed to create VMO");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("failed to duplicate VMO handle");

        let queue_capacity = 4;
        let mut sender = AsyncSender::<TestMessage>::new(vmo, 4, queue_capacity)
            .expect("failed to create AsyncSender");
        let mut receiver = Receiver::<TestMessage>::new(vmo_dup, queue_capacity)
            .expect("failed to create Receiver");

        for i in 0..queue_capacity {
            sender.push(TestMessage { val: i }).await.expect("failed to push message");
        }

        let push_task = async {
            sender
                .push(TestMessage { val: 42 })
                .await
                .expect("background push task failed after yielding");
        };
        let pop_thread = std::thread::spawn(move || {
            thread::sleep(Duration::from_millis(5));
            let msg = receiver.peek().expect("failed to peek message");
            assert_eq!(msg.val, 0); // we expect 0 to be popped since it was pushed first
            msg.pop().expect("failed to pop message");
            receiver
        });

        push_task.await;
        let mut receiver = pop_thread.join().unwrap();

        for expected in [1, 2, 3, 42] {
            let msg = receiver.peek().expect("failed to peek message");
            assert_eq!(msg.val, expected);
            msg.pop().expect("failed to pop message");
        }
    }

    #[fuchsia::test]
    async fn test_reserve_payload_async_yields() {
        let page_size = zx::system_get_page_size() as u64;
        let vmo = zx::Vmo::create(page_size * 2).expect("failed to create VMO");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("failed to duplicate VMO handle");

        let mut sender =
            AsyncSender::<TestMessage>::new(vmo, 4, 4).expect("failed to create AsyncSender");
        let mut receiver =
            Receiver::<TestMessage>::new(vmo_dup, 4).expect("failed to create Receiver");

        // The VMO is 2 pages long. The first page is reserved for queue headers and message slots,
        // leaving `page_size` bytes of remaining payload capacity. Reserving `page_size` bytes here
        // completely exhausts the payload buffer.
        let block1 = sender
            .reserve_payload(page_size as usize)
            .await
            .expect("failed to reserve payload capacity");
        block1.commit(TestMessage { val: 1 }).await.expect("failed to commit message block");

        // With 0 bytes of payload capacity remaining, the next reserve is forced to yield.
        let reserve_task = async {
            let buffer = sender
                .reserve_payload(4)
                .await
                .expect("background reserve payload task failed after yielding");
            buffer.commit(TestMessage { val: 2 }).await.expect("failed to commit message block");
        };
        let pop_thread = std::thread::spawn(move || {
            // Give the reserve_task a tiny bit of time to yield
            thread::sleep(Duration::from_millis(5));
            let msg = receiver.peek().expect("failed to peek message");
            assert_eq!(msg.val, 1);
            msg.pop().expect("failed to pop message");
            receiver
        });

        reserve_task.await;
        let mut receiver = pop_thread.join().unwrap();

        // Pop the second item out
        let msg = receiver.peek().expect("failed to peek message");
        assert_eq!(msg.val, 2);
        msg.pop().expect("failed to pop message");
    }

    #[fuchsia::test]
    async fn test_async_commit_yields_when_full() {
        let page_size = zx::system_get_page_size() as u64;
        let vmo = zx::Vmo::create(page_size * 2).expect("failed to create VMO");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("failed to duplicate VMO handle");

        let queue_capacity = 4;
        let mut sender = AsyncSender::<WireTestCommand>::new(vmo, 4, queue_capacity)
            .expect("failed to create AsyncSender");
        let mut receiver = Receiver::<WireTestCommand>::new(vmo_dup, queue_capacity)
            .expect("failed to create Receiver");

        // Fill up all the queue slots.
        for i in 0..queue_capacity {
            let standalone = TestCommand::NoPayload { val: i as u64 };
            sender.push(standalone.to_wire()).await.expect("failed to push message");
        }

        // Even though payload capacity is available, the queue slot capacity is fully saturated.
        let block = sender.reserve_payload(4).await.expect("failed to reserve payload capacity");

        let commit_task = async {
            let msg = TestCommand::WithPayload { vmo_offset: block.offset(), len: 4 };
            block
                .commit(msg.to_wire())
                .await
                .expect("background commit task failed after yielding");
        };

        let pop_thread = std::thread::spawn(move || {
            // Give the commit_task a tiny bit of time to yield
            thread::sleep(Duration::from_millis(5));

            // Pop the 4 standalone messages we initially filled the queue with.
            for expected in 0..4 {
                let msg = receiver.peek().expect("failed to peek message");
                let app_msg = TestCommand::from_wire(*msg);
                assert!(matches!(app_msg, TestCommand::NoPayload { val } if val == expected));
                msg.pop().expect("failed to pop message");
            }

            // Pop the actually pushed payload command to verify it.
            let msg2 = receiver.peek().expect("failed to pop payload message");
            let app_msg2 = TestCommand::from_wire(*msg2);
            assert!(matches!(app_msg2, TestCommand::WithPayload { .. }));
            msg2.pop().expect("failed to pop message");
        });

        commit_task.await;
        pop_thread.join().unwrap();
    }

    #[fuchsia::test]
    fn test_push_when_receiver_drops() {
        let vmo = zx::Vmo::create(8192).expect("VMO creation failed");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("VMO duplicate_handle failed");

        let sender = SyncSender::<TestMessage>::new(vmo, 8, 4).expect("Sender creation failed");
        let receiver = Receiver::<TestMessage>::new(vmo_dup, 4).expect("Receiver creation failed");

        // The receiver goes away unexpectedly before the queue is full.
        drop(receiver);

        // Even though the remote peer is dead, the queue still has capacity and doesn't know peer
        // has died.
        sender.push(TestMessage { val: 1 }).expect("push failed");
        sender.push(TestMessage { val: 2 }).expect("push failed");
        sender.push(TestMessage { val: 3 }).expect("push failed");
        sender.push(TestMessage { val: 4 }).expect("push failed");

        // Now the queue is full. The sender blocks until receiver signals there is space again, but
        // discovers that the receiver has died with `SIG_SHUTDOWN`.
        assert_eq!(sender.push(TestMessage { val: 5 }), Err(zx::Status::CANCELED));
    }

    #[fuchsia::test]
    async fn test_async_push_when_receiver_drops() {
        let vmo = zx::Vmo::create(8192).expect("VMO creation failed");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("VMO duplicate_handle failed");

        let mut sender =
            AsyncSender::<TestMessage>::new(vmo, 8, 4).expect("Sender creation failed");
        let receiver = Receiver::<TestMessage>::new(vmo_dup, 4).expect("Receiver creation failed");

        // The receiver goes away unexpectedly before the queue is full.
        drop(receiver);

        // Even though the remote peer is dead, the queue still has capacity and doesn't know peer
        // has died.
        sender.push(TestMessage { val: 1 }).await.expect("push failed");
        sender.push(TestMessage { val: 2 }).await.expect("push failed");
        sender.push(TestMessage { val: 3 }).await.expect("push failed");
        sender.push(TestMessage { val: 4 }).await.expect("push failed");

        // Now the queue is full. The sender blocks until receiver signals there is space again, but
        // discovers that the receiver has died with `SIG_SHUTDOWN`.
        assert_eq!(sender.push(TestMessage { val: 5 }).await.unwrap_err(), zx::Status::CANCELED);
    }

    #[fuchsia::test]
    fn test_reserve_payload_sync_exceeds_capacity() {
        let page_size = zx::system_get_page_size() as u64;
        let vmo = zx::Vmo::create(page_size * 2).expect("failed to create VMO");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("failed to duplicate VMO handle");

        let mut sender =
            SyncSender::<TestMessage>::new(vmo, 4, 4).expect("failed to create Sender");
        let _receiver =
            Receiver::<TestMessage>::new(vmo_dup, 4).expect("failed to create Receiver");

        // The first page is reserved for queue headers and message slots. We attempt to reserve
        // more than the capacity.
        let result = sender.reserve_payload((page_size + 10) as usize);
        assert_eq!(result.err(), Some(zx::Status::NO_MEMORY));
    }

    #[fuchsia::test]
    async fn test_reserve_payload_async_exceeds_capacity() {
        let page_size = zx::system_get_page_size() as u64;
        let vmo = zx::Vmo::create(page_size * 2).expect("failed to create VMO");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("failed to duplicate VMO handle");

        let mut sender =
            AsyncSender::<TestMessage>::new(vmo, 4, 4).expect("failed to create AsyncSender");
        let _receiver =
            Receiver::<TestMessage>::new(vmo_dup, 4).expect("failed to create Receiver");

        // The first page is reserved for queue headers and message slots. We attempt to reserve
        // more than the capacity.
        let result = sender.reserve_payload((page_size + 10) as usize).await;
        assert_eq!(result.err(), Some(zx::Status::NO_MEMORY));
    }

    #[fuchsia::test]
    fn test_concurrent_claim_out_of_order_retire() {
        let vmo = zx::Vmo::create(65536).expect("VMO creation failed");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("VMO duplicate_handle failed");

        let sender = SyncSender::<TestMessage>::new(vmo, 8, 8).expect("Sender creation failed");
        let receiver =
            Arc::new(Receiver::<TestMessage>::new(vmo_dup, 8).expect("Receiver creation failed"));

        sender.push(TestMessage { val: 10 }).expect("push 1 failed");
        sender.push(TestMessage { val: 20 }).expect("push 2 failed");

        let msg1 = receiver.claim().expect("claim 1 failed");
        let msg2 = receiver.claim().expect("claim 2 failed");
        assert_eq!(msg1.val, 10);
        assert_eq!(msg2.val, 20);
        assert_eq!(receiver.index(), 0);

        // Retire slot 1 before slot 0: committed read_index must remain 0 until slot 0 finishes.
        msg2.pop().expect("pop 2 failed");
        assert_eq!(receiver.index(), 0);

        // Retiring slot 0 advances committed read_index to 2.
        msg1.pop().expect("pop 1 failed");
        assert_eq!(receiver.index(), 2);

        // Verify a pool of 2 worker threads can concurrently drain items and shut down cleanly.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let rx = receiver.clone();
            let seen = seen.clone();
            threads.push(std::thread::spawn(move || {
                while let Ok(msg) = rx.claim() {
                    seen.lock().push(msg.val);
                    msg.pop().expect("pop failed");
                }
            }));
        }

        for i in 0..100 {
            sender.push(TestMessage { val: i }).expect("push failed");
        }
        drop(sender);

        for t in threads {
            t.join().expect("worker panicked");
        }

        let mut results = seen.lock().clone();
        results.sort_unstable();
        assert_eq!(results, (0..100).collect::<Vec<u32>>());
    }

    #[derive(FromBytes, IntoBytes, KnownLayout, Clone, Copy, Debug, PartialEq)]
    #[repr(C)]
    struct PayloadRefMessage {
        id: u32,
        offset: u32,
        length: u32,
    }

    #[fuchsia::test]
    fn test_concurrent_sender_out_of_order_prefix_and_drop_wakeup() {
        let page = zx::system_get_page_size() as usize;
        // 1 page header/slots + 4 pages payload.
        let vmo = zx::Vmo::create((5 * page) as u64).expect("VMO creation failed");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("VMO duplicate_handle failed");

        let sender = Arc::new(SyncSender::<PayloadRefMessage>::new(vmo, page, 8).expect("sender"));
        let mut receiver =
            Receiver::<PayloadRefMessage>::new(vmo_dup, 8).expect("receiver creation failed");
        assert_eq!(sender.payload_capacity(), 4 * page);

        // Reservation A takes 2 pages [0..2*page), reservation B takes 2 pages [2*page..4*page).
        let mut res_a = sender.reserve_owned_payload(2 * page, 0).expect("res_a");
        let mut res_b = sender.reserve_owned_payload(2 * page, 0).expect("res_b");
        assert_eq!(res_a.offset(), 0);
        assert_eq!(res_b.offset(), (2 * page) as u32);

        // Payload is now 100% full. Spawn a writer C requesting 2 pages; it must block without
        // preventing res_b or res_a from writing, committing, or dropping!
        let sender_c = Arc::clone(&sender);
        let writer_c = thread::spawn(move || {
            let mut res_c = sender_c.reserve_owned_payload(2 * page, 0).expect("res_c");
            res_c.data_mut().fill(0xCC);
            let msg = PayloadRefMessage {
                id: 3,
                offset: res_c.offset(),
                length: res_c.remaining_len() as u32,
            };
            res_c.commit(msg).expect("commit c");
        });

        thread::sleep(Duration::from_millis(10));

        // B finishes BEFORE A, commits first page via commit_prefix, and drops its second page!
        res_b.data_mut().subslice_mut(0..page).fill(0xB1);
        let b_offset = res_b.offset();
        res_b
            .commit_prefix(page, PayloadRefMessage { id: 2, offset: b_offset, length: page as u32 })
            .expect("commit_prefix b");
        assert_eq!(res_b.remaining_len(), page);
        drop(res_b); // Transfers ownership of the 2-page buddy block to B's committed slot.

        // Receiver pops B's 1-page message -> frees B's 2-page buddy block [2*page..4*page)
        // while A STILL owns [0..2*page)!
        let msg_b = receiver.peek().expect("peek b");
        assert_eq!(msg_b.id, 2);
        let mut read_b = vec![0u8; page];
        msg_b.payload_slice(msg_b.offset, msg_b.length).copy_to_slice(&mut read_b);
        assert_eq!(read_b, vec![0xB1; page]);
        msg_b.pop().expect("pop b");

        // C must wake up and take [2*page..4*page), never touching A's [0..2*page)!
        writer_c.join().expect("writer_c panicked");

        // Verify A can still safely write to [0..2*page) and commit after C!
        res_a.data_mut().fill(0xAA);
        let a_msg = PayloadRefMessage {
            id: 1,
            offset: res_a.offset(),
            length: res_a.remaining_len() as u32,
        };
        res_a.commit(a_msg).expect("commit a");

        let msg_c = receiver.peek().expect("peek c");
        assert_eq!(msg_c.id, 3);
        assert_eq!(msg_c.offset, (2 * page) as u32);
        let mut read_c = vec![0u8; 2 * page];
        msg_c.payload_slice(msg_c.offset, msg_c.length).copy_to_slice(&mut read_c);
        assert_eq!(read_c, vec![0xCC; 2 * page]);
        msg_c.pop().expect("pop c");

        let msg_a = receiver.peek().expect("peek a");
        assert_eq!(msg_a.id, 1);
        assert_eq!(msg_a.offset, 0);
        let mut read_a = vec![0u8; 2 * page];
        msg_a.payload_slice(msg_a.offset, msg_a.length).copy_to_slice(&mut read_a);
        assert_eq!(read_a, vec![0xAA; 2 * page]);
        msg_a.pop().expect("pop a");
    }

    #[fuchsia::test]
    fn test_concurrent_sender_multi_prefix_and_uncommitted_drop_wakeup() {
        let page = zx::system_get_page_size() as usize;
        // 1 page header/slots + 2 pages payload.
        let vmo = zx::Vmo::create((3 * page) as u64).expect("VMO creation failed");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("VMO duplicate_handle failed");

        let sender = Arc::new(SyncSender::<PayloadRefMessage>::new(vmo, page, 8).expect("sender"));
        let mut receiver =
            Receiver::<PayloadRefMessage>::new(vmo_dup, 8).expect("receiver creation failed");
        assert_eq!(sender.payload_capacity(), 2 * page);
        assert_eq!(sender.max_allocation_size(), 2 * page);

        // Reserve all 2 pages, commit the first page, and have the consumer pop it immediately
        // while the second page is still uncommitted.
        let mut res1 = sender.reserve_owned_payload(2 * page, 0).expect("res1");
        res1.data_mut().subslice_mut(0..page).fill(0x11);
        let off1 = res1.offset();
        res1.commit_prefix(page, PayloadRefMessage { id: 10, offset: off1, length: page as u32 })
            .expect("commit_prefix 1");

        let msg1 = receiver.peek().expect("peek 1");
        assert_eq!(msg1.id, 10);
        msg1.pop().expect("pop 1");

        // Even though `msg1` was popped, `res1` still owns the second page of the 2-page buddy
        // block, so a new 1-page reservation with 2-page capacity must still block until `res1`
        // finishes!
        res1.data_mut().fill(0x22);
        let off2 = res1.offset();
        assert_eq!(off2, page as u32);
        res1.commit(PayloadRefMessage { id: 11, offset: off2, length: page as u32 })
            .expect("commit 2");

        let msg2 = receiver.peek().expect("peek 2");
        assert_eq!(msg2.id, 11);
        let mut read2 = vec![0u8; page];
        msg2.payload_slice(msg2.offset, msg2.length).copy_to_slice(&mut read2);
        assert_eq!(read2, vec![0x22; page]);
        msg2.pop().expect("pop 2");

        // Now reserve all 2 pages and drop without committing while another writer is blocked;
        // dropping the uncommitted reservation must immediately wake the blocked writer.
        let res_drop = sender.reserve_owned_payload(2 * page, 0).expect("res_drop");
        let sender_w = Arc::clone(&sender);
        let writer = thread::spawn(move || {
            let mut res_w = sender_w.reserve_owned_payload(2 * page, 0).expect("res_w");
            res_w.data_mut().fill(0x77);
            let off = res_w.offset();
            res_w
                .commit(PayloadRefMessage { id: 20, offset: off, length: (2 * page) as u32 })
                .expect("commit w");
        });

        thread::sleep(Duration::from_millis(10));
        drop(res_drop);
        writer.join().expect("writer panicked");

        let msg_w = receiver.peek().expect("peek w");
        assert_eq!(msg_w.id, 20);
        msg_w.pop().expect("pop w");
    }

    #[fuchsia::test]
    fn test_concurrent_sender_drop_after_prefix_already_popped() {
        let page = zx::system_get_page_size() as usize;
        // 1 page header/slots + 2 pages payload.
        let vmo = zx::Vmo::create((3 * page) as u64).expect("VMO creation failed");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("VMO duplicate_handle failed");

        let sender = Arc::new(SyncSender::<PayloadRefMessage>::new(vmo, page, 8).expect("sender"));
        let mut receiver =
            Receiver::<PayloadRefMessage>::new(vmo_dup, 8).expect("receiver creation failed");

        // Reserve 2 pages, commit 1 page via commit_prefix, and have the consumer pop it BEFORE
        // the reservation is dropped.
        let mut res1 = sender.reserve_owned_payload(2 * page, 0).expect("res1");
        let off1 = res1.offset();
        res1.commit_prefix(page, PayloadRefMessage { id: 1, offset: off1, length: page as u32 })
            .expect("commit_prefix");

        let msg1 = receiver.peek().expect("peek 1");
        assert_eq!(msg1.id, 1);
        msg1.pop().expect("pop 1");

        // Spawn a writer waiting for the full 2-page block.
        let sender_w = Arc::clone(&sender);
        let writer = thread::spawn(move || {
            let res_w = sender_w.reserve_owned_payload(2 * page, 0).expect("res_w");
            let off = res_w.offset();
            res_w
                .commit(PayloadRefMessage { id: 2, offset: off, length: (2 * page) as u32 })
                .expect("commit w");
        });

        thread::sleep(Duration::from_millis(10));
        // Dropping `res1` after `last_committed_slot` was already popped must free `base_offset`
        // immediately (rather than attaching it to an already-retired slot) and wake `writer`.
        drop(res1);
        writer.join().expect("writer panicked");

        let msg2 = receiver.peek().expect("peek 2");
        assert_eq!(msg2.id, 2);
        assert_eq!(msg2.offset, 0);
        msg2.pop().expect("pop 2");
    }

    #[fuchsia::test]
    fn test_concurrent_sender_headroom_and_zero_byte_reservations() {
        let page = zx::system_get_page_size() as usize;
        // 1 page header/slots + 4 pages payload.
        let vmo = zx::Vmo::create((5 * page) as u64).expect("VMO creation failed");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("VMO duplicate_handle failed");

        let sender = Arc::new(SyncSender::<PayloadRefMessage>::new(vmo, page, 8).expect("sender"));
        let mut receiver =
            Receiver::<PayloadRefMessage>::new(vmo_dup, 8).expect("receiver creation failed");

        // Reserve 2 pages, leaving 2 pages free.
        let res1 = sender.reserve_owned_payload(2 * page, 0).expect("res1");

        // A low-priority writer requiring 2 pages + 1 page of headroom must block while only 2
        // pages are free.
        let sender_low = Arc::clone(&sender);
        let low_writer = thread::spawn(move || {
            let res_low = sender_low.reserve_owned_payload(2 * page, page).expect("res_low");
            let off = res_low.offset();
            res_low
                .commit(PayloadRefMessage { id: 30, offset: off, length: (2 * page) as u32 })
                .expect("commit low");
        });

        thread::sleep(Duration::from_millis(10));

        // Meanwhile, a high-priority reservation (headroom = 0) can claim the remaining 2 pages.
        let mut res_high = sender.reserve_owned_payload(2 * page, 0).expect("res_high");
        assert_eq!(res_high.offset(), (2 * page) as u32);

        // Even with 0 payload bytes free, a 0-byte reservation and 0-byte commit_prefix succeed
        // without blocking.
        let res_zero = sender.reserve_owned_payload(0, 4 * page).expect("res_zero");
        assert_eq!(res_zero.remaining_len(), 0);
        res_zero.commit(PayloadRefMessage { id: 10, offset: 0, length: 0 }).expect("commit zero");
        res_high
            .commit_prefix(0, PayloadRefMessage { id: 11, offset: 0, length: 0 })
            .expect("commit_prefix zero");

        // Commit both 2-page reservations and pop all messages so `low_writer` can proceed.
        res1.commit(PayloadRefMessage { id: 20, offset: 0, length: (2 * page) as u32 })
            .expect("commit 1");
        let high_off = res_high.offset();
        res_high
            .commit(PayloadRefMessage { id: 21, offset: high_off, length: (2 * page) as u32 })
            .expect("commit high");

        for expected_id in [10, 11, 20, 21] {
            let msg = receiver.peek().expect("peek");
            assert_eq!(msg.id, expected_id);
            msg.pop().expect("pop");
        }

        low_writer.join().expect("low_writer panicked");
        let msg_low = receiver.peek().expect("peek low");
        assert_eq!(msg_low.id, 30);
        msg_low.pop().expect("pop low");
    }

    #[fuchsia::test]
    fn test_concurrent_sender_shutdown_wakes_multiple_blocked_writers() {
        let page = zx::system_get_page_size() as usize;
        // 1 page header/slots + 1 page payload, queue_capacity = 2.
        let vmo = zx::Vmo::create((2 * page) as u64).expect("VMO creation failed");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("VMO duplicate_handle failed");

        let sender = Arc::new(SyncSender::<PayloadRefMessage>::new(vmo, page, 2).expect("sender"));
        let receiver =
            Receiver::<PayloadRefMessage>::new(vmo_dup, 2).expect("receiver creation failed");

        // Exhaust both the 1-page payload region and the 2 FIFO slots.
        let _res_full = sender.reserve_owned_payload(page, 0).expect("res_full");
        sender.push(PayloadRefMessage { id: 1, offset: 0, length: 0 }).expect("push 1");
        sender.push(PayloadRefMessage { id: 2, offset: 0, length: 0 }).expect("push 2");

        let mut handles = Vec::new();
        for _ in 0..2 {
            let s = Arc::clone(&sender);
            handles.push(thread::spawn(move || s.reserve_owned_payload(page, 0).err()));
        }
        let s_push = Arc::clone(&sender);
        handles.push(thread::spawn(move || {
            s_push.push(PayloadRefMessage { id: 3, offset: 0, length: 0 }).err()
        }));

        thread::sleep(Duration::from_millis(10));
        drop(receiver);

        for h in handles {
            assert_eq!(h.join().expect("writer panicked"), Some(zx::Status::CANCELED));
        }
    }
}
