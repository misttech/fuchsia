// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod ring_allocator;
pub use ring_allocator::AllocationToken;
use ring_allocator::RingAllocator;

mod signal;
use fuchsia_sync::Mutex;
pub use signal::SIG_SHUTDOWN;
use signal::{
    EventSignal, SIG_DATA_AVAILABLE_0, SIG_DATA_AVAILABLE_1, SIG_SPACE_AVAILABLE_0,
    SIG_SPACE_AVAILABLE_1,
};
use std::fmt::{Debug, Formatter};
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
    // The maximum number of items the queue can hold.
    // Must be a power of two: The queue index resets to zero at 2^63. The capacity must be a power
    // of two so that the overflow reset aligns with the modulo arithmetic boundary used for slot
    // calculation (`index % capacity`).
    capacity: u32,

    // Used by the consumer to wait for and by the producer to signal that the queue is non-empty.
    data_available: EventSignal,
    // Used by the producer to wait for and by the consumer to signal that space is available.
    space_available: EventSignal,

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

        Ok(Self {
            mapping,
            header,
            slots,
            capacity,
            data_available: EventSignal::new(SIG_DATA_AVAILABLE_0, SIG_DATA_AVAILABLE_1),
            space_available: EventSignal::new(SIG_SPACE_AVAILABLE_0, SIG_SPACE_AVAILABLE_1),
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
        let raw_offset = HEADER_SIZE + self.capacity as usize * std::mem::size_of::<T>();
        let page_size = zx::system_get_page_size() as usize;
        (raw_offset + page_size - 1) & !(page_size - 1)
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

    fn read_head_atomic(&self) -> &AtomicU64 {
        // SAFETY: `self.header` points to a valid memory-mapped VMO region of at least HEADER_SIZE.
        unsafe {
            let ptr = std::ptr::addr_of_mut!((*self.header).read_head);
            AtomicU64::from_ptr(ptr)
        }
    }

    fn load_write_head(&self, ordering: Ordering) -> State {
        State(self.write_head_atomic().load(ordering))
    }

    fn load_read_head(&self, ordering: Ordering) -> State {
        State(self.read_head_atomic().load(ordering))
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
        let slot_idx = ((index & State::INDEX_MASK) % self.capacity as u64) as isize;
        // SAFETY: `slot_idx` is calculated using modulo `capacity`, guaranteeing it stays within
        // the validated memory bounds of `self.slots`.
        unsafe { self.slots.offset(slot_idx) }
    }
}

impl<T> SharedQueue<T> {
    // Trigger shutdown, waking up all waiters.
    fn shutdown(&self) -> Result<(), zx::Status> {
        self.mapping.vmo().signal(zx::Signals::empty(), SIG_SHUTDOWN)?;
        Ok(())
    }
}

// The core of a Sender endpoint containing helper functions for the `SyncSender` and `AsyncSender`.
struct SenderInner<T> {
    inner: SharedQueue<T>,
    allocator: RingAllocator,
}
impl<T: FromBytes + IntoBytes + KnownLayout + Copy> SenderInner<T> {
    // Creates a new sender endpoint mapping the provided `vmo`.
    //
    // * `vmo` - The shared mapping destination.
    // * `alignment` - The byte boundaries all payload allocations must adhere to. Modulo padding
    //                 will be applied so that all payloads begin aligned to this size. Must be a
    //                 power of two.
    // * `capacity` - Maximum queue node capacity.
    fn new(vmo: zx::Vmo, alignment: usize, capacity: u32) -> Result<Self, zx::Status> {
        let inner = SharedQueue::new(vmo, capacity)?;
        let payload_size = inner.vmo_size().saturating_sub(inner.payload_region_offset());
        let allocator = RingAllocator::new(payload_size, inner.capacity() as usize, alignment);
        Ok(Self { inner, allocator })
    }

    fn is_full(&self) -> bool {
        self.inner.is_full()
    }

    fn capacity(&self) -> u32 {
        self.inner.capacity()
    }

    fn index(&self) -> u64 {
        self.inner.write_index()
    }

    fn vmo(&self) -> &zx::Vmo {
        self.inner.vmo()
    }

    // Attempts to reserve the next available slot in the queue.
    //
    // If there is available capacity, returns `Some(write_index)` to indicate the reservation was
    // successful. If the queue is full, asserts the `WAITER` flag on the reader state and returns
    // `None`, indicating the caller must wait for the reader to make space available.
    fn try_reserve_slot(
        &mut self,
        curr_write_head: State,
        curr_read_head: &mut State,
    ) -> Option<u64> {
        let write_idx = curr_write_head.index();
        loop {
            if self.inner.is_full_at(write_idx, curr_read_head.index()) {
                // Inform the reader that we are waiting for space.
                let next_read_head = curr_read_head.with_waiter();
                match self.inner.read_head_atomic().compare_exchange_weak(
                    curr_read_head.0,
                    next_read_head.0,
                    // `Ordering::Relaxed`: We are just publishing a sleep signal.
                    Ordering::Relaxed,
                    // `Ordering::Acquire`: exchange failed (reader likely popped an item) - the
                    // updated state is loaded here. This Acquire barrier ensures the reader fully
                    // completed reading the popped item before we perform any operations.
                    Ordering::Acquire,
                ) {
                    Ok(_) => return None,
                    Err(actual) => {
                        // Start over - flag or index was changed.
                        *curr_read_head = State(actual);
                    }
                }
            } else {
                // There is space available.
                return Some(write_idx);
            }
        }
    }

    // Writes the message into the specified slot. This should only be called after a successful
    // `try_reserve_slot`.
    fn push_to_slot(&mut self, msg: T, mut curr_write_head: State) -> Result<u64, zx::Status> {
        let write_idx = curr_write_head.index();

        // Write the new entry.
        let slot_ptr = self.inner.get_slot_ptr(write_idx);
        // SAFETY: `slot_ptr` was obtained from `get_slot_ptr` which enforces VMO bounds.
        // Single-producer semantics (`&mut self` on `Sender`) guarantee we solely own `write_idx`
        // without local data races. `T: IntoBytes` ensures no uninitialized padding is written,
        // preventing kernel info leaks across the boundary.
        unsafe {
            std::ptr::write(slot_ptr, msg);
        }

        // Increment the new index and wake reader if asleep.
        loop {
            let next_write_head = curr_write_head.clear_waiter_and_increment();
            match self.inner.write_head_atomic().compare_exchange_weak(
                curr_write_head.0,
                next_write_head.0,
                // `Ordering::Release`: ensures the new entry written to the VMO earlier is visible
                // to the reader before the reader sees this new index.
                Ordering::Release,
                // `Ordering::Relaxed`: exchange failed (most likely due to WAITER flag being
                // modified by the reader - the writer didn't publish anything), so no memory
                // barriers are needed before retrying.
                Ordering::Relaxed,
            ) {
                Ok(prev_write_head) => {
                    // Wake the reader if they fell asleep waiting for new items.
                    if State(prev_write_head).has_waiter() {
                        let vmo = self.inner.mapping.vmo();
                        self.inner.data_available.signal(vmo)?;
                    }
                    return Ok(write_idx);
                }
                Err(actual) => curr_write_head = State(actual),
            }
        }
    }

    fn try_allocate_payload(
        &mut self,
        size: usize,
        curr_read_head: &mut State,
    ) -> Result<Option<AllocationToken>, zx::Status> {
        if !self.allocator.is_within_capacity(size) {
            return Err(zx::Status::NO_MEMORY);
        }
        loop {
            let current_read_index = curr_read_head.index();
            self.allocator.reclaim_consumed_slots(current_read_index);
            match self.allocator.allocate(size) {
                Some(token) => return Ok(Some(token)),
                None => {
                    // Inform the reader that we are waiting for space.
                    let next_read_head = curr_read_head.with_waiter();
                    match self.inner.read_head_atomic().compare_exchange_weak(
                        curr_read_head.0,
                        next_read_head.0,
                        // `Ordering::Relaxed`: We are just publishing a sleep signal.
                        Ordering::Relaxed,
                        // `Ordering::Acquire`: exchange failed (reader likely popped an item).
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return Ok(None),
                        Err(actual) => {
                            *curr_read_head = State(actual);
                        }
                    }
                }
            }
        }
    }
}

impl<T> SenderInner<T> {
    fn shutdown(&self) -> Result<(), zx::Status> {
        self.inner.shutdown()
    }
}

impl<T> Drop for SenderInner<T> {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// The synchronous sender endpoint for a VMO-backed FIFO queue.
pub struct SyncSender<T>(SenderInner<T>);
impl<T: FromBytes + IntoBytes + KnownLayout + Copy> SyncSender<T> {
    /// Creates a new `SyncSender`.
    ///
    /// * `vmo` - The shared VMO used for queue messages and payload buffers.
    /// * `alignment` - The byte alignment boundaries required for allocations. Modulo padding
    ///                 is applied so that all payload offsets begin aligned to this size. Must be a
    ///                 power of two.
    /// * `queue_capacity` - The maximum number of queue slots.
    pub fn new(vmo: zx::Vmo, alignment: usize, queue_capacity: u32) -> Result<Self, zx::Status> {
        SenderInner::new(vmo, alignment, queue_capacity).map(Self)
    }

    pub fn capacity(&self) -> u32 {
        self.0.capacity()
    }

    pub fn is_full(&self) -> bool {
        self.0.is_full()
    }

    pub fn index(&self) -> u64 {
        self.0.index()
    }

    pub fn vmo(&self) -> &zx::Vmo {
        self.0.vmo()
    }

    pub fn shutdown(&self) -> Result<(), zx::Status> {
        self.0.shutdown()
    }

    pub fn push(&mut self, msg: T) -> Result<u64, zx::Status> {
        // `Ordering::Relaxed`: this function is called by the writer which owns the write index, no
        // synchronization needed to read its own state.
        let curr_write_head = self.0.inner.load_write_head(Ordering::Relaxed);
        // `Ordering::Acquire`: guarantees the reader has finished reading the old data before the
        // writer overwrites the slot with new data.
        let mut curr_read_head = self.0.inner.load_read_head(Ordering::Acquire);

        loop {
            // Attempt to reserve the next queue slot. If the queue is full, block and wait for the
            // reader to signal that space has become available.
            match self.0.try_reserve_slot(curr_write_head, &mut curr_read_head) {
                Some(_) => break,
                None => {
                    let vmo = self.0.inner.mapping.vmo();
                    self.0.inner.space_available.wait(vmo, SIG_SHUTDOWN)?;
                    curr_read_head = self.0.inner.load_read_head(Ordering::Acquire);
                }
            }
        }
        self.0.push_to_slot(msg, curr_write_head)
    }

    /// Reserves capacity in the payload region for a payload of `size` bytes.
    /// Returns a `PayloadBuffer` referencing the payload region so that data can be copied or
    /// written directly into it. The returned `PayloadBuffer` locks the `Sender` until the buffer
    /// is either committed or dropped, preventing multiple concurrent allocations.
    pub fn reserve_payload(&mut self, size: usize) -> Result<PayloadBuffer<'_, T>, zx::Status> {
        // `Ordering::Acquire`: ensures any data writes made by the consumer in payload space
        // prior to them updating the reader head is visible here.
        let mut curr_read_head = self.0.inner.load_read_head(Ordering::Acquire);

        let token = loop {
            // Attempt to allocate space in the payload region. If no space is available, block
            // until the reader processes a message and frees its memory block.
            match self.0.try_allocate_payload(size, &mut curr_read_head)? {
                Some(token) => break token,
                None => {
                    let vmo = self.0.inner.mapping.vmo();
                    self.0.inner.space_available.wait(vmo, SIG_SHUTDOWN)?;
                    curr_read_head = self.0.inner.load_read_head(Ordering::Acquire);
                }
            }
        };

        // SAFETY:
        // 1. `self.0.inner.addr()` is a valid base pointer to a mapped VMO memory space active
        //    for the entire lifetime of this instance.
        // 2. The `RingAllocator` math strictly guarantees that `token.offset() + size` fits
        //    within the allocated payload boundaries, preventing out-of-bounds pointer derivation.
        // 3. Returning a `MutPtrByteSlice` wrapper instead of a native `&mut [u8]` avoids violating
        //    Rust's strict aliasing rules for shared memory, thereby preventing Undefined Behavior.
        unsafe {
            let abs_offset = self.0.inner.payload_region_offset() + token.offset() as usize;
            let dest_ptr = (self.0.inner.addr() + abs_offset) as *mut u8;
            let slice = std::ptr::slice_from_raw_parts_mut(dest_ptr, size);
            Ok(PayloadBuffer {
                sender: self,
                token: Some(token),
                slice: MutPtrByteSlice::new(slice),
            })
        }
    }
}

/// The asynchronous sender endpoint for a VMO-backed FIFO queue.
pub struct AsyncSender<T>(SenderInner<T>);
impl<T: FromBytes + IntoBytes + KnownLayout + Copy> AsyncSender<T> {
    /// Creates a new `AsyncSender`.
    ///
    /// * `vmo` - The shared VMO used for queue messages and payload buffers.
    /// * `alignment` - The byte alignment boundaries required for allocations. Modulo padding
    ///                 is applied so that all payload offsets begin aligned to this size. Must be a
    ///                 power of two.
    /// * `queue_capacity` - The maximum number of queue slots.
    pub fn new(vmo: zx::Vmo, alignment: usize, queue_capacity: u32) -> Result<Self, zx::Status> {
        SenderInner::new(vmo, alignment, queue_capacity).map(Self)
    }

    /// Returns the maximum number of items the queue can hold.
    pub fn capacity(&self) -> u32 {
        self.0.capacity()
    }

    pub fn is_full(&self) -> bool {
        self.0.is_full()
    }

    pub fn vmo(&self) -> &zx::Vmo {
        self.0.vmo()
    }

    /// Pushes a standalone message onto the queue, yielding to the executor if the queue is full.
    pub async fn push(&mut self, msg: T) -> Result<u64, zx::Status> {
        // `Ordering::Relaxed`: this function is called by the writer which owns the write index, no
        // thread synchronization is needed for reading it.
        let curr_write_head = self.0.inner.load_write_head(Ordering::Relaxed);
        // `Ordering::Acquire`: guarantees the reader has finished reading the old data before the
        // writer overwrites it.
        let mut curr_read_head = self.0.inner.load_read_head(Ordering::Acquire);

        loop {
            // Attempt to reserve the next queue slot. If the queue is full, yield until the reader
            // signals that space has become available.
            match self.0.try_reserve_slot(curr_write_head, &mut curr_read_head) {
                Some(_) => break,
                None => {
                    let vmo = self.0.inner.mapping.vmo();
                    // Successfully marked writer as asleep. Yield control to the async executor
                    // so the current thread can do other work until space becomes available.
                    self.0.inner.space_available.wait_async(vmo, SIG_SHUTDOWN).await?;
                    curr_read_head = self.0.inner.load_read_head(Ordering::Acquire);
                }
            }
        }

        self.0.push_to_slot(msg, curr_write_head)
    }

    /// Reserves a payload buffer in the VMO. Yields to the executor if there is insufficient
    /// payload capacity available. Returns an `AsyncPayloadBuffer` that commits on success
    /// or rolls back on drop.
    pub async fn reserve_payload(
        &mut self,
        size: usize,
    ) -> Result<AsyncPayloadBuffer<'_, T>, zx::Status> {
        // `Ordering::Acquire`: ensures any data writes made by the consumer in payload space
        // prior to them updating the reader head is visible here.
        let mut curr_read_head = self.0.inner.load_read_head(Ordering::Acquire);
        let token = loop {
            // Attempt to allocate space in the payload region. If no contiguous space is available,
            // yield until the reader signals that space is available.
            match self.0.try_allocate_payload(size, &mut curr_read_head)? {
                Some(token) => break token,
                None => {
                    let vmo = self.0.inner.mapping.vmo();
                    self.0.inner.space_available.wait_async(vmo, SIG_SHUTDOWN).await?;
                    curr_read_head = self.0.inner.load_read_head(Ordering::Acquire);
                }
            }
        };

        // SAFETY:
        // 1. `self.0.inner.addr()` is a valid base pointer to a mapped VMO memory space active
        //    for the entire lifetime of this instance.
        // 2. The `RingAllocator` math strictly guarantees that `token.offset() + size` fits
        //    within the allocated payload boundaries, preventing out-of-bounds pointer derivation.
        // 3. Returning a `MutPtrByteSlice` wrapper instead of a native `&mut [u8]` avoids violating
        //    Rust's strict aliasing rules for shared memory, thereby preventing Undefined Behavior.
        let slice = unsafe {
            let abs_offset = self.0.inner.payload_region_offset() + token.offset() as usize;
            let dest_ptr = (self.0.inner.addr() + abs_offset) as *mut u8;
            MutPtrByteSlice::new(std::ptr::slice_from_raw_parts_mut(dest_ptr, size))
        };
        Ok(AsyncPayloadBuffer { sender: self, token: Some(token), slice })
    }
}

