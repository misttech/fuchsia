// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

use arch_rs::{curr_cpu_num, ints_disabled};
use core::cell::UnsafeCell;
use core::mem::{MaybeUninit, size_of};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use core::{ffi, ptr, slice};
use kalloc::Box;
use kstring::interned_category::InternedCategory;
use kstring::{declare_interned_category, declare_interned_string};
use platform_rs::timer_current_boot_ticks;
use spsc_buffer::{Buffer, NoOpAllocator, Reservation};
use zx_status::Status;

// LINT.IfChange(KTraceState)
#[repr(C)]
struct KTraceState {
    categories_bitmask: AtomicU32,
    writes_enabled: AtomicBool,
}
const _: () = assert!(size_of::<KTraceState>() == 8);
// LINT.ThenChange(//zircon/kernel/include/lib/ktrace.h:KTraceState)

declare_interned_category!(META_CAT, "kernel:meta", extern);
declare_interned_string!(DROP_STATS_REF, "drop_stats", extern);
declare_interned_string!(NUM_RECORDS_REF, "num_records", extern);
declare_interned_string!(NUM_BYTES_REF, "num_bytes", extern);

// LINT.IfChange(DroppedRecordDurationEvent)
#[repr(C)]
struct DroppedRecordDurationEvent {
    header: u64,
    start: i64,
    process_id: u64,
    thread_id: u64,
    num_dropped_arg: u64,
    bytes_dropped_arg: u64,
    end: i64,
}
const _: () = assert!(size_of::<DroppedRecordDurationEvent>() == 56);
// LINT.ThenChange(//zircon/kernel/lib/percpu_writer/include/lib/percpu_writer/buffer.h:DroppedRecordDurationEvent)

// LINT.IfChange(DroppedRecordStats)
/// This struct keeps track of the duration, number, and size of trace records dropped when the
/// buffer is full. These statistics are emitted to the trace buffer as a duration as soon as
/// space is available to do so, at which point the values are reset to 0, or false in the case
/// of has_dropped.
#[repr(C)]
#[derive(Default, Debug, Clone)]
struct DroppedRecordStats {
    first_dropped: i64,
    last_dropped: i64,

    /// By storing num_dropped and bytes_dropped in 32-bit values, we ensure that they can each
    /// be stored in a single 64-bit word in the FXT record we emit when space is available.
    num_dropped: u32,
    bytes_dropped: u32,
    has_dropped: bool,
}
const _: () = assert!(size_of::<DroppedRecordStats>() == 32);
// LINT.ThenChange(//zircon/kernel/lib/percpu_writer/include/lib/percpu_writer/buffer.h:DroppedRecordStats)

impl DroppedRecordStats {
    fn reset(&mut self) {
        self.first_dropped = 0;
        self.last_dropped = 0;
        self.num_dropped = 0;
        self.bytes_dropped = 0;
        self.has_dropped = false;
    }

    fn track(&mut self, now: i64, size: u32) {
        if !self.has_dropped {
            self.first_dropped = now;
            self.has_dropped = true;
        }
        self.last_dropped = now;
        self.num_dropped = self.num_dropped.wrapping_add(1);
        self.bytes_dropped = self.bytes_dropped.wrapping_add(size);
    }

    fn has_dropped(&self) -> bool {
        self.has_dropped
    }
}

/// A Rust implementation of `percpu_writer::Buffer` that wraps a static reference to an existing
/// `spsc_buffer::Buffer` and tracks dropped trace records.
pub struct KTraceBuffer {
    buffer: &'static mut Buffer<NoOpAllocator>,
    drop_stats: &'static mut DroppedRecordStats,
    size: u32,
    cpu_ref_header_entry: u16,
    process_koid: u64,
    thread_koid: u64,
}

// SAFETY: Access to the per-CPU buffers is synchronized by the caller (typically via disabling
// interrupts).
unsafe impl Send for KTraceBuffer {}
unsafe impl Sync for KTraceBuffer {}

impl KTraceBuffer {
    /// Constructs a `KTraceBuffer` from a static reference to an existing `spsc_buffer::Buffer`,
    /// a static reference to `DroppedRecordStats`, and its metadata.
    fn new(
        buffer: &'static mut Buffer<NoOpAllocator>,
        drop_stats: &'static mut DroppedRecordStats,
        cpu_ref_header_entry: u16,
        process_koid: u64,
        thread_koid: u64,
    ) -> Self {
        let size = buffer.size();
        Self { buffer, drop_stats, size, cpu_ref_header_entry, process_koid, thread_koid }
    }

