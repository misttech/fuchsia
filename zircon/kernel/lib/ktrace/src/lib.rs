// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

use arch_rs::{InterruptDisableGuard, curr_cpu_num, ints_disabled};
use core::cell::UnsafeCell;
use core::mem::{MaybeUninit, size_of};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use core::{ffi, ptr, slice};
use kalloc::Box;
use kernel::thread::{FxtRef, ThreadPtr};
pub use kstring::declare_interned_category;
use kstring::declare_interned_string;
use kstring::interned_category::InternedCategory;
pub use kstring::interned_string::InternedString;
pub use platform_rs::{InstantBootTicks, timer_current_boot_ticks};
use spsc_buffer::{Buffer, NoOpAllocator, Reservation};
use zx_status::Status;

// Re-export the ktrace macros from the sub-crate.
pub use ktrace_macro::*;
#[allow(unused_extern_crates)]
extern crate self as ktrace_rs;

// LINT.IfChange(KTraceState)
#[repr(C)]
pub struct KTraceState {
    pub categories_bitmask: AtomicU32,
    pub writes_enabled: AtomicBool,
}
const _: () = assert!(size_of::<KTraceState>() == 8);
// LINT.ThenChange(//zircon/kernel/include/lib/ktrace.h:KTraceState)

declare_interned_category!(META_CAT, "kernel:meta", extern);
declare_interned_string!(DROP_STATS_REF, "drop_stats", extern);
declare_interned_string!(NUM_RECORDS_REF, "num_records", extern);
declare_interned_string!(NUM_BYTES_REF, "num_bytes", extern);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    Thread = 0,
    Cpu = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    Instant = 0,
    Counter = 1,
    DurationBegin = 2,
    DurationEnd = 3,
    DurationComplete = 4,
    FlowBegin = 8,
    FlowStep = 9,
    FlowEnd = 10,
}

/// The value of a trace argument.
pub enum ArgValue<'a> {
    Null,
    Bool(bool),
    Int32(i32),
    Uint32(u32),
    Int64(i64),
    Uint64(u64),
    Double(f64),
    String(&'a str),
    Pointer(usize),
    Koid(u64),
}

impl<'a> From<bool> for ArgValue<'a> {
    fn from(v: bool) -> Self {
        ArgValue::Bool(v)
    }
}
impl<'a> From<i32> for ArgValue<'a> {
    fn from(v: i32) -> Self {
        ArgValue::Int32(v)
    }
}
impl<'a> From<u32> for ArgValue<'a> {
    fn from(v: u32) -> Self {
        ArgValue::Uint32(v)
    }
}
impl<'a> From<i64> for ArgValue<'a> {
    fn from(v: i64) -> Self {
        ArgValue::Int64(v)
    }
}
impl<'a> From<u64> for ArgValue<'a> {
    fn from(v: u64) -> Self {
        ArgValue::Uint64(v)
    }
}
impl<'a> From<f64> for ArgValue<'a> {
    fn from(v: f64) -> Self {
        ArgValue::Double(v)
    }
}
impl<'a> From<&'a str> for ArgValue<'a> {
    fn from(v: &'a str) -> Self {
        ArgValue::String(v)
    }
}

pub struct Argument<'a> {
    name: &'static InternedString,
    value: ArgValue<'a>,
}

impl<'a> Argument<'a> {
    pub fn new(name: &'static InternedString, value: impl Into<ArgValue<'a>>) -> Self {
        Self { name, value: value.into() }
    }

    /// Returns the size of this argument in 64-bit words.
    fn size_words(&self) -> usize {
        match &self.value {
            ArgValue::Null | ArgValue::Bool(_) | ArgValue::Int32(_) | ArgValue::Uint32(_) => 1,
            ArgValue::Int64(_)
            | ArgValue::Uint64(_)
            | ArgValue::Double(_)
            | ArgValue::Pointer(_)
            | ArgValue::Koid(_) => 2,
            ArgValue::String(s) => 1 + (s.len() + 7) / 8,
        }
    }