/// An RAII buffer for committing a payload to the FIFO. Dropping this buffer automatically rolls
/// back the payload allocation.
pub struct PayloadBuffer<'a, T: FromBytes + IntoBytes + KnownLayout + Copy> {
    // Holds an exclusive mutable borrow on the `Sender` to statically prevent the caller from
    // reserving a second payload before this one is either committed or dropped.
    sender: &'a mut SyncSender<T>,
    token: Option<AllocationToken>,
    slice: MutPtrByteSlice<'a>,
}

impl<'a, T: FromBytes + IntoBytes + KnownLayout + Copy> PayloadBuffer<'a, T> {
    /// Returns a mutable slice to the payload data.
    pub fn data(&mut self) -> &mut MutPtrByteSlice<'a> {
        &mut self.slice
    }

    /// Returns the physical byte offset of the payload in the VMO.
    pub fn offset(&self) -> u32 {
        self.token.as_ref().unwrap().offset()
    }

    /// Pushes the given message to the FIFO and commits this payload allocation, linking the
    /// payload to the queue slot.
    pub fn commit(mut self, msg: T) -> Result<(), zx::Status> {
        let token = self.token.take().unwrap();
        match self.sender.push(msg) {
            Ok(slot) => {
                self.sender.0.allocator.commit_allocation_to_slot(slot, token);
                Ok(())
            }
            Err(e) => {
                self.sender.0.allocator.cancel_allocation(token);
                Err(e)
            }
        }
    }
}