    /// Returns the size of the backing storage.
    pub fn size(&self) -> u32 {
        self.size
    }

    /// Drains the underlying Buffer.
    pub fn drain(&self) -> Result<(), Status> {
        self.buffer.drain()
    }

    /// Copies `len` bytes out of the buffer using the provided `copy_fn`.
    pub fn read<F>(&self, copy_fn: F, len: u32) -> Result<u32, Status>
    where
        F: FnMut(u32, &[u8]) -> Result<(), Status>,
    {
        self.buffer.read(copy_fn, len)
    }

    /// Reserves a block of the given size in the buffer, interposing to write dropped record
    /// statistics if any were tracked.
    pub fn reserve(&mut self, header: u64) -> Result<KTraceReservation<'_>, Status> {
        debug_assert!(ints_disabled());
        // Compute the number of bytes we need to reserve from the provided fxt header.
        let record_type = (header & 0xf) as u32;
        let num_words = if record_type == 15 {
            // Large record
            ((header >> 4) & 0xffffffff) as u32
        } else {
            // Normal record
            ((header >> 4) & 0xfff) as u32
        };
        let size = num_words * 8;

        let mut total_size = size;
        let event = if self.drop_stats.has_dropped() {
            total_size += size_of::<DroppedRecordDurationEvent>() as u32;
            Some(self.serialize_drop_stats())
        } else {
            None
        };

        match self.buffer.reserve(total_size) {
            Err(status) => {
                let now = timer_current_boot_ticks();
                self.drop_stats.track(now, size);
                Err(status)
            }
            Ok(mut res) => {
                if let Some(event) = event {
                    // SAFETY: DroppedRecordDurationEvent is repr(C) and has a defined binary
                    // layout.
                    let event_bytes = unsafe {
                        slice::from_raw_parts(
                            ptr::from_ref(&event).cast::<u8>(),
                            size_of::<DroppedRecordDurationEvent>(),
                        )
                    };
                    res.write(event_bytes)?;
                    self.drop_stats.reset();
                }
                Ok(KTraceReservation::new(res, header))
            }
        }
    }

    /// Emit the dropped record stats to the trace buffer if we're currently tracking them.
    pub fn emit_drop_stats(&mut self) -> Result<(), Status> {
        debug_assert!(ints_disabled());
        if !self.drop_stats.has_dropped() {
            return Ok(());
        }

        // Serialize the event first so we release the borrow on self before calling reserve.
        let event = self.serialize_drop_stats();

        let mut res = self.buffer.reserve(size_of::<DroppedRecordDurationEvent>() as u32)?;
        // SAFETY: DroppedRecordDurationEvent is repr(C) and has a defined binary layout.
        let event_bytes = unsafe {
            slice::from_raw_parts(
                ptr::from_ref(&event).cast::<u8>(),
                size_of::<DroppedRecordDurationEvent>(),
            )
        };
        res.write(event_bytes)?;
        res.commit()?;

        // Reset the fields directly rather than calling reset_drop_stats() to avoid
        // borrowing the entire self while res is in scope.
        self.drop_stats.reset();
        Ok(())
    }

    /// Resets the dropped records statistics to their initial values.
    pub fn reset_drop_stats(&mut self) {
        self.drop_stats.reset();
    }

    fn serialize_drop_stats(&self) -> DroppedRecordDurationEvent {
        let mut header = 4u64; // RecordType::kEvent (4)
        let record_size_words = (size_of::<DroppedRecordDurationEvent>() / 8) as u64;
        header |= record_size_words << 4; // RecordSize
        header |= 4u64 << 16; // EventType::kDurationComplete (4)
        header |= 2u64 << 20; // ArgumentCount = 2
        header |= (self.cpu_ref_header_entry as u64) << 24;
        header |= (META_CAT.label().id() as u64) << 32;
        header |= (DROP_STATS_REF.id() as u64) << 48;

        // Pack the arguments.
        // In FXT:
        // ArgumentType::kUint32 is 2
        // ArgumentSize is 1 word (8 bytes)
        // NameRef is packed into bits 16..31
        // Value is packed into bits 32..63
        let mut num_dropped_arg = 2u64;
        num_dropped_arg |= 1u64 << 4;
        num_dropped_arg |= (NUM_RECORDS_REF.id() as u64) << 16;
        num_dropped_arg |= (self.drop_stats.num_dropped as u64) << 32;

        let mut bytes_dropped_arg = 2u64;
        bytes_dropped_arg |= 1u64 << 4;
        bytes_dropped_arg |= (NUM_BYTES_REF.id() as u64) << 16;
        bytes_dropped_arg |= (self.drop_stats.bytes_dropped as u64) << 32;

        DroppedRecordDurationEvent {
            header,
            start: self.drop_stats.first_dropped,
            process_id: self.process_koid,
            thread_id: self.thread_koid,
            num_dropped_arg,
            bytes_dropped_arg,
            end: self.drop_stats.last_dropped,
        }
    }
}

