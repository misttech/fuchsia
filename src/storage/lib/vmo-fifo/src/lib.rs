// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod ring_allocator;
pub use ring_allocator::AllocationToken;
use ring_allocator::RingAllocator;

mod signal;
use signal::{
    EventSignal, SIG_DATA_AVAILABLE_0, SIG_DATA_AVAILABLE_1, SIG_SHUTDOWN, SIG_SPACE_AVAILABLE_0,
    SIG_SPACE_AVAILABLE_1,
};
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

/// A sender endpoint for a VMO-backed FIFO queue.
pub struct Sender<T> {
    inner: SharedQueue<T>,
    allocator: RingAllocator,
}

/// An RAII buffer for committing a payload to the FIFO. Dropping this buffer automatically rolls
/// back the payload allocation.
pub struct PayloadBuffer<'a, T: FromBytes + IntoBytes + KnownLayout + Copy> {
    // Holds an exclusive mutable borrow on the `Sender` to statically prevent the caller from
    // reserving a second payload before this one is either committed or dropped.
    sender: &'a mut Sender<T>,
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
        self.sender.push_and_commit_payload(msg, token)
    }
}

impl<'a, T: FromBytes + IntoBytes + KnownLayout + Copy> Drop for PayloadBuffer<'a, T> {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            self.sender.cancel_allocation(token);
        }
    }
}

impl<T: FromBytes + IntoBytes + KnownLayout + Copy> Sender<T> {
    /// Creates a new `Sender` endpoint mapping the provided `vmo`.
    ///
    /// * `vmo` - The shared mapping destination.
    /// * `alignment` - The byte boundaries all payload allocations must adhere to. Modulo padding
    ///                 will be applied so that all payloads begin aligned to this size. Must be a
    ///                 power of two.
    /// * `capacity` - Maximum queue node capacity.
    pub fn new(vmo: zx::Vmo, alignment: usize, capacity: u32) -> Result<Self, zx::Status> {
        let inner = SharedQueue::new(vmo, capacity)?;
        let payload_size = inner.vmo_size().saturating_sub(inner.payload_region_offset());
        let allocator = RingAllocator::new(payload_size, inner.capacity() as usize, alignment);
        Ok(Self { inner, allocator })
    }

    pub fn is_full(&self) -> bool {
        self.inner.is_full()
    }

    pub fn capacity(&self) -> u32 {
        self.inner.capacity()
    }

    pub fn index(&self) -> u64 {
        self.inner.write_index()
    }

    pub fn vmo(&self) -> &zx::Vmo {
        self.inner.vmo()
    }

    pub fn push(&mut self, msg: T) -> Result<u64, zx::Status> {
        // `Ordering::Relaxed`: this function is called by the writer which owns the write index, no
        // synchronization needed to read its own state.
        let mut curr_write_head = self.inner.load_write_head(Ordering::Relaxed);
        // `Ordering::Acquire`: guarantees the reader has finished reading the old data before the
        // writer overwrites the slot with new data.
        let mut curr_read_head = self.inner.load_read_head(Ordering::Acquire);
        let write_idx = curr_write_head.index();

        // Wait for space.
        while self.inner.is_full_at(write_idx, curr_read_head.index()) {
            // Inform the reader that we are waiting for space.
            let next_read_head = curr_read_head.with_waiter();
            match self.inner.read_head_atomic().compare_exchange_weak(
                curr_read_head.0,
                next_read_head.0,
                // `Ordering::Relaxed`: We are just publishing a sleep signal.
                Ordering::Relaxed,
                // `Ordering::Acquire`: exchange failed (reader likely popped an item) - the updated
                // state is loaded here. This Acquire barrier ensures the reader fully completed
                // reading the popped item before we perform any operations.
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Successfully marked writer as asleep. Sleep until the reader signals that
                    // space has become available.
                    let vmo = self.inner.mapping.vmo();
                    self.inner.space_available.wait(vmo, SIG_SHUTDOWN)?;
                    curr_read_head = self.inner.load_read_head(Ordering::Acquire);
                }
                Err(current) => curr_read_head = State(current),
            }
        }

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

    /// Reserves capacity in the payload region for a payload of `size` bytes.
    /// Returns a `PayloadBuffer` referencing the payload region so that data can be copied or
    /// written directly into it. The returned `PayloadBuffer` locks the `Sender` until the buffer
    /// is either committed or dropped, preventing multiple concurrent allocations.
    pub fn reserve_payload(&mut self, size: usize) -> Result<PayloadBuffer<'_, T>, zx::Status> {
        // Eagerly sync with the receiver to reclaim slots before allocating. If the queue is
        // routinely emptied, this allows the allocator to continually reuse the same physical
        // memory addresses, maximizing CPU cache hits.
        let current_read_index = self.inner.read_index();
        self.allocator.reclaim_consumed_slots(current_read_index);