    fn write(&self, res: &mut KTraceReservation<'_>) -> Result<(), Status> {
        let name_id = self.name.id() as u64;
        let mut header = 0u64;
        header |= (name_id & 0xffff) << 16; // NameRef

        match &self.value {
            ArgValue::Null => {
                header |= 0u64; // ArgumentType::kNull (0)
                res.write_word(header)?;
            }
            ArgValue::Int32(v) => {
                header |= 1u64; // ArgumentType::kInt32 (1)
                header |= ((*v as u32) as u64) << 32;
                res.write_word(header)?;
            }
            ArgValue::Uint32(v) => {
                header |= 2u64; // ArgumentType::kUint32 (2)
                header |= (*v as u64) << 32;
                res.write_word(header)?;
            }
            ArgValue::Int64(v) => {
                header |= 3u64; // ArgumentType::kInt64 (3)
                res.write_word(header)?;
                res.write_word(*v as u64)?;
            }
            ArgValue::Uint64(v) => {
                header |= 4u64; // ArgumentType::kUint64 (4)
                res.write_word(header)?;
                res.write_word(*v)?;
            }
            ArgValue::Double(v) => {
                header |= 5u64; // ArgumentType::kDouble (5)
                res.write_word(header)?;
                res.write_word(v.to_bits())?;
            }
            ArgValue::String(s) => {
                header |= 6u64; // ArgumentType::kString (6)
                let string_len = s.len();
                header |= (((string_len + 7) / 8) as u64) << 4; // ArgumentSize in words
                header |= (string_len as u64) << 32; // String length in bytes
                res.write_word(header)?;
                res.write_bytes(s.as_bytes())?;
            }
            ArgValue::Pointer(v) => {
                header |= 7u64; // ArgumentType::kPointer (7)
                res.write_word(header)?;
                res.write_word(*v as u64)?;
            }
            ArgValue::Koid(v) => {
                header |= 8u64; // ArgumentType::kKoid (8)
                res.write_word(header)?;
                res.write_word(*v)?;
            }
            ArgValue::Bool(v) => {
                header |= 9u64; // ArgumentType::kBool (9)
                header |= ((*v as u64) & 1) << 32;
                res.write_word(header)?;
            }
        }
        Ok(())
    }
}

pub struct KTraceScope<'a> {
    category: &'static InternedCategory,
    name: &'static InternedString,
    timestamp: InstantBootTicks,
    context: Context,
    args: &'a [Argument<'a>],
}

impl<'a> KTraceScope<'a> {
    #[inline(never)]
    #[cold]
    pub fn begin(
        category: &'static InternedCategory,
        name: &'static InternedString,
        context: Context,
        args: &'a [Argument<'a>],
    ) -> Self {
        let timestamp = timer_current_boot_ticks();
        Self { category, name, timestamp, context, args }
    }
}

impl<'a> Drop for KTraceScope<'a> {
    #[inline(never)]
    #[cold]
    fn drop(&mut self) {
        let end_time = timer_current_boot_ticks();
        let ktrace = KTrace::get_instance();
        ktrace.emit_event(
            EventType::DurationComplete,
            self.category,
            self.name,
            self.timestamp,
            self.context,
            Some(end_time.0 as u64),
            self.args,
        );
    }
}

// LINT.IfChange(DroppedRecordDurationEvent)
#[repr(C)]
struct DroppedRecordDurationEvent {
    header: u64,
    start: InstantBootTicks,
    process_id: u64,
    thread_id: u64,
    num_dropped_arg: u64,
    bytes_dropped_arg: u64,
    end: InstantBootTicks,
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
pub struct DroppedRecordStats {
    pub first_dropped: InstantBootTicks,
    pub last_dropped: InstantBootTicks,

    /// By storing num_dropped and bytes_dropped in 32-bit values, we ensure that they can each
    /// be stored in a single 64-bit word in the FXT record we emit when space is available.
    pub num_dropped: u32,
    pub bytes_dropped: u32,
    pub has_dropped: bool,
}
const _: () = assert!(size_of::<DroppedRecordStats>() == 32);
// LINT.ThenChange(//zircon/kernel/lib/percpu_writer/include/lib/percpu_writer/buffer.h:DroppedRecordStats)

impl DroppedRecordStats {
    pub fn reset(&mut self) {
        self.first_dropped = InstantBootTicks(0);
        self.last_dropped = InstantBootTicks(0);
        self.num_dropped = 0;
        self.bytes_dropped = 0;
        self.has_dropped = false;
    }

    pub fn track(&mut self, now: InstantBootTicks, size: u32) {
        if !self.has_dropped {
            self.first_dropped = now;
            self.has_dropped = true;
        }
        self.last_dropped = now;
        self.num_dropped = self.num_dropped.wrapping_add(1);
        self.bytes_dropped = self.bytes_dropped.wrapping_add(size);
    }