/// KTraceReservation encapsulates a pending write to the buffer.
#[derive(Debug)]
pub struct KTraceReservation<'a> {
    reservation: Reservation<'a>,
}

impl<'a> KTraceReservation<'a> {
    fn new(reservation: Reservation<'a>, header: u64) -> Self {
        let mut this = Self { reservation };
        let _ = this.write_word(header);
        this
    }

    /// Writes a single 64-bit word into the reservation.
    pub fn write_word(&mut self, word: u64) -> Result<(), Status> {
        debug_assert!(ints_disabled());
        self.reservation.write(&word.to_ne_bytes())
    }

    /// Writes a byte slice into the reservation, padding to an 8-byte boundary.
    pub fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), Status> {
        debug_assert!(ints_disabled());
        self.reservation.write(bytes)?;
        let num_bytes = bytes.len();
        let aligned_bytes = (num_bytes + 7) & !7;
        let num_zeros_to_write = aligned_bytes - num_bytes;
        if num_zeros_to_write > 0 {
            let zero = [0u8; 8];
            self.reservation.write(&zero[..num_zeros_to_write])?;
        }
        Ok(())
    }

    /// Commits the reservation, making it visible to the reader.
    pub fn commit(self) -> Result<(), Status> {
        debug_assert!(ints_disabled());
        self.reservation.commit()
    }
}

/// A pure Rust implementation of KTrace.
pub struct KTrace {
    /// Reference to the shared C++ KTraceState.
    // TODO(https://fxbug.dev/517305548): This should be made a direct allocation, and not a
    // reference, once the C++ implementation is removed.
    state: &'static KTraceState,

    /// A heap-allocated slice of atomic pointers to per-CPU buffers.
    /// Allocated once at boot time, and accessed completely lock-free on the hot path by CPU ID.
    buffers: Box<[AtomicPtr<KTraceBuffer>]>,
}

// SAFETY: KTrace is a global singleton. Access to the per-CPU buffers is synchronized.
unsafe impl Sync for KTrace {}
unsafe impl Send for KTrace {}

#[repr(transparent)]
struct KTraceSingleton(UnsafeCell<MaybeUninit<KTrace>>);

unsafe impl Sync for KTraceSingleton {}
unsafe impl Send for KTraceSingleton {}

static INSTANCE: KTraceSingleton = KTraceSingleton(UnsafeCell::new(MaybeUninit::uninit()));

impl KTrace {
    /// Retrieve the global instance of KTrace.
    pub fn get_instance() -> &'static Self {
        // SAFETY: KTrace must be initialized during kernel boot.
        unsafe { &*INSTANCE.0.get().cast::<KTrace>() }
    }

    /// Reserves a slot of memory to write a record into.
    ///
    /// # Safety
    ///
    /// This method MUST be invoked with interrupts disabled to enforce the single-writer invariant.
    pub unsafe fn reserve(&self, header: u64) -> Result<KTraceReservation<'_>, Status> {
        debug_assert!(ints_disabled());
        if !self.writes_enabled() {
            return Err(Status::BAD_STATE);
        }

        // Direct, lock-free indexing into the slice, followed by loading the atomic pointer!
        let ptr = self.buffers[curr_cpu_num() as usize].load(Ordering::Acquire);
        if ptr.is_null() {
            return Err(Status::BAD_STATE);
        }

        let buf = unsafe { &mut *ptr };
        buf.reserve(header)
    }

    /// Returns true if writes are currently enabled.
    pub fn writes_enabled(&self) -> bool {
        self.state.writes_enabled.load(Ordering::Acquire)
    }

    /// Returns the categories bitmask.
    pub fn categories_bitmask(&self) -> u32 {
        self.state.categories_bitmask.load(Ordering::Acquire)
    }

    /// Returns true if the given category is enabled.
    pub fn is_category_enabled(&self, category: &InternedCategory) -> bool {
        let category_index = category.index();
        if category_index == InternedCategory::INVALID_INDEX {
            return false;
        }
        let bitmask = self.categories_bitmask();
        (bitmask & (1 << category_index)) != 0
    }
}

