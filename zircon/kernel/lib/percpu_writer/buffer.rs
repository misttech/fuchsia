// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Per-CPU SPSC ring buffer wrapper with dropped-record tracking.

use core::cell::UnsafeCell;
use core::mem::size_of;
use core::ptr::NonNull;

pub use percpu_writer_bindings::{DroppedRecordDurationEvent, DroppedRecordStats};
use spsc_buffer::{Buffer as SpscBuffer, NoOpAllocator, Reservation as SpscReservation};
use zerocopy::IntoBytes;
use zx_status::Status;

use super::KernelAspaceAllocator;
use crate::arch_rs::InterruptDisableGuard;
use crate::ktrace_rs::{
    ArgumentHeader, ArgumentType, EventRecordHeader, EventType, LargeRecordHeader, RecordHeader,
    RecordType, declare_interned_category, declare_interned_string,
};
use crate::platform_rs::timer::timer_current_boot_ticks;

declare_interned_category!(META_CAT, "kernel:meta", extern);
declare_interned_string!(DROP_STATS_REF, "drop_stats", extern);
declare_interned_string!(NUM_RECORDS_REF, "num_records", extern);
declare_interned_string!(NUM_BYTES_REF, "num_bytes", extern);

zr::static_assert!(size_of::<DroppedRecordStats>() == 32);
zr::static_assert!(size_of::<DroppedRecordDurationEvent>() == 56);

/// Serializes `stats` into a [`DroppedRecordDurationEvent`].
#[inline]
pub fn serialize_drop_stats(
    stats: &DroppedRecordStats,
    thread_ref: u8,
    process_koid: u64,
    thread_koid: u64,
) -> DroppedRecordDurationEvent {
    // This method should only be called if we are currently tracking a run of dropped records.
    debug_assert!(stats.has_dropped());
    debug_assert!(stats.last_dropped != 0);

    let record_size_words = (size_of::<DroppedRecordDurationEvent>() / size_of::<u64>()) as u16;
    let mut header = EventRecordHeader::default();
    header
        .set_record_size(record_size_words)
        .set_event_type(EventType::DurationComplete)
        .set_arg_count(2)
        .set_thread_ref(thread_ref)
        .set_category_ref(META_CAT.label().id())
        .set_name_ref(DROP_STATS_REF.id());

    let mut num_dropped_arg =
        ArgumentHeader::for_argument(NUM_RECORDS_REF.id(), 1, ArgumentType::Uint32);
    num_dropped_arg.set_value_bits(stats.num_dropped);

    let mut bytes_dropped_arg =
        ArgumentHeader::for_argument(NUM_BYTES_REF.id(), 1, ArgumentType::Uint32);
    bytes_dropped_arg.set_value_bits(stats.bytes_dropped);

    DroppedRecordDurationEvent {
        header: header.bits(),
        start: stats.first_dropped,
        process_id: process_koid,
        thread_id: thread_koid,
        num_dropped_arg: num_dropped_arg.bits(),
        bytes_dropped_arg: bytes_dropped_arg.bits(),
        end: stats.last_dropped,
    }
}

/// `Reservation` encapsulates a pending write to the buffer.
///
/// Holding a shared reference to [`InterruptDisableGuard`] for the lifetime `'a` statically
/// ensures that interrupts remain disabled on the current CPU for the entire lifetime of this
/// reservation, preserving the single-writer invariant of each per-CPU buffer.
pub struct Reservation<'a> {
    reservation: SpscReservation<'a>,
    _irq_guard: &'a InterruptDisableGuard,
}

impl<'a> Reservation<'a> {
    #[inline]
    fn new(
        reservation: SpscReservation<'a>,
        irq_guard: &'a InterruptDisableGuard,
        header: u64,
    ) -> Self {
        let mut res = Self { reservation, _irq_guard: irq_guard };
        res.write_word(header);
        res
    }

    /// Writes a 64-bit word into the reservation.
    #[inline]
    pub fn write_word(&mut self, word: u64) {
        let status = self.reservation.write(&word.to_ne_bytes());
        debug_assert!(status.is_ok());
    }

    /// Writes `bytes` into the reservation, padding with trailing zeroes to an 8-byte boundary.
    #[inline]
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        // Write the data provided.
        let status = self.reservation.write(bytes);
        debug_assert!(status.is_ok());

        // Write any padding bytes necessary.
        let aligned_bytes = (bytes.len() + 7) & !7;
        let num_zeros_to_write = aligned_bytes - bytes.len();
        if num_zeros_to_write != 0 {
            const ZERO: [u8; 8] = [0; 8];
            let pad_status = self.reservation.write(&ZERO[..num_zeros_to_write]);
            debug_assert!(pad_status.is_ok());
        }
    }

    /// Commits the reservation, making the written record visible to readers.
    #[inline]
    pub fn commit(self) {
        let status = self.reservation.commit();
        debug_assert!(status.is_ok());
    }
}