    pub fn has_dropped(&self) -> bool {
        self.has_dropped
    }
}

/// A Rust implementation of `percpu_writer::Buffer` that wraps a static reference to an existing
/// `spsc_buffer::Buffer` and tracks dropped trace records.
pub struct KTraceBuffer {
    buffer: &'static mut Buffer<NoOpAllocator>,
    pub drop_stats: &'static mut DroppedRecordStats,
    size: u32,
    cpu_ref_header_entry: u16,
    pub process_koid: u64,
    pub thread_koid: u64,
}

// SAFETY: Access to the per-CPU buffers is synchronized by the caller (typically via disabling
// interrupts).
unsafe impl Send for KTraceBuffer {}
unsafe impl Sync for KTraceBuffer {}

impl KTraceBuffer {
    /// Constructs a `KTraceBuffer` from a static reference to an existing `spsc_buffer::Buffer`,
    /// a static reference to `DroppedRecordStats`, and its metadata.
    pub fn new(
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

    /// Returns the raw pointer to the KTraceBuffer for the given CPU.
    pub fn get_buffer(&self, cpu: usize) -> *mut KTraceBuffer {
        self.buffers[cpu].load(Ordering::Acquire)
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

    /// Low-level helper to write an FXT kernel object record.
    ///
    /// This is not inlined to reduce code size at the instrumentation sites.
    #[inline(never)]
    #[cold]
    pub fn emit_kernel_object_outlined(
        &self,
        koid: u64,
        obj_type: u32,
        name: &InternedString,
        args: &[Argument<'_>],
    ) {
        let _guard = InterruptDisableGuard::new();
        if !self.writes_enabled() {
            return;
        }

        let base_size = 2; // Header, KOID
        let args_size: usize = args.iter().map(|a| a.size_words()).sum();
        let total_size_words = base_size + args_size;

        if total_size_words > 0xfff {
            return;
        }

        let mut header = 7u64; // RecordType::kKernelObject (7)
        header |= (total_size_words as u64) << 4; // RecordSize
        header |= (obj_type as u64) << 16; // ObjectType
        header |= (name.id() as u64) << 24; // NameStringRef
        header |= (args.len() as u64) << 40; // ArgumentCount

        if let Ok(mut res) = unsafe { self.reserve(header) } {
            let _ = res.write_word(koid);
            for arg in args {
                let _ = arg.write(&mut res);
            }
            let _ = res.commit();
        }
    }

    /// Low-level helper to write a generic FXT event record.
    ///
    /// This is not inlined to reduce code size at the instrumentation sites.
    #[inline(never)]
    #[cold]
    pub fn emit_event(
        &self,
        event_type: EventType,
        category: &InternedCategory,
        name: &InternedString,
        timestamp: InstantBootTicks,
        context: Context,
        content: Option<u64>,
        args: &[Argument<'_>],
    ) {
        let _guard = InterruptDisableGuard::new();
        if !self.writes_enabled() {
            return;
        }

        // 1. Get the process/thread KOIDs for the context.
        let (process_koid, thread_koid) = match context {
            Context::Thread => {
                // SAFETY: ktrace is initialized and running after threading has been initialized.
                let FxtRef { pid, tid } = unsafe { ThreadPtr::current().fxt_ref() };
                (pid, tid)
            }
            Context::Cpu => {
                let cpu = curr_cpu_num() as usize;
                let ptr = self.buffers[cpu].load(Ordering::Acquire);
                if ptr.is_null() {
                    return;
                }
                let buf = unsafe { &*ptr };
                (buf.process_koid, buf.thread_koid)
            }
        };

        // 2. Calculate the record size.
        let base_size = 4; // Header, Timestamp, Process KOID, Thread KOID
        let content_size = if content.is_some() { 1 } else { 0 };
        let args_size: usize = args.iter().map(|a| a.size_words()).sum();
        let total_size_words = base_size + content_size + args_size;

        if total_size_words > 0xfff {
            return;
        }

        // 3. Construct the header.
        let mut header = 4u64; // RecordType::kEvent (4)
        header |= (total_size_words as u64) << 4; // RecordSize
        header |= (event_type as u32 as u64) << 16; // EventType
        header |= (args.len() as u64) << 20; // ArgumentCount
        header |= (category.label().id() as u64) << 32; // CategoryStringRef
        header |= (name.id() as u64) << 48; // NameStringRef

        // 4. Reserve space and write the record.
        if let Ok(mut res) = unsafe { self.reserve(header) } {
            let _ = res.write_word(timestamp.0 as u64);
            let _ = res.write_word(process_koid);
            let _ = res.write_word(thread_koid);

            for arg in args {
                let _ = arg.write(&mut res);
            }

            if let Some(c) = content {
                let _ = res.write_word(c);
            }

            let _ = res.commit();
        }
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