        let token = match self.allocator.allocate(size) {
            Some(token) => token,
            None => return Err(zx::Status::NO_MEMORY),
        };

        // SAFETY:
        // 1. `self.inner.addr()` is a valid base pointer to a mapped VMO memory space active
        //    for the entire lifetime of this instance.
        // 2. The `RingAllocator` math strictly guarantees that `token.offset() + size` fits
        //    within the allocated payload boundaries, preventing out-of-bounds pointer derivation.
        // 3. Returning a `MutPtrByteSlice` wrapper instead of a native `&mut [u8]` avoids violating
        //    Rust's strict aliasing rules for shared memory, thereby preventing Undefined Behavior.
        unsafe {
            let abs_offset = self.inner.payload_region_offset() + token.offset() as usize;
            let dest_ptr = (self.inner.addr() + abs_offset) as *mut u8;
            let slice = std::ptr::slice_from_raw_parts_mut(dest_ptr, size);
            Ok(PayloadBuffer {
                sender: self,
                token: Some(token),
                slice: MutPtrByteSlice::new(slice),
            })
        }
    }

    // Rolls back an uncommitted `AllocationToken`. Called automatically by `PayloadBuffer` drop.
    fn cancel_allocation(&mut self, token: AllocationToken) {
        self.allocator.cancel_allocation(token);
    }

    // Pushes the message and commits its payload token to the newly written queue slot. This links
    // the memory reservation to the queue's FIFO lifecycle, ensuring that the allocator safely
    // reclaims the bytes exactly when the receiver pops this message.
    fn push_and_commit_payload(
        &mut self,
        msg: T,
        token: AllocationToken,
    ) -> Result<(), zx::Status> {
        // Note: `self.push` executes an `Ordering::Release` on the atomic write head, which acts
        // as a memory fence guaranteeing our payload bytes above are fully visible to the reader.
        match self.push(msg) {
            Ok(slot) => {
                self.allocator.commit_allocation_to_slot(slot, token);
                Ok(())
            }
            Err(e) => {
                self.allocator.cancel_allocation(token);
                Err(e)
            }
        }
    }
}