/// `percpu_writer::Buffer` wraps an [`SpscBuffer`] and adds functionality to track dropped trace
/// records.
#[repr(C, align(64))]
pub struct Buffer {
    // Safety invariant: `buffer`'s backing storage is a live kernel address space region allocated
    // by `KernelAspaceAllocator::allocate` in `new` and freed in `Drop`.
    buffer: SpscBuffer<NoOpAllocator>,
    /// Track stats of trace records dropped when the buffer is full.
    ///
    /// Mutated exclusively by the owning CPU's producer path (`reserve` and `emit_drop_stats`)
    /// with interrupts disabled.
    drop_stats: UnsafeCell<DroppedRecordStats>,
    process_koid: u64,
    thread_koid: u64,
}

// SAFETY: `Buffer` owns its kernel address space backing storage (`self.buffer`) and can be safely
// sent across threads.
unsafe impl Send for Buffer {}
// SAFETY: Concurrent access to `Buffer` is split between a single per-CPU producer (serialized via
// `&InterruptDisableGuard` on the owning CPU, which mutates `drop_stats` through `UnsafeCell` and
// writes disjoint ring-buffer slots via `SpscBuffer::reserve_shared`) and a single consumer
// (`read` / `drain`), synchronized via `SpscBuffer`'s atomic read/write pointers.
unsafe impl Sync for Buffer {}

impl Drop for Buffer {
    fn drop(&mut self) {
        // Extract the raw storage pointer and reset the `SpscBuffer<NoOpAllocator>` view first,
        // then free the underlying kernel address space mapping.
        let (storage_ptr, _) = self.buffer.take_raw_parts();
        // SAFETY: `storage_ptr` was allocated by `KernelAspaceAllocator::allocate` in `new`
        // and is no longer in use because `self` is being dropped with exclusive `&mut self`.
        unsafe {
            KernelAspaceAllocator::free(NonNull::new(storage_ptr));
        }
    }
}

impl Buffer {
    /// Maximum size of backing storage buffer (2 GiB), matching `spsc_buffer::Buffer`.
    const MAX_STORAGE_SIZE: u32 = 1 << 31;

    /// Allocates and initializes a new `Buffer` with backing storage of `size` bytes.
    pub fn new(
        size: u32,
        buffer_name: &[u8],
        process_koid: u64,
        thread_koid: u64,
    ) -> Result<Self, Status> {
        if size > Self::MAX_STORAGE_SIZE || !size.is_power_of_two() {
            return Err(Status::INVALID_ARGS);
        }

        let storage =
            KernelAspaceAllocator::allocate(size, buffer_name).ok_or(Status::NO_MEMORY)?;
        // SAFETY: `storage` is non-null and points to a valid, zero-initialized kernel mapping of
        // at least `size` bytes, where `size > 0`, `size <= MAX_STORAGE_SIZE`, and
        // `size.is_power_of_two()`, and `Self` exclusively owns the backing mapping for the
        // lifetime of `buffer`.
        let buffer = unsafe { SpscBuffer::from_raw_parts(storage.as_ptr(), size as usize) };
        Ok(Self {
            buffer,
            drop_stats: UnsafeCell::new(DroppedRecordStats::new()),
            process_koid,
            thread_koid,
        })
    }

    /// Returns the configured size of the buffer in bytes.
    #[inline]
    pub const fn size(&self) -> u32 {
        self.buffer.size()
    }

    /// Drains the underlying `SpscBuffer`.
    pub fn drain(&self) -> Result<(), Status> {
        self.buffer.drain()
    }

    /// Reads from the underlying `SpscBuffer`.
    pub fn read<F>(&self, copy_fn: F, len: u32) -> Result<u32, Status>
    where
        F: FnMut(u32, &[u8]) -> Result<(), Status>,
    {
        self.buffer.read(copy_fn, len)
    }