impl<'a, T: FromBytes + IntoBytes + KnownLayout + Copy> Drop for PayloadBuffer<'a, T> {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            self.sender.0.allocator.cancel_allocation(token);
        }
    }
}

/// An asynchronous version of `PayloadBuffer` that yields when committing if the queue is full.
/// Dropping this buffer automatically rolls back the payload allocation if not committed.
pub struct AsyncPayloadBuffer<'a, T: FromBytes + IntoBytes + KnownLayout + Copy> {
    sender: &'a mut AsyncSender<T>,
    token: Option<AllocationToken>,
    slice: MutPtrByteSlice<'a>,
}

impl<'a, T: FromBytes + IntoBytes + KnownLayout + Copy> AsyncPayloadBuffer<'a, T> {
    /// Returns a mutable slice to the reserved payload bytes.
    pub fn data(&mut self) -> &mut MutPtrByteSlice<'a> {
        &mut self.slice
    }

    /// Returns the absolute VMO offset where this payload reservation begins.
    pub fn offset(&self) -> u32 {
        self.token.as_ref().unwrap().offset()
    }

    /// Pushes the given message to the FIFO and commits this payload allocation, linking the
    /// payload to the queue slot. Yielding if the queue is full.
    pub async fn commit(mut self, msg: T) -> Result<(), zx::Status> {
        let token = self.token.take().unwrap();
        match self.sender.push(msg).await {
            Ok(slot) => {
                self.sender.0.allocator.commit_allocation_to_slot(slot, token);
                Ok(())
            }
            Err(e) => {
                self.sender.0.allocator.cancel_allocation(token);
                Err(e)
            }
        }
    }
}