impl<T> Sender<T> {
    pub fn shutdown(&self) -> Result<(), zx::Status> {
        self.inner.shutdown()
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// A receiver endpoint for a VMO-backed FIFO queue.
pub struct Receiver<T> {
    inner: SharedQueue<T>,
    cached_read_index: u64,
}

impl<T: FromBytes + IntoBytes + KnownLayout + Copy> Receiver<T> {
    pub fn new(vmo: zx::Vmo, capacity: u32) -> Result<Self, zx::Status> {
        Ok(Self { inner: SharedQueue::new(vmo, capacity)?, cached_read_index: 0 })
    }

    // Creates a Receiver that initializes its cached index from the provided VMO. This is solely
    // used for tests. Receiver should normally not support a populated VMO.
    #[cfg(test)]
    fn new_from_populated_vmo(vmo: zx::Vmo, capacity: u32) -> Result<Self, zx::Status> {
        let inner = SharedQueue::new(vmo, capacity)?;
        let cached_read_index = inner.read_index();
        Ok(Self { inner, cached_read_index })
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

    /// Fetches the next message without incrementing the read index.
    pub fn pop_reserve(&mut self) -> Result<T, zx::Status> {
        let read_idx = self.cached_read_index;

        // `Ordering::Acquire`: guarantees the writer has finished writing the new data to the
        // VMO before the reader attempts to read it.
        let mut curr_write_head = self.inner.load_write_head(Ordering::Acquire);

        loop {
            if read_idx != curr_write_head.index() {
                let slot_ptr = self.inner.get_slot_ptr(read_idx);
                // SAFETY: `slot_ptr` enforces VMO bounds. The `Acquire` ordering guarantees the
                // sender has fully completed writing before we read. Furthermore, `T: FromBytes`
                // ensures any arbitrary bits provided by Fxfs safely map to a valid Rust struct.
                let msg = unsafe { std::ptr::read(slot_ptr) };
                return Ok(msg);
            }

            // The queue is empty. Inform the writer that we are going to sleep and waiting for a
            // signal when new items are pushed.
            let asleep_write_head = curr_write_head.with_waiter();
            match self.inner.write_head_atomic().compare_exchange_weak(
                curr_write_head.0,
                asleep_write_head.0,
                // `Ordering::Relaxed`: We are just publishing a sleep signal. There is no
                // accompanying VMO data payload that requires a memory barrier to be made visible
                // to the writer.
                Ordering::Relaxed,
                // `Ordering::Acquire`: exchange failed (writer likely pushed an item) - the updated
                // state is loaded here. This Acquire barrier ensures the writer fully completed
                // writing the pushed item before we perform any operations.
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // Successfully marked reader as asleep. Sleep until the writer signals that new
                    // items have been pushed.
                    let vmo = self.inner.mapping.vmo();
                    match self.inner.data_available.wait(vmo, SIG_SHUTDOWN) {
                        Ok(()) => curr_write_head = self.inner.load_write_head(Ordering::Acquire),
                        Err(zx::Status::CANCELED) => {
                            // The Sender has shut down. Check one last time if they pushed items
                            // just before dying. If the queue is truly empty, return CANCELED.
                            curr_write_head = self.inner.load_write_head(Ordering::Acquire);
                            if read_idx == curr_write_head.index() {
                                return Err(zx::Status::CANCELED);
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
                Err(actual) => curr_write_head = State(actual),
            }
        }
    }

    /// Commit the read, freeing the slot.
    pub fn pop_commit(&mut self) -> Result<(), zx::Status> {
        // Note that the `WAITER` flag in `read_index` may have been set by the writer if the queue
        // became full, in which case the `compare_exchange_weak` will fail and we will retry. This
        // case doesn't happen often, however, so we optimistically use `cached_read_index` to
        // reduce the number of atomic loads for the common path.
        let mut curr_read_head = State(self.cached_read_index);
        loop {
            let next_read_head = curr_read_head.clear_waiter_and_increment();
            match self.inner.read_head_atomic().compare_exchange_weak(
                curr_read_head.0,
                next_read_head.0,
                // `Ordering::Release`: ensure the reader has finished reading the old data from the
                // VMO before the writer sees this new index and overwrites the slot.
                Ordering::Release,
                // `Ordering::Relaxed`: swap failed (most likely due to WAITER flag being modified -
                // the reader didn't publish anything), so no memory barriers are needed before
                // retrying.
                Ordering::Relaxed,
            ) {
                Ok(prev_read_head) => {
                    self.cached_read_index = next_read_head.index();
                    // Wake the writer if they fell asleep waiting for a free slot.
                    if State(prev_read_head).has_waiter() {
                        let vmo = self.inner.mapping.vmo();
                        self.inner.space_available.signal(vmo)?;
                    }
                    return Ok(());
                }
                Err(actual) => curr_read_head = State(actual),
            }
        }
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

        let mut sender = Sender::<TestMessage>::new(vmo, 1, 4).expect("Sender new failed");
        let mut receiver = Receiver::<TestMessage>::new(vmo_dup, 4).expect("Receiver new failed");

        assert!(receiver.is_empty());
        assert!(!sender.is_full());

        let msg = TestMessage { val: 42 };
        sender.push(msg).expect("push failed");

        assert!(!receiver.is_empty());

        let popped = receiver.pop_reserve().expect("pop_reserve failed");
        assert_eq!(popped, msg);

        receiver.pop_commit().expect("pop_commit failed");
        assert!(receiver.is_empty());
    }

    #[fuchsia::test]
    fn test_blocking_push_pop() {
        let vmo = zx::Vmo::create(4096).expect("VMO creation failed");
        let vmo_writer =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("VMO duplicate_handle failed");

        let mut receiver = Receiver::<TestMessage>::new(vmo, 2).expect("receiver creation failed");
        let mut sender =
            Sender::<TestMessage>::new(vmo_writer, 8, 2).expect("sender creation failed");

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
        std::thread::sleep(std::time::Duration::from_millis(100));

        let r1 = receiver.pop_reserve().expect("pop_reserve failed");
        assert_eq!(r1.val, 1);
        // Committing this pop frees a slot, waking the blocked sender.
        receiver.pop_commit().expect("pop_commit failed");

        let r2 = receiver.pop_reserve().expect("pop_reserve failed");
        assert_eq!(r2.val, 2);
        receiver.pop_commit().expect("pop_commit failed");

        let r3 = receiver.pop_reserve().expect("pop_reserve failed");
        assert_eq!(r3.val, 3);
        receiver.pop_commit().expect("pop_commit failed");

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

        let mut sender = Sender::<TestMessage>::new(vmo, 8, 4).expect("Sender new failed");
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
        assert_eq!(receiver.pop_reserve().expect("pop_reserve failed").val, 10);
        receiver.pop_commit().expect("pop_commit failed");
        assert_eq!(receiver.index(), State::INDEX_MASK);

        assert_eq!(receiver.pop_reserve().expect("pop_reserve failed").val, 20);
        receiver.pop_commit().expect("pop_commit failed");
        assert_eq!(receiver.index(), 0);

        assert_eq!(receiver.pop_reserve().expect("pop_reserve failed").val, 30);
        receiver.pop_commit().expect("pop_commit failed");
        assert_eq!(receiver.index(), 1);

        assert!(receiver.is_empty());
    }

    #[fuchsia::test]
    fn test_receiver_wakes_on_sender_drop() {
        let vmo = zx::Vmo::create(4096).expect("VMO creation failed");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("VMO duplicate_handle failed");

        let sender = Sender::<TestMessage>::new(vmo, 8, 2).expect("Sender creation failed");
        let mut receiver =
            Receiver::<TestMessage>::new(vmo_dup, 2).expect("Receiver creation failed");

        let handle = std::thread::spawn(move || {
            // Receiver blocks because the queue is empty
            receiver.pop_reserve()
        });

        // Give the receiver thread a moment to block on wait()
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Dropping sender should trigger shutdown
        drop(sender);

        let result = handle.join().expect("thread panicked");
        assert_eq!(result, Err(zx::Status::CANCELED));
    }

    #[fuchsia::test]
    fn test_sender_wakes_on_receiver_drop() {
        let vmo = zx::Vmo::create(4096).expect("VMO creation failed");
        let vmo_dup =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("VMO duplicate_handle failed");

        let mut sender = Sender::<TestMessage>::new(vmo, 8, 2).expect("Sender creation failed");
        let receiver = Receiver::<TestMessage>::new(vmo_dup, 2).expect("Receiver creation failed");

        // Fill up the queue so the next push blocks
        sender.push(TestMessage { val: 1 }).expect("push failed");
        sender.push(TestMessage { val: 2 }).expect("push failed");

        let handle = std::thread::spawn(move || {
            // Sender blocks because the queue is full
            sender.push(TestMessage { val: 3 })
        });

        // Give the sender thread a moment to block on wait()
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Dropping receiver should trigger shutdown
        drop(receiver);

        let result = handle.join().expect("thread panicked");
        assert_eq!(result, Err(zx::Status::CANCELED));
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

        let mut sender = Sender::<WireTestCommand>::new(vmo, 8, 4).expect("Sender creation failed");

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

        let mut sender = Sender::<WireTestCommand>::new(vmo, 8, 4).expect("Sender creation failed");

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

        let mut sender = Sender::<WireTestCommand>::new(vmo, 8, 4).expect("Sender creation failed");
        let mut receiver =
            Receiver::<WireTestCommand>::new(vmo_dup, 4).expect("Receiver creation failed");

        let mut buffer = sender.reserve_payload(10).expect("reserve failed");
        buffer.data().fill(0xAA);
        let wire_msg = TestCommand::WithPayload { vmo_offset: buffer.offset(), len: 10 }.to_wire();
        buffer.commit(wire_msg).expect("commit failed");

        let standalone = TestCommand::NoPayload { val: 456 };
        sender.push(standalone.to_wire()).expect("push failed");

        let msg1 = receiver.pop_reserve().expect("pop_reserve failed");
        let app_msg1 = TestCommand::from_wire(msg1);
        if let TestCommand::WithPayload { vmo_offset, len } = app_msg1 {
            assert_eq!(len, 10);
            let read_data = receiver.payload_slice(vmo_offset, len);
            assert_eq!(read_data.to_vec(), vec![0xAA; 10]);
        } else {
            panic!("Expected TestCommand::WithPayload");
        }
        receiver.pop_commit().expect("pop_commit failed");

        let msg2 = receiver.pop_reserve().expect("pop_reserve failed");
        let app_msg2 = TestCommand::from_wire(msg2);
        assert_eq!(app_msg2, TestCommand::NoPayload { val: 456 });
        receiver.pop_commit().expect("pop_commit failed");
    }

    #[fuchsia::test]
    fn test_pop_handles_canceled_race_condition() {
        // Run ten thousand times to force thread-scheduler starvation naturally
        for _ in 0..10000 {
            let vmo = zx::Vmo::create(4096).unwrap();
            let vmo_dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
            let mut sender = Sender::<u32>::new(vmo, 4, 1).unwrap();
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
            let val = receiver.pop_reserve().expect("pop_reserve failed on race condition");
            assert_eq!(val, 42);
            receiver.pop_commit().expect("pop_commit failed");

            // The queue is now genuinely empty and the peer has dropped.
            assert_eq!(receiver.pop_reserve().unwrap_err(), zx::Status::CANCELED);

            handle.join().unwrap();
        }
    }
}