    /// We interpose ourselves in the `reserve` path to ensure that we can emit a record containing
    /// the dropped records statistics if we need to.
    ///
    /// # Safety
    ///
    /// - Producer calls ([`Self::reserve`] and [`Self::emit_drop_stats`]) on `self` must only be
    ///   invoked from the single CPU that owns this per-CPU buffer.
    /// - No uncommitted [`Reservation`] for `self` may be live when this method is called.
    pub unsafe fn reserve<'a>(
        &'a self,
        irq_guard: &'a InterruptDisableGuard,
        header: u64,
    ) -> Result<Reservation<'a>, Status> {
        // Compute the number of bytes we need to reserve from the provided fxt header.
        let rec_hdr = RecordHeader::from(header);
        let num_words = if matches!(rec_hdr.try_record_type(), Ok(RecordType::LargeBlob)) {
            LargeRecordHeader::from(header).record_size()
        } else {
            u32::from(rec_hdr.record_size())
        };
        if num_words == 0 {
            return Err(Status::INVALID_ARGS);
        }
        let size = num_words.checked_mul(size_of::<u64>() as u32).ok_or(Status::INVALID_ARGS)?;

        // SAFETY: The caller guarantees this is the owning producer CPU with no live
        // `Reservation`, `irq_guard` disables interrupts on this CPU, and readers never access
        // `drop_stats`.
        let drop_stats = unsafe { &mut *self.drop_stats.get() };

        // If `first_dropped` is set to a value, then we are currently tracking a run of dropped
        // trace records, so we need to emit a duration record containing that information. We
        // could emit this record independently (with its own call to `SpscBuffer::reserve`), but
        // this could lead to situations in which we thrash and emit multiple
        // `DroppedRecordDurationEvent`s in a row. To avoid this, we attempt to reserve the
        // desired size plus the size needed to store the `DroppedRecordDurationEvent` record, and
        // then write the statistics into the first part of the reservation before returning it.
        let has_dropped = drop_stats.has_dropped();
        let mut total_size = size;
        if has_dropped {
            total_size = total_size.saturating_add(size_of::<DroppedRecordDurationEvent>() as u32);
        }

        // SAFETY: The caller guarantees this is the owning producer CPU with no live
        // `Reservation`, and `irq_guard` disables interrupts on this CPU.
        let mut res = match unsafe { self.buffer.reserve_shared(total_size) } {
            Ok(res) => res,
            Err(status) => {
                // If the reservation failed, then we did not have enough space in this buffer,
                // and the record we were attempting to write will be dropped. Add the `size` to
                // the dropped record statistics. Notably, we do not add the `total_size`, because
                // that may include the size of the `DroppedRecordDurationEvent`.
                drop_stats.track(timer_current_boot_ticks().0, size);
                return Err(status);
            }
        };

        // If we need to write a dropped record duration event, do that here.
        debug_assert!(has_dropped || total_size == size);
        if has_dropped {
            let record = serialize_drop_stats(drop_stats, 0, self.process_koid, self.thread_koid);
            let write_status = res.write(record.as_bytes());
            debug_assert!(write_status.is_ok());
            // We've successfully written out the dropped record stats, so reset them for the next
            // run.
            drop_stats.reset();
        }

        Ok(Reservation::new(res, irq_guard, header))
    }

    /// Emit the dropped record stats to the trace buffer.
    /// If we're not tracking a run of dropped records, this is a no-op.
    ///
    /// # Safety
    ///
    /// - Producer calls ([`Self::reserve`] and [`Self::emit_drop_stats`]) on `self` must only be
    ///   invoked from the single CPU that owns this per-CPU buffer.
    /// - No uncommitted [`Reservation`] for `self` may be live when this method is called.
    pub unsafe fn emit_drop_stats(&self, _irq_guard: &InterruptDisableGuard) -> Result<(), Status> {
        // SAFETY: The caller guarantees this is the owning producer CPU with no live
        // `Reservation`, `_irq_guard` disables interrupts on this CPU, and readers never access
        // `drop_stats`.
        let drop_stats = unsafe { &mut *self.drop_stats.get() };
        if !drop_stats.has_dropped() {
            debug_assert_eq!(drop_stats.last_dropped, 0);
            return Ok(());
        }

        // Try to reserve a slot for the duration record. This will fail if there still isn't
        // enough space in buffer to store the statistics.
        // SAFETY: The caller guarantees this is the owning producer CPU with no live
        // `Reservation`, and `_irq_guard` disables interrupts on this CPU.
        let mut res =
            unsafe { self.buffer.reserve_shared(size_of::<DroppedRecordDurationEvent>() as u32) }?;
        let record = serialize_drop_stats(drop_stats, 0, self.process_koid, self.thread_koid);
        let write_status = res.write(record.as_bytes());
        debug_assert!(write_status.is_ok());
        let commit_status = res.commit();
        debug_assert!(commit_status.is_ok());

        // We've successfully emitted a record containing statistics on the last run of dropped
        // records. To prepare for the next run, we must reset the stats.
        drop_stats.reset();
        Ok(())
    }
}

/// Unit tests for `percpu_writer::Buffer`.
#[cfg(ktest)]
#[unittest::suite(name = "percpu_writer_rust")]
mod tests {
    use core::mem::size_of;

