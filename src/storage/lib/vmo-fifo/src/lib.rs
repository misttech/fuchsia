// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::atomic::{AtomicU64, Ordering};
use zerocopy::{FromBytes, IntoBytes, KnownLayout};

mod signal;
use crate::signal::EventSignal;
pub use crate::signal::{
    SIG_DATA_AVAILABLE_0, SIG_DATA_AVAILABLE_1, SIG_SHUTDOWN, SIG_SPACE_AVAILABLE_0,
    SIG_SPACE_AVAILABLE_1,
};

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

    // Returns a slice to the payload region (the portion of the VMO after the slots).
    fn payload_region(&self) -> &[u8] {
        let offset = std::cmp::min(self.payload_region_offset(), self.vmo_size());
        // SAFETY: The mapping is valid for `self.vmo_size()` bytes.
        unsafe {
            std::slice::from_raw_parts(
                (self.addr() + offset) as *const u8,
                self.vmo_size() - offset,
            )
        }
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
}

impl<T: FromBytes + IntoBytes + KnownLayout + Copy> Sender<T> {
    pub fn new(vmo: zx::Vmo, capacity: u32) -> Result<Self, zx::Status> {
        Ok(Self { inner: SharedQueue::new(vmo, capacity)? })
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

    pub fn payload_region(&self) -> &[u8] {
        self.inner.payload_region()
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
    pub(crate) fn new_from_populated_vmo(vmo: zx::Vmo, capacity: u32) -> Result<Self, zx::Status> {
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

    pub fn payload_region(&self) -> &[u8] {
        self.inner.payload_region()
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
                    self.inner.data_available.wait(vmo, SIG_SHUTDOWN)?;
                    curr_write_head = self.inner.load_write_head(Ordering::Acquire);
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
        let vmo_dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Duplicate failed");

        let mut sender = Sender::<TestMessage>::new(vmo, 8).expect("Sender creation failed");
        let mut receiver =
            Receiver::<TestMessage>::new(vmo_dup, 8).expect("Receiver creation failed");

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
    fn test_payload_region() {
        let vmo = zx::Vmo::create(8192).expect("VMO creation failed");
        let vmo_dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Duplicate failed");

        let sender = Sender::<TestMessage>::new(vmo, 8).expect("Sender creation failed");

        let offset = sender.inner.payload_region_offset();
        assert!(offset < sender.inner.vmo_size(), "Payload region offset should be within VMO");

        // Write some recognizable bytes to the VMO at the payload offset
        let test_data = [0xDE, 0xAD, 0xBE, 0xEF];
        vmo_dup.write(&test_data, offset as u64).expect("VMO write failed");

        // Verify that `payload_region()` returns a slice that includes our written data
        let region = sender.payload_region();
        assert_eq!(&region[0..test_data.len()], &test_data);
    }

    #[fuchsia::test]
    fn test_blocking_push_pop() {
        let vmo = zx::Vmo::create(4096).expect("VMO creation failed");
        let vmo_writer =
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("VMO handle duplication failed");

        let mut receiver = Receiver::<TestMessage>::new(vmo, 2).expect("receiver creation failed");
        let mut sender = Sender::<TestMessage>::new(vmo_writer, 2).expect("sender creation failed");

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

        let vmo_dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Duplicate failed");

        let mut sender = Sender::<TestMessage>::new(vmo, 4).expect("Sender creation failed");
        let mut receiver = Receiver::<TestMessage>::new_from_populated_vmo(vmo_dup, 4)
            .expect("Receiver creation failed");

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
        let vmo_dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Duplicate failed");

        let sender = Sender::<TestMessage>::new(vmo, 2).expect("Sender creation failed");
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
        let vmo_dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("Duplicate failed");

        let mut sender = Sender::<TestMessage>::new(vmo, 2).expect("Sender creation failed");
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
}