impl<'a, T: FromBytes + IntoBytes + KnownLayout + Copy> Drop for AsyncPayloadBuffer<'a, T> {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            self.sender.0.allocator.cancel_allocation(token);
        }
    }
}

const PORT_KEY_SIGNAL: u64 = 1;
const PORT_KEY_WAKE: u64 = 2;

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
        let cap = self.inner.capacity() as u64;
        state.completed_slots[(slot_index % cap) as usize] = true;
        let mut advanced = false;
        loop {
            let committed_slot = (state.committed_read_index % cap) as usize;
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
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[derive(FromBytes, IntoBytes, KnownLayout, Clone, Copy, Debug, PartialEq)]
    #[repr(C)]
    struct TestMessage {
        val: u32,
    }

    #[fuchsia::test]
    fn test_push_pop() {
        let vmo = zx::Vmo::create(4096).expect("VMO creation failed");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate_handle failed");

        let mut sender = SyncSender::<TestMessage>::new(vmo, 1, 4).expect("Sender new failed");
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
        let vmo = zx::Vmo::create(4096).expect("VMO creation failed");
        let vmo_writer =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("VMO duplicate_handle failed");

        let mut receiver = Receiver::<TestMessage>::new(vmo, 2).expect("receiver creation failed");
        let mut sender =
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
        let vmo = zx::Vmo::create(4096).expect("VMO creation failed");

        // Seed the write and read indexes to near INDEX_MASK.
        let start_idx = State::INDEX_MASK - 1;
        // `write_index` is at byte offset 0
        vmo.write(&start_idx.to_ne_bytes(), 0).expect("write failed");
        // `read_index` is at byte offset 8
        vmo.write(&start_idx.to_ne_bytes(), 8).expect("write failed");

        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate_handle failed");

        let mut sender = SyncSender::<TestMessage>::new(vmo, 8, 4).expect("Sender new failed");
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
        let vmo = zx::Vmo::create(4096).expect("VMO creation failed");
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
        let vmo = zx::Vmo::create(4096).expect("VMO creation failed");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("VMO duplicate_handle failed");

        let mut sender = SyncSender::<TestMessage>::new(vmo, 8, 2).expect("Sender creation failed");
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
            let vmo = zx::Vmo::create(4096).unwrap();
            let vmo_dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
            let mut sender = SyncSender::<u32>::new(vmo, 4, 1).unwrap();
            let mut receiver = Receiver::<u32>::new(vmo_dup, 1).unwrap();

            let handle = std::thread::spawn(move || {
                // Spin loop until the receiver sets the WAITER flag indicating it is blocked.
                loop {
                    let write_head = sender.0.inner.load_write_head(Ordering::Acquire);
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
        let vmo = zx::Vmo::create(4096).expect("VMO creation failed");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("VMO duplicate_handle failed");

        let mut sender = SyncSender::<TestMessage>::new(vmo, 8, 4).expect("Sender creation failed");
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
        let vmo = zx::Vmo::create(4096).expect("VMO creation failed");
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

        let mut sender = SyncSender::<TestMessage>::new(vmo, 8, 8).expect("Sender creation failed");
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
}