    use fxt_layout::{MAGIC_VALUE, MetadataType, StringRecordHeader};
    use unittest::{assert_eq, assert_err, assert_ok, assert_true, expect_ge, expect_le};
    use zerocopy::FromBytes;
    use zx_status::Status;

    use super::{Buffer, DroppedRecordDurationEvent, timer_current_boot_ticks};
    use crate::arch_rs::InterruptDisableGuard;
    use crate::ktrace_rs::{RecordHeader, RecordType};

    fn write_magic_number_record(
        buffer: &Buffer,
        irqd: &InterruptDisableGuard,
    ) -> Result<(), Status> {
        let mut hdr = RecordHeader::new();
        hdr.set_record_type(RecordType::Metadata).set_record_size(1);
        let header =
            hdr.bits() | ((MetadataType::TraceInfo as u64) << 16) | (u64::from(MAGIC_VALUE) << 24);
        // SAFETY: Test owns `buffer` exclusively on the current CPU with `irqd` held and no
        // uncommitted `Reservation` live.
        let res = unsafe { buffer.reserve(irqd, header) }?;
        res.commit();
        Ok(())
    }

    fn write_string_record(
        buffer: &Buffer,
        irqd: &InterruptDisableGuard,
        index: u16,
        string: &[u8],
    ) -> Result<(), Status> {
        let word_size = 1 + string.len().div_ceil(8) as u16;
        let mut hdr = StringRecordHeader::default();
        hdr.set_record_size(word_size).set_string_index(index).set_string_len(string.len() as u16);
        // SAFETY: Test owns `buffer` exclusively on the current CPU with `irqd` held and no
        // uncommitted `Reservation` live.
        let mut res = unsafe { buffer.reserve(irqd, hdr.bits()) }?;
        res.write_bytes(string);
        res.commit();
        Ok(())
    }

    /// Verifies dropped record tracking and `DroppedRecordDurationEvent` emission on `Buffer`.
    #[test]
    fn test_drop_stats() {
        let buffer = Buffer::new(128, b"test-buffer", 0xAA, 0xBB).unwrap();

        // Fill up the buffer.
        {
            let irqd = InterruptDisableGuard::new();
            for _ in 0..(128 / 8) {
                assert_ok!(write_magic_number_record(&buffer, &irqd));
            }
        }

        const STRING_RECORD_SIZE: usize = 16;
        // Reserving anything should fail.
        let drop_start_time = timer_current_boot_ticks().0;
        {
            let irqd = InterruptDisableGuard::new();
            assert_err!(write_string_record(&buffer, &irqd, 0, b"abcdefgh"), Status::NO_SPACE);
            assert_err!(write_string_record(&buffer, &irqd, 0, b"abcdefgh"), Status::NO_SPACE);
            assert_err!(write_string_record(&buffer, &irqd, 0, b"abcdefgh"), Status::NO_SPACE);
        }
        let drop_end_time = timer_current_boot_ticks().0;

        // Clear out the buffer so we can write the dropped records record.
        assert_ok!(buffer.drain());
        {
            let irqd = InterruptDisableGuard::new();
            assert_ok!(write_string_record(&buffer, &irqd, 0, b"abcdefgh"));
        }

        let mut buf = [0u8; 128];
        let bytes_read = buffer.read(
            |offset, src| {
                let start = offset as usize;
                buf[start..start + src.len()].copy_from_slice(src);
                Ok(())
            },
            128,
        );
        assert_true!(bytes_read.is_ok());
        // We should only get the drop stats plus the record that triggered it.
        let event_size = size_of::<DroppedRecordDurationEvent>();
        assert_eq!(STRING_RECORD_SIZE + event_size, bytes_read.unwrap() as usize);

        let record = DroppedRecordDurationEvent::read_from_bytes(&buf[..event_size]).unwrap();
        expect_ge!(record.start, drop_start_time);
        expect_le!(record.end, drop_end_time);

        let num_dropped_value = record.num_dropped_arg >> 32;
        let bytes_dropped_value = record.bytes_dropped_arg >> 32;
        assert_eq!(num_dropped_value, 3u64);
        assert_eq!(bytes_dropped_value, (3 * STRING_RECORD_SIZE) as u64);

        assert_eq!(record.process_id, 0xAAu64);
        assert_eq!(record.thread_id, 0xBBu64);

        let record_data: [u8; 16] =
            [0x22, 0, 0, 0, 8, 0, 0, 0, b'a', b'b', b'c', b'd', b'e', b'f', b'g', b'h'];
        // After, we should get our record.
        assert_true!(buf[event_size..event_size + 16] == record_data);
    }
}