/// Initializes the global KTrace instance with the given number of CPU buffers.
///
/// # Safety
///
/// This must be called at most once during kernel boot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_ktrace_init(num_buffers: u32, state_ptr: *mut ffi::c_void) -> i32 {
    if num_buffers == 0 || state_ptr.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }

    // SAFETY: The caller guarantees that state_ptr points to a valid KTraceState instance
    // which has static storage duration (lives forever) and is safe to access concurrently
    // (since its fields are atomic).
    let state = unsafe { &*state_ptr.cast::<KTraceState>() };

    let buffers = match Box::<[AtomicPtr<KTraceBuffer>]>::try_new_zeroed_slice(num_buffers as usize)
    {
        Ok(b) => b,
        Err(_) => return Status::NO_MEMORY.into_raw(),
    };

    let ktrace = KTrace { state, buffers };

    unsafe {
        let slot = INSTANCE.0.get();
        ptr::write(slot.cast::<KTrace>(), ktrace);
    }

    Status::OK.into_raw()
}

/// Initializes the KTraceBuffer for a specific CPU using a pointer to the C++ SpscBuffer.
///
/// # Safety
///
/// - `spsc_buffer_ptr` must point to a valid C++ `SpscBuffer` instance
///   which is binary-compatible with `spsc_buffer::Buffer<NoOpAllocator>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_ktrace_init_cpu_buffer(
    cpu_num: u32,
    spsc_buffer_ptr: *mut ffi::c_void,
    drop_stats_ptr: *mut ffi::c_void,
    process_koid: u64,
    thread_koid: u64,
    cpu_ref_header_entry: u16,
) -> i32 {
    let ktrace = KTrace::get_instance();
    if cpu_num >= ktrace.buffers.len() as u32 {
        return Status::INVALID_ARGS.into_raw();
    }

    // SAFETY: The caller guarantees the pointers are valid, 'static, and binary-compatible with
    // their respective Rust types.
    let (buffer, drop_stats) = unsafe {
        (
            &mut *spsc_buffer_ptr.cast::<Buffer<NoOpAllocator>>(),
            &mut *drop_stats_ptr.cast::<DroppedRecordStats>(),
        )
    };

    // Allocate the KTraceBuffer on the heap.
    let buf =
        KTraceBuffer::new(buffer, drop_stats, cpu_ref_header_entry, process_koid, thread_koid);

    let boxed_buf = match Box::try_new(buf) {
        Ok(b) => b,
        Err(_) => return Status::NO_MEMORY.into_raw(),
    };

    let raw_ptr = Box::into_raw(boxed_buf);

    // Store the pointer atomically.
    let old_ptr = ktrace.buffers[cpu_num as usize].swap(raw_ptr, Ordering::AcqRel);
    if !old_ptr.is_null() {
        // If there was a previous buffer, reclaim and drop it.
        unsafe {
            let _ = Box::from_raw(old_ptr);
        }
    }

    Status::OK.into_raw()
}

#[cfg(test)]
mod tests {
    use super::*;
    use arch_rs::{InterruptDisableGuard, max_num_cpus};

    declare_interned_category!(META_CAT, "kernel:meta", extern);
    declare_interned_category!(MEMORY_CAT, "kernel:memory", extern);
    declare_interned_category!(SCHED_CAT, "kernel:sched", extern);
    declare_interned_category!(CONTENTION_CAT, "kernel:contention", extern);
    declare_interned_category!(IPC_CAT, "kernel:ipc", extern);
    declare_interned_category!(IRQ_CAT, "kernel:irq", extern);
    declare_interned_string!(DROP_STATS_REF, "drop_stats", extern);
    declare_interned_string!(NUM_RECORDS_REF, "num_records", extern);
    declare_interned_string!(NUM_BYTES_REF, "num_bytes", extern);

    //
    // Zircon kernel-tests FFI entry points.
    //

    /// Test-only FFI helper to write a single word record from Rust.
    ///
    /// # Safety
    ///
    /// This must be called with interrupts disabled.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn rust_ktrace_test_interop(header: u64, val: u64) -> i32 {
        let ktrace = KTrace::get_instance();
        // SAFETY: The caller guarantees interrupts are disabled.
        if let Ok(mut res) = unsafe { ktrace.reserve(header) } {
            let _ = res.write_word(val);
            let _ = res.commit();
            0
        } else {
            -1
        }
    }

    /// Verifies KTraceBuffer initialization and size/metadata attributes.
    #[unsafe(no_mangle)]
    pub extern "C" fn rust_ktrace_test_init_and_size() -> bool {
        let mut storage = [0u8; 256];
        let mut inner_buf = unsafe { Buffer::from_raw_parts(storage.as_mut_ptr(), storage.len()) };
        let leaked_ref = unsafe { &mut *ptr::from_mut(&mut inner_buf) };
        let mut stats = DroppedRecordStats::default();
        let leaked_stats = unsafe { &mut *ptr::from_mut(&mut stats) };
        let kbuf = KTraceBuffer::new(leaked_ref, leaked_stats, 1, 100, 200);

        if kbuf.size() != 256 {
            return false;
        }
        if kbuf.process_koid != 100 {
            return false;
        }
        if kbuf.thread_koid != 200 {
            return false;
        }
        true
    }

    /// Verifies KTraceBuffer reservation, writing words, and committing.
    #[unsafe(no_mangle)]
    pub extern "C" fn rust_ktrace_test_write() -> bool {
        let _guard = InterruptDisableGuard::new();
        let mut storage = [0u8; 256];
        let mut inner_buf = unsafe { Buffer::from_raw_parts(storage.as_mut_ptr(), storage.len()) };
        let leaked_ref = unsafe { &mut *ptr::from_mut(&mut inner_buf) };
        let mut stats = DroppedRecordStats::default();
        let leaked_stats = unsafe { &mut *ptr::from_mut(&mut stats) };
        let mut kbuf = KTraceBuffer::new(leaked_ref, leaked_stats, 1, 100, 200);

        // Reserve 16 bytes (2 words).
        let header = 4u64 | (2u64 << 4);
        let mut res = match kbuf.reserve(header) {
            Ok(r) => r,
            Err(_) => return false,
        };

        // Write a word (8 bytes).
        if res.write_word(0xabcdef0123456789).is_err() {
            return false;
        }
        if res.commit().is_err() {
            return false;
        }

        // Read it back.
        let mut read_bytes = [0u8; 16];
        let read_len = match kbuf.read(
            |offset, src| {
                read_bytes[offset as usize..offset as usize + src.len()].copy_from_slice(src);
                Ok(())
            },
            16,
        ) {
            Ok(l) => l,
            Err(_) => return false,
        };

        if read_len != 16 {
            return false;
        }
        if u64::from_ne_bytes(read_bytes[0..8].try_into().unwrap()) != header {
            return false;
        }
        if u64::from_ne_bytes(read_bytes[8..16].try_into().unwrap()) != 0xabcdef0123456789 {
            return false;
        }
        true
    }

    /// Verifies tracking of dropped records and their subsequent serialization when space becomes
    /// available.
    #[unsafe(no_mangle)]
    pub extern "C" fn rust_ktrace_test_dropped_record_tracking() -> bool {
        let _guard = InterruptDisableGuard::new();
        let mut storage = [0u8; 128]; // small buffer
        let mut inner_buf = unsafe { Buffer::from_raw_parts(storage.as_mut_ptr(), storage.len()) };
        let leaked_ref = unsafe { &mut *ptr::from_mut(&mut inner_buf) };
        let mut stats = DroppedRecordStats::default();
        let leaked_stats = unsafe { &mut *ptr::from_mut(&mut stats) };
        let mut kbuf = KTraceBuffer::new(leaked_ref, leaked_stats, 1, 100, 200);

        // Reserve almost all space.
        // 128 bytes total. Let's reserve 96 bytes (12 words).
        let header = 4u64 | (12u64 << 4);
        let mut res = match kbuf.reserve(header) {
            Ok(r) => r,
            Err(_) => return false,
        };
        if res.write_bytes(&[0u8; 88]).is_err() {
            return false;
        }
        if res.commit().is_err() {
            return false;
        }

        // Now, try to reserve 64 bytes (8 words). This should fail because there are only 32 bytes
        // left.
        let header2 = 4u64 | (8u64 << 4);
        if kbuf.reserve(header2).err() != Some(Status::NO_SPACE) {
            return false;
        }

        // This failed reservation should have been tracked!
        if !kbuf.drop_stats.has_dropped() {
            return false;
        }
        if kbuf.drop_stats.num_dropped != 1 {
            return false;
        }
        if kbuf.drop_stats.bytes_dropped != 64 {
            return false;
        }

        // Now, drain the buffer to free all space.
        if kbuf.drain().is_err() {
            return false;
        }

        // Now, reserve 16 bytes (2 words).
        // Since first_dropped was set, this reservation should successfully write the 56-byte
        // dropped records duration event first!
        let header3 = 4u64 | (2u64 << 4);
        let mut res3 = match kbuf.reserve(header3) {
            Ok(r) => r,
            Err(_) => return false,
        };
        if res3.write_bytes(&[0u8; 8]).is_err() {
            return false;
        }
        if res3.commit().is_err() {
            return false;
        }

        // The dropped stats should have been reset!
        if kbuf.drop_stats.has_dropped() {
            return false;
        }
        if kbuf.drop_stats.num_dropped != 0 {
            return false;
        }
        if kbuf.drop_stats.bytes_dropped != 0 {
            return false;
        }

        // Let's read the buffer content.
        let mut read_bytes = [0u8; 72];
        let read_len = match kbuf.read(
            |offset, src| {
                read_bytes[offset as usize..offset as usize + src.len()].copy_from_slice(src);
                Ok(())
            },
            72,
        ) {
            Ok(l) => l,
            Err(_) => return false,
        };

        if read_len != 72 {
            return false;
        }

        // Verify the DroppedRecordDurationEvent header:
        let event_header = u64::from_ne_bytes(read_bytes[0..8].try_into().unwrap());
        if (event_header & 0xf) != 4 {
            return false;
        }
        if ((event_header >> 4) & 0xfff) != 7 {
            return false;
        }
        if ((event_header >> 16) & 0xf) != 4 {
            return false;
        }
        if ((event_header >> 20) & 0xf) != 2 {
            return false;
        }
        if ((event_header >> 24) & 0xff) != 1 {
            return false;
        }
        if ((event_header >> 32) & 0xffff) != u64::from(META_CAT.label().id()) {
            return false;
        }
        if ((event_header >> 48) & 0xffff) != u64::from(DROP_STATS_REF.id()) {
            return false;
        }

        // Verify process_koid (100) and thread_koid (200)
        if u64::from_ne_bytes(read_bytes[16..24].try_into().unwrap()) != 100 {
            return false;
        }
        if u64::from_ne_bytes(read_bytes[24..32].try_into().unwrap()) != 200 {
            return false;
        }

        // Verify the two arguments:
        let num_dropped_arg = u64::from_ne_bytes(read_bytes[32..40].try_into().unwrap());
        if (num_dropped_arg & 0xf) != 2 {
            return false;
        }
        if ((num_dropped_arg >> 4) & 0xfff) != 1 {
            return false;
        }
        if ((num_dropped_arg >> 16) & 0xffff) != u64::from(NUM_RECORDS_REF.id()) {
            return false;
        }
        if ((num_dropped_arg >> 32) & 0xffffffff) != 1 {
            return false;
        }

        let bytes_dropped_arg = u64::from_ne_bytes(read_bytes[40..48].try_into().unwrap());
        if (bytes_dropped_arg & 0xf) != 2 {
            return false;
        }
        if ((bytes_dropped_arg >> 16) & 0xffff) != u64::from(NUM_BYTES_REF.id()) {
            return false;
        }
        if ((bytes_dropped_arg >> 32) & 0xffffffff) != 64 {
            return false;
        }

        // Verify the reservation header3:
        let res_header = u64::from_ne_bytes(read_bytes[56..64].try_into().unwrap());
        if res_header != header3 {
            return false;
        }

        true
    }

    /// Verifies direct invocation of KTraceBuffer::emit_drop_stats.
    #[unsafe(no_mangle)]
    pub extern "C" fn rust_ktrace_test_emit_drop_stats() -> bool {
        let _guard = InterruptDisableGuard::new();
        let mut storage = [0u8; 128];
        let mut inner_buf = unsafe { Buffer::from_raw_parts(storage.as_mut_ptr(), storage.len()) };
        let leaked_ref = unsafe { &mut *ptr::from_mut(&mut inner_buf) };
        let mut stats = DroppedRecordStats::default();
        let leaked_stats = unsafe { &mut *ptr::from_mut(&mut stats) };
        let mut kbuf = KTraceBuffer::new(leaked_ref, leaked_stats, 1, 100, 200);

        // 1. Force a failed reservation to track a dropped record.
        let header = 4u64 | (32u64 << 4);
        if kbuf.reserve(header).err() != Some(Status::NO_SPACE) {
            return false;
        }

        if !kbuf.drop_stats.has_dropped() {
            return false;
        }
        if kbuf.drop_stats.num_dropped != 1 {
            return false;
        }
        if kbuf.drop_stats.bytes_dropped != 256 {
            return false;
        }

        // 2. Call emit_drop_stats directly.
        if kbuf.emit_drop_stats().is_err() {
            return false;
        }

        if kbuf.drop_stats.has_dropped() {
            return false;
        }
        if kbuf.drop_stats.num_dropped != 0 {
            return false;
        }
        if kbuf.drop_stats.bytes_dropped != 0 {
            return false;
        }

        // 3. Read and verify the event.
        let mut read_bytes = [0u8; 56];
        let read_len = match kbuf.read(
            |offset, src| {
                read_bytes[offset as usize..offset as usize + src.len()].copy_from_slice(src);
                Ok(())
            },
            56,
        ) {
            Ok(l) => l,
            Err(_) => return false,
        };

        if read_len != 56 {
            return false;
        }

        let event_header = u64::from_ne_bytes(read_bytes[0..8].try_into().unwrap());
        if (event_header & 0xf) != 4 {
            return false;
        }
        if ((event_header >> 4) & 0xfff) != 7 {
            return false;
        }
        if ((event_header >> 16) & 0xf) != 4 {
            return false;
        }
        if ((event_header >> 20) & 0xf) != 2 {
            return false;
        }
        if ((event_header >> 24) & 0xff) != 1 {
            return false;
        }
        if ((event_header >> 32) & 0xffff) != u64::from(META_CAT.label().id()) {
            return false;
        }
        if ((event_header >> 48) & 0xffff) != u64::from(DROP_STATS_REF.id()) {
            return false;
        }

        if u64::from_ne_bytes(read_bytes[16..24].try_into().unwrap()) != 100 {
            return false;
        }
        if u64::from_ne_bytes(read_bytes[24..32].try_into().unwrap()) != 200 {
            return false;
        }

        let num_dropped_arg = u64::from_ne_bytes(read_bytes[32..40].try_into().unwrap());
        if (num_dropped_arg & 0xf) != 2 {
            return false;
        }
        if ((num_dropped_arg >> 16) & 0xffff) != u64::from(NUM_RECORDS_REF.id()) {
            return false;
        }
        if ((num_dropped_arg >> 32) & 0xffffffff) != 1 {
            return false;
        }

        let bytes_dropped_arg = u64::from_ne_bytes(read_bytes[40..48].try_into().unwrap());
        if (bytes_dropped_arg & 0xf) != 2 {
            return false;
        }
        if ((bytes_dropped_arg >> 16) & 0xffff) != u64::from(NUM_BYTES_REF.id()) {
            return false;
        }
        if ((bytes_dropped_arg >> 32) & 0xffffffff) != 256 {
            return false;
        }

        true
    }

    /// Verifies the full global lifecycle of KTrace: initialization, category bitmasks,
    /// CPU buffer allocation, and high-level tracing macro execution.
    #[unsafe(no_mangle)]
    pub extern "C" fn rust_ktrace_test_global_lifecycle() -> bool {
        let _guard = InterruptDisableGuard::new();
        // Initialize indices of the categories we're testing.
        META_CAT.set_index(0, InternedCategory::INVALID_INDEX);
        MEMORY_CAT.set_index(1, InternedCategory::INVALID_INDEX);
        SCHED_CAT.set_index(2, InternedCategory::INVALID_INDEX);
        CONTENTION_CAT.set_index(3, InternedCategory::INVALID_INDEX);
        IPC_CAT.set_index(4, InternedCategory::INVALID_INDEX);
        IRQ_CAT.set_index(5, InternedCategory::INVALID_INDEX);

        // 1. Initialize the global KTrace instance with the system CPU count buffers and a local
        // mock state.
        let num_cpus = max_num_cpus();
        let mut local_state = KTraceState {
            categories_bitmask: AtomicU32::new(0),
            writes_enabled: AtomicBool::new(false),
        };
        let local_state_ptr = ptr::from_mut(&mut local_state).cast::<ffi::c_void>();

        let status = unsafe { rust_ktrace_init(num_cpus, local_state_ptr) };
        if status != 0 {
            return false;
        }

        let ktrace = KTrace::get_instance();

        // 2. Verify initial states.
        if ktrace.writes_enabled() {
            return false;
        }
        if ktrace.categories_bitmask() != 0 {
            return false;
        }
        if ktrace.is_category_enabled(&META_CAT) {
            return false;
        }
        if ktrace.is_category_enabled(&IRQ_CAT) {
            return false;
        }

        // 3. Test writes_enabled.
        local_state.writes_enabled.store(true, Ordering::Release);
        if !ktrace.writes_enabled() {
            return false;
        }
        local_state.writes_enabled.store(false, Ordering::Release);
        if ktrace.writes_enabled() {
            return false;
        }

        // 4. Test categories_bitmask.
        let mask = (1 << MEMORY_CAT.index()) | (1 << CONTENTION_CAT.index());
        local_state.categories_bitmask.store(mask, Ordering::Release);
        if ktrace.categories_bitmask() != mask {
            return false;
        }
        if ktrace.is_category_enabled(&META_CAT) {
            return false;
        }
        if !ktrace.is_category_enabled(&MEMORY_CAT) {
            return false;
        }
        if ktrace.is_category_enabled(&SCHED_CAT) {
            return false;
        }
        if !ktrace.is_category_enabled(&CONTENTION_CAT) {
            return false;
        }
        if ktrace.is_category_enabled(&IPC_CAT) {
            return false;
        }

        // 5. Test CPU buffer initialization and reserve.
        let mut storage = [0u8; 256];
        let mut inner_buf = unsafe { Buffer::from_raw_parts(storage.as_mut_ptr(), storage.len()) };
        let inner_buf_ptr = ptr::from_mut(&mut inner_buf).cast::<ffi::c_void>();
        let mut stats = DroppedRecordStats::default();
        let stats_ptr = ptr::from_mut(&mut stats).cast::<ffi::c_void>();

        // Initialize current CPU buffer.
        let cpu = curr_cpu_num();
        let init_status = unsafe {
            rust_ktrace_init_cpu_buffer(
                cpu, // cpu_num
                inner_buf_ptr,
                stats_ptr,
                100, // process_koid
                200, // thread_koid
                1,   // cpu_ref_header_entry
            )
        };
        if init_status != 0 {
            return false;
        }

        // If writes are disabled, reserving should fail.
        let header = 4u64 | (2u64 << 4); // Event with 2 words
        if unsafe { ktrace.reserve(header) }.err() != Some(Status::BAD_STATE) {
            return false;
        }

        // Enable writes.
        local_state.writes_enabled.store(true, Ordering::Release);

        // Now reserve should succeed!
        let mut res = match unsafe { ktrace.reserve(header) } {
            Ok(r) => r,
            Err(_) => return false,
        };
        if res.write_word(0x1234567890abcdef).is_err() {
            return false;
        }
        if res.commit().is_err() {
            return false;
        }

        // Read and verify the written record from the current CPU buffer.
        let ptr = ktrace.buffers[cpu as usize].load(Ordering::Acquire);
        if ptr.is_null() {
            return false;
        }
        let buf = unsafe { &*ptr };

        let mut read_bytes = [0u8; 16];
        let read_len = match buf.read(
            |offset, src| {
                read_bytes[offset as usize..offset as usize + src.len()].copy_from_slice(src);
                Ok(())
            },
            16,
        ) {
            Ok(l) => l,
            Err(_) => return false,
        };

        if read_len != 16 {
            return false;
        }
        if u64::from_ne_bytes(read_bytes[0..8].try_into().unwrap()) != header {
            return false;
        }
        if u64::from_ne_bytes(read_bytes[8..16].try_into().unwrap()) != 0x1234567890abcdef {
            return false;
        }

        true
    }
}
