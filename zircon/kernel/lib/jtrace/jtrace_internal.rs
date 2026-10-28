// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Internal ring-buffer state machine and hooks trait for Jump Trace (`jtrace`).

use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};

use affine::Round;
use kprint::kformat;
use libarch::r#yield as arch_yield;
use zx_types::{
    zx_duration_boot_t, zx_duration_t, zx_instant_boot_ticks_t, zx_koid_t, zx_ticks_t, zx_time_t,
};

use crate::arch_rs::ops::clean_cache_range;
use crate::arch_rs::{curr_cpu_num, ints_disabled, max_num_cpus};
use crate::kernel::thread::{ThreadPtr, preempt_disable, preempt_reenable};
use crate::platform_rs::timer::{
    timer_current_boot_ticks, timer_current_mono_ticks, timer_get_ticks_to_time_ratio, zx_sec,
};

use super::{
    IsPersistent, LargeEntry, SmallEntry, TraceBufferType, TraceEntry, UseLargeEntries,
    UseMonoTimestamps,
};

/// Sentinel TID recorded when a trace entry is logged before early thread initialization.
pub(super) const UNKNOWN_TID: zx_koid_t = zx_koid_t::MAX;

/// A small utility function, used only locally.
// TODO(johngro): Add this to ktl?
#[inline]
const fn saturate(val: usize) -> u32 {
    if val > (u32::MAX as usize) { u32::MAX } else { val as u32 }
}

/// The definition of an interface used to abstract printing operations used when
/// dumping trace buffers. This implementation is replaced during testing in
/// order to verify that trace dumps are working as expected.
pub(super) trait TraceHooks {
    fn print_warning(&self, msg: &[u8]);
    fn print_info(&self, msg: &[u8]);
    /// Hexdumps `data` for a corrupted trace buffer.
    fn hexdump(&self, data: &[u8]);
    fn per_cpu_dump_started(&self) {}
    fn print_large_trace_entry(
        &self,
        e: &LargeEntry,
        buf_type: TraceBufferType,
        ts: zx_time_t,
        delta: zx_duration_t,
    );
    fn print_small_trace_entry(
        &self,
        e: &SmallEntry,
        buf_type: TraceBufferType,
        ts: zx_time_t,
        delta: zx_duration_t,
    );
}

/// 8-byte aligned byte buffer of length `N`.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub(super) struct AlignedBuffer<const N: usize>(pub [u8; N]);

impl<const N: usize> Default for AlignedBuffer<N> {
    fn default() -> Self {
        Self([0u8; N])
    }
}

impl<const N: usize> AsRef<[u8]> for AlignedBuffer<N> {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl<const N: usize> AsMut<[u8]> for AlignedBuffer<N> {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }
}

/// Fixed-size array wrapper for per-CPU last-entry storage.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub(super) struct LastCpuEntries<E: TraceEntry, const N: usize>(pub [E; N]);

impl<E: TraceEntry, const N: usize> AsRef<[E]> for LastCpuEntries<E, N> {
    #[inline]
    fn as_ref(&self) -> &[E] {
        &self.0
    }
}

impl<E: TraceEntry, const N: usize> AsMut<[E]> for LastCpuEntries<E, N> {
    #[inline]
    fn as_mut(&mut self) -> &mut [E] {
        &mut self.0
    }
}

/// A traits interface which holds the configuration of an instance of the debug trace subsystem.
pub(super) trait JTraceConfig: Sized + 'static {
    /// Target buffer size in bytes.
    const TARGET_BUFFER_SIZE: usize;
    /// Number of per-CPU last-entry slots in the header.
    const LAST_ENTRY_STORAGE: usize;
    /// Recovery buffer size in bytes (`TARGET_BUFFER_SIZE` when persistent, `0` otherwise).
    const RECOVERY_BUFFER_SIZE: usize;
    /// Whether the trace buffer resides in persistent RAM.
    const IS_PERSISTENT: IsPersistent;
    /// Whether large entries are used.
    const USE_LARGE_ENTRIES: UseLargeEntries;
    /// Whether monotonic timestamps are used.
    const USE_MONO_TIMESTAMPS: UseMonoTimestamps;

    /// Trace entry record type (`SmallEntry` or `LargeEntry`).
    type Entry: TraceEntry;
    /// Per-CPU last-entry array type (`LastCpuEntries<Self::Entry, LAST_ENTRY_STORAGE>`).
    type LastCpuEntries: Copy + AsRef<[Self::Entry]> + AsMut<[Self::Entry]>;
    /// Recovery buffer storage type (`AlignedBuffer<RECOVERY_BUFFER_SIZE>`).
    type RecoveryBuffer: Default + Copy + AsRef<[u8]> + AsMut<[u8]>;
    /// Backing storage buffer type (`AlignedBuffer<TARGET_BUFFER_SIZE>`).
    type StorageBuffer: Default + Copy + AsRef<[u8]> + AsMut<[u8]>;
}

/// Concrete configuration struct implementing [`JTraceConfig`].
pub(super) struct Config<
    E: TraceEntry,
    const TARGET_BUFFER_SIZE: usize,
    const LAST_ENTRY_STORAGE: usize,
    const RECOVERY_BUFFER_SIZE: usize,
    const IS_PERSISTENT: i32,
    const USE_LARGE_ENTRIES: i32,
    const USE_MONO_TIMESTAMPS: i32,
>(PhantomData<E>);

impl<
    E: TraceEntry,
    const TARGET_BUFFER_SIZE: usize,
    const LAST_ENTRY_STORAGE: usize,
    const RECOVERY_BUFFER_SIZE: usize,
    const IS_PERSISTENT: i32,
    const USE_LARGE_ENTRIES: i32,
    const USE_MONO_TIMESTAMPS: i32,
> JTraceConfig
    for Config<
        E,
        TARGET_BUFFER_SIZE,
        LAST_ENTRY_STORAGE,
        RECOVERY_BUFFER_SIZE,
        IS_PERSISTENT,
        USE_LARGE_ENTRIES,
        USE_MONO_TIMESTAMPS,
    >
{
    const TARGET_BUFFER_SIZE: usize = TARGET_BUFFER_SIZE;
    const LAST_ENTRY_STORAGE: usize = LAST_ENTRY_STORAGE;
    const RECOVERY_BUFFER_SIZE: usize = RECOVERY_BUFFER_SIZE;
    const IS_PERSISTENT: IsPersistent =
        if IS_PERSISTENT != 0 { IsPersistent::Yes } else { IsPersistent::No };
    const USE_LARGE_ENTRIES: UseLargeEntries =
        if USE_LARGE_ENTRIES != 0 { UseLargeEntries::Yes } else { UseLargeEntries::No };
    const USE_MONO_TIMESTAMPS: UseMonoTimestamps =
        if USE_MONO_TIMESTAMPS != 0 { UseMonoTimestamps::Yes } else { UseMonoTimestamps::No };

    type Entry = E;
    type LastCpuEntries = LastCpuEntries<E, LAST_ENTRY_STORAGE>;
    type RecoveryBuffer = AlignedBuffer<RECOVERY_BUFFER_SIZE>;
    type StorageBuffer = AlignedBuffer<TARGET_BUFFER_SIZE>;
}

/// On-buffer header at the start of a `jtrace` trace buffer.
#[repr(C)]
pub(super) struct Header<C: JTraceConfig> {
    /// Header magic value (`MAGIC` when active, `NO_MAGIC` when invalidated).
    pub magic: AtomicU32,
    /// Current write index and `TRACE_DISABLED_FLAG` bit.
    pub wr: AtomicU32,
    /// Optional per-CPU last-entry storage table.
    pub last_cpu_entries: UnsafeCell<C::LastCpuEntries>,
    /// Zero-sized array ensuring `Header` has the alignment of `C::Entry` even when
    /// `LAST_ENTRY_STORAGE == 0`.
    pub entries: [C::Entry; 0],
}

impl<C: JTraceConfig> Header<C> {
    /// Invalidated magic value (`0`).
    pub const NO_MAGIC: u32 = 0;
    /// Active header magic value (`"Jtrc"` in big-endian).
    pub const MAGIC: u32 = u32::from_be_bytes(*b"Jtrc");

    /// Initializes a `Header<C>` in-place at `ptr` without constructing a multi-kilobyte
    /// temporary on the 8 KB kernel stack.
    ///
    /// # Safety
    ///
    /// `ptr` must be non-null, aligned to `align_of::<Self>()`, and valid for writes of
    /// `size_of::<Self>()` bytes.
    #[inline]
    pub unsafe fn init_in_place(ptr: *mut Self) {
        // SAFETY: Caller guarantees `ptr` is valid and aligned for `size_of::<Self>()` bytes.
        // Zeroing initializes `wr = 0` and all `last_cpu_entries` fields (since `SmallEntry` and
        // `LargeEntry` default values are all-zero bit patterns).
        unsafe {
            ptr.write_bytes(0, 1);
            (&raw mut (*ptr).magic).write(AtomicU32::new(Self::MAGIC));
        }
    }
}

/// Jump trace ring-buffer manager.
pub(super) struct JTrace<C: JTraceConfig, H: TraceHooks> {
    pub(super) hooks: H,
    // Safety invariant: Either null, or aligned to `Header<C>` and pointing to a valid readable
    // and writable allocation of `storage_len >= size_of::<Header<C>>() + (entry_cnt as usize) *
    // size_of::<C::Entry>()` bytes containing an initialized `Header<C>` published via
    // `Ordering::Release`.
    storage_ptr: AtomicPtr<u8>,
    // Safety invariant: When `storage_ptr` is non-null, holds the byte length of the allocation
    // at `storage_ptr`.
    storage_len: AtomicUsize,
    // Safety invariant: Published via `Ordering::Release` before `storage_ptr` becomes non-null;
    // satisfies `size_of::<Header<C>>() + (entry_cnt as usize) * size_of::<C::Entry>() <=
    // storage_len`.
    entry_cnt: AtomicU32,
    trace_ops_in_flight: AtomicU32,
    after_thread_init_early: AtomicBool,
    // Safety invariant: Mutated only during `set_location` (or single-threaded test reset) and
    // read only during `dump_recovered`, which must not run concurrently with `set_location`.
    recovered_buf: UnsafeCell<C::RecoveryBuffer>,
}

// SAFETY: Concurrent `log`, `invalidate`, `dump`, and `get_location` operations synchronize
// via atomic fields (`storage_ptr`, `storage_len`, `entry_cnt`, `trace_ops_in_flight`,
// `after_thread_init_early`, and `Header::wr`), while `set_location` and `recovered_buf`
// mutation occur once during single-core early boot (or single-threaded test setup).
unsafe impl<C: JTraceConfig, H: TraceHooks + Sync> Sync for JTrace<C, H> {}

impl<C: JTraceConfig, H: TraceHooks> JTrace<C, H> {
    /// Required byte alignment for the trace buffer.
    pub const REQUIRED_BUFFER_ALIGNMENT: usize = core::mem::align_of::<Header<C>>();

    const TRACE_DISABLED_FLAG: u32 = 0x8000_0000;

    /// We never want to deliberately log a timestamp of zero.  The buffer is
    /// filled with zeros at the start, and we skip entries with zero time stamps
    /// when trying to find the read pointer during a dump operation.
    /// Unfortunately, if JTRACE entries are being created before the clock has
    /// been selected, the time they are going to get is always 0.  So, instead, we
    /// replace that value with this constant instead, and substitute back again
    /// during the dump.
    const ZERO_REPLACEMENT: zx_ticks_t = zx_ticks_t::MIN;

    /// Constructs a new `JTrace` instance using `hooks` and zeroed `recovered_buf`.
    pub const fn new(hooks: H, recovered_buf: C::RecoveryBuffer) -> Self {
        Self {
            hooks,
            storage_ptr: AtomicPtr::new(core::ptr::null_mut()),
            storage_len: AtomicUsize::new(0),
            entry_cnt: AtomicU32::new(0),
            trace_ops_in_flight: AtomicU32::new(0),
            after_thread_init_early: AtomicBool::new(false),
            recovered_buf: UnsafeCell::new(recovered_buf),
        }
    }

    /// Marks that `thread_init_early` has completed, enabling TID capture and preemption disabling.
    ///
    /// # Safety
    ///
    /// The caller must ensure that early thread initialization is complete so that
    /// [`ThreadPtr::current()`] is valid on the calling CPU.
    pub unsafe fn set_after_thread_init_early(&self) {
        // Relaxed ordering is sufficient because `set_after_thread_init_early` is a monotonic
        // one-way flag set once during single-CPU boot before secondary CPUs are brought online.
        self.after_thread_init_early.store(true, Ordering::Relaxed);
    }

    /// Sets the backing storage buffer for the trace log.
    ///
    /// # Safety
    ///
    /// The caller must ensure that this function is not called concurrently with itself or
    /// `dump_recovered`. If `storage.0` is non-null, aligned to `REQUIRED_BUFFER_ALIGNMENT`, and
    /// `storage.1 >= size_of::<Header<C>>() + size_of::<C::Entry>()`, the caller must ensure that
    /// `storage.0` refers to a live (not freed) object/allocation of `storage.1` initialized
    /// (e.g. zero-initialized or previously initialized persistent RAM) readable and writable
    /// bytes that remains valid for the lifetime of `self` (or until `reset` in unit tests).
    #[sanitize(address = "off")]
    pub(super) unsafe fn set_location(&self, storage: (*mut u8, usize)) {
        const {
            assert!(
                core::mem::size_of::<C::LastCpuEntries>()
                    == C::LAST_ENTRY_STORAGE * core::mem::size_of::<C::Entry>()
            );
            assert!(
                core::mem::align_of::<C::LastCpuEntries>() == core::mem::align_of::<C::Entry>()
            );
            assert!(core::mem::size_of::<C::RecoveryBuffer>() == C::RECOVERY_BUFFER_SIZE);
            assert!(core::mem::align_of::<C::RecoveryBuffer>() >= Self::REQUIRED_BUFFER_ALIGNMENT);
            assert!(core::mem::size_of::<C::Entry>() > 0);
            assert!(core::mem::size_of::<Header<C>>() <= u32::MAX as usize);
        }

        let (storage_data, storage_size) = storage;
        // The location of the trace buffer should only ever get set once, either
        // during the init call for a non-persistent log, or later on during the
        // allocation of persistent RAM during ZBI processing.  If an attempt is
        // made to set the location twice, simply ignore it.  Do not attempt to
        // debug assert, as we are very likely to be so early in boot that it would
        // be very difficult to debug such an assert.
        if !self.hdr().is_null() {
            return;
        }

        // Reject the buffer if it is null/empty for any reason, or if the pointer
        // provided does not meet the alignment requirements of our header.
        if storage_data.is_null()
            || storage_size == 0
            || !storage_data.addr().is_multiple_of(Self::REQUIRED_BUFFER_ALIGNMENT)
        {
            return;
        }

        // If the supplied storage is not enough to hold the header and even a
        // single trace entry, then just disable debug tracing by setting the header
        // location and the number of entries to nothing.  In theory, these should
        // already be nullptr and 0, but we explicitly set them anyway since (again)
        // ASSERTing is not really an option given how early on in boot we are
        // likely to be right now.
        let hdr_size = core::mem::size_of::<Header<C>>();
        let entry_size = core::mem::size_of::<C::Entry>();
        if storage_size < hdr_size + entry_size {
            return;
        }

        // If this is a persistent trace and we have a recovery buffer, attempt to
        // recover the log from our storage before proceeding to re-init our trace
        // storage.
        if C::RECOVERY_BUFFER_SIZE > 0 {
            let copy_len = core::cmp::min(C::RECOVERY_BUFFER_SIZE, storage_size);
            // SAFETY: Caller guarantees exclusive access to `recovered_buf` during `set_location`
            // and that `storage_data` is valid for `storage_size` bytes.
            unsafe {
                let dst = self.recovered_buf.get().cast::<u8>();
                core::ptr::copy_nonoverlapping(storage_data, dst, copy_len);
            }
        }

        // Go ahead and initialize our header and the number of entries we can hold
        // in our log, then flush this out to physical RAM if this is a persistent
        // trace buffer.
        // SAFETY: `storage_data` is non-null, aligned to `Header<C>`, and at least
        // `hdr_size + entry_size` bytes long.
        unsafe {
            Header::<C>::init_in_place(storage_data.cast::<Header<C>>());
        }
        let cnt = ((saturate(storage_size) as usize) - hdr_size) / entry_size;
        // Store `storage_len` and `entry_cnt` before publishing `storage_ptr` with
        // `Ordering::Release` so any reader that observes a non-null `storage_ptr` via
        // `Ordering::Acquire` also observes the initialized `Header<C>`, `entry_cnt`, and
        // `storage_len`.
        self.storage_len.store(storage_size, Ordering::Relaxed);
        self.entry_cnt.store(cnt as u32, Ordering::Release);
        self.storage_ptr.store(storage_data, Ordering::Release);
        // SAFETY: `storage_data` is valid for `storage_size` bytes.
        unsafe {
            Self::clean_cache(storage_data.cast::<c_void>(), storage_size);
        }
    }

    /// Returns the configured trace buffer pointer and byte length.
    #[inline]
    pub fn get_location(&self) -> (*mut u8, usize) {
        // Acquire load of `storage_ptr` synchronizes with the Release store at the end of
        // `set_location`, ensuring `storage_len` has already been written.
        let ptr = self.storage_ptr.load(Ordering::Acquire);
        if ptr.is_null() {
            (core::ptr::null_mut(), 0)
        } else {
            (ptr, self.storage_len.load(Ordering::Relaxed))
        }
    }

    /// Invalidates the trace buffer header magic.
    pub fn invalidate(&self) {
        let hdr = self.hdr();
        if !hdr.is_null() {
            // SAFETY: `self.hdr()` performs an `Ordering::Acquire` load of `storage_ptr`, which
            // synchronizes with the `Ordering::Release` store in `set_location` after `Header<C>`
            // was initialized in a valid aligned allocation of at least `size_of::<Header<C>>()`
            // bytes. `Relaxed` is sufficient for `magic` because it is flushed to physical RAM via
            // `clean_cache` immediately below for post-reboot recovery.
            unsafe {
                (*hdr).magic.store(Header::<C>::NO_MAGIC, Ordering::Relaxed);
                Self::clean_cache(hdr.cast::<c_void>(), core::mem::size_of::<Header<C>>());
            }
        }
    }

    /// Records `entry` into the ring buffer and optional per-CPU last-entry table.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `entry`'s pointer fields (`tag`, and for [`LargeEntry`]
    /// `ffl_info` and its `file`/`func` pointers) are either null or point to valid `'static`
    /// null-terminated C strings / [`FileFuncLineInfo`](super::FileFuncLineInfo).
    #[sanitize(address = "off")]
    pub unsafe fn log(&self, entry: &mut C::Entry) {
        let hdr = self.hdr();
        if hdr.is_null() {
            core::hint::cold_path();
            return;
        }

        // Record the fact that there is now a trace operation in progress, making
        // sure that we remember to decrement this count when we exit this method.
        self.trace_ops_in_flight.fetch_add(1, Ordering::AcqRel);
        let _cleanup = zr::defer(|| {
            self.trace_ops_in_flight.fetch_sub(1, Ordering::AcqRel);
        });

        // SAFETY: `hdr` is non-null and was initialized before `storage_ptr` was published with
        // `Ordering::Release`.
        let wr_atomic = unsafe { &(*hdr).wr };

        // Try to reserve a slot to write a record into.  This should only fail if
        // the tracing is temporarily disabled for dumping.
        let Some(wr) = self.reserve_slot(wr_atomic) else {
            core::hint::cold_path();
            return;
        };

        // Go ahead and finish filling out the entry, then copy the entry into the
        // main trace buffer, and (if enabled) into the per-cpu last entry slot in
        // the header.  Please note 2 potentially subtle points about this:
        //
        // 1) We mutate the log entry passed to us by the caller to finish filling
        //    it out, then record that entry in both places.  It is important that
        //    we don't copy the record into the main trace buffer and _then_ finish
        //    filling out the record (copying it from the main trace buffer to the
        //    per-cpu header slot afterwards).  There is a small chance that between
        //    these operations, the log ends up wrapping and our currently assigned
        //    slot gets stomped (causing the per-cpu record to be corrupted).
        // 2) We need to disable preemption for the time period between when we
        //    record the CPU ID in the trace entry, and when write that entry into
        //    the per-cpu slot.  Failure to do this could result in us recording CPU
        //    X in the entry, but then being moved to CPU Y before writing to the
        //    per-cpu slot for X (and setting up a potential race between us and
        //    whoever is running on CPU X now).  Note that we cannot disable
        //    preemption until after thread_init_early has been called, but at that
        //    point in boot, interrupts are disabled and the secondary CPUs are not
        //    running yet, so that should be OK.  Also, we don't want to disable and
        //    later attempt to re-enable preemption if interrupts are currently off
        //    (as they would be when a spinlock is held) as it _could_ trip an
        //    assert in the consistency checks for the preempt-disabler.  If
        //    interrupts are already disabled, we should be fine.  We can't change
        //    CPUs at that point in time anyway.
        let after_early = self.after_thread_init_early.load(Ordering::Relaxed);
        let disable_preemption = after_early && !ints_disabled();
        if disable_preemption {
            preempt_disable();
        }

        let mut ts_ticks = if C::USE_MONO_TIMESTAMPS == UseMonoTimestamps::Yes {
            timer_current_mono_ticks().0
        } else {
            timer_current_boot_ticks().0
        };
        if ts_ticks == 0 {
            ts_ticks = Self::ZERO_REPLACEMENT;
        }

        let cpu_id = curr_cpu_num();
        {
            let mut fields = entry.fields_mut();
            *fields.ts_ticks = ts_ticks;
            *fields.cpu_id = cpu_id;
            if C::USE_LARGE_ENTRIES == UseLargeEntries::Yes {
                let tid = if after_early {
                    // SAFETY: `ThreadPtr::current()` is valid once `after_thread_init_early` is
                    // set.
                    unsafe { ThreadPtr::current().tid() }
                } else {
                    UNKNOWN_TID
                };
                fields.set_tid(tid);
            }
        }

        // SAFETY: `self.hdr()` acquired a non-null `storage_ptr` with full allocation provenance
        // over `Header<C>` followed immediately at offset `size_of::<Header<C>>()` by `entry_cnt`
        // elements of `C::Entry`. `wr < entry_cnt` was reserved atomically by `reserve_slot()`.
        // Preemption (or interrupts / early single-CPU execution) is disabled between
        // `curr_cpu_num()` and writing `last_cpu_entries[cpu_id]`, and `cpu_id <
        // LAST_ENTRY_STORAGE` is checked before indexing `last_cpu_entries`.
        let entry_ptr = unsafe {
            let slot = hdr.add(1).cast::<C::Entry>().add(wr as usize);
            slot.write(*entry);
            if C::LAST_ENTRY_STORAGE > 0 && (cpu_id as usize) < C::LAST_ENTRY_STORAGE {
                UnsafeCell::raw_get(&raw const (*hdr).last_cpu_entries)
                    .cast::<C::Entry>()
                    .add(cpu_id as usize)
                    .write(*entry);
            }
            slot
        };

        if disable_preemption {
            // SAFETY: Paired with `preempt_disable()` above on the current thread, and no
            // `PreemptDisabledToken` is live.
            unsafe {
                preempt_reenable();
            }
        }

        // Flush the header and the entry if this is a persistent trace.
        // SAFETY: `hdr` and `entry_ptr` lie within the valid `storage_ptr` allocation.
        unsafe {
            Self::clean_cache(hdr.cast::<c_void>(), core::mem::size_of::<Header<C>>());
            Self::clean_cache(entry_ptr.cast::<c_void>(), core::mem::size_of::<C::Entry>());
        }
    }

    /// `timeout` controls how long this method will wait (spin) for other threads
    /// to complete in progress writes before continuing on and dumping the buffer.
    pub fn dump(&self, timeout: zx_duration_boot_t) {
        let hdr = self.hdr();
        if hdr.is_null() {
            self.hooks.print_warning(b"No debug trace buffer was ever configured\n");
            return;
        }

        // SAFETY: `hdr` is non-null and was initialized before `storage_ptr` was published with
        // `Ordering::Release`.
        let wr_atomic = unsafe { &(*hdr).wr };

        // Disable tracing and give any thread currently in the process of writing a
        // record some time to get out of the way.  Note that this is a best effort
        // approach to disabling the tracing during our dump.  It is important that
        // debug tracing remain lockless at all times.  The worst case scenario here
        // is that we end up with a partially garbled trace record.
        self.set_trace_enabled(wr_atomic, false);

        let ticks_to_time_ratio = timer_get_ticks_to_time_ratio();
        let deadline: zx_instant_boot_ticks_t = timer_current_boot_ticks()
            .0
            .saturating_add(ticks_to_time_ratio.inverse().scale::<{ Round::DOWN }>(timeout));
        while self.trace_ops_in_flight.load(Ordering::Acquire) > 0
            && timer_current_boot_ticks().0 < deadline
        {
            // just spin while we wait.
            arch_yield();
        }

        // Print a warning if we never saw the in flight op-count hit zero, then go
        // ahead and dump the current buffer.
        if timer_current_boot_ticks().0 >= deadline {
            self.hooks.print_warning(
                concat!(
                    "Warning: ops in flight was never observed at zero while waiting to dump the ",
                    "current trace buffer.  Some trace records might be corrupt.\n",
                )
                .as_bytes(),
            );
        }

        let entry_cnt = self.entry_cnt.load(Ordering::Acquire) as usize;
        let data_len =
            core::mem::size_of::<Header<C>>() + (core::mem::size_of::<C::Entry>() * entry_cnt);
        // SAFETY: `hdr` is non-null and `set_location` verified the buffer is 8-byte aligned and
        // at least `data_len` bytes long.
        unsafe {
            self.dump_buf(hdr.cast::<u8>().cast_const(), data_len, TraceBufferType::Current);
        }

        // Finally, go ahead and re-enable tracing.
        self.set_trace_enabled(wr_atomic, true);
    }

    /// Dumps the recovered persistent trace buffer, if configured.
    pub fn dump_recovered(&self) {
        if C::RECOVERY_BUFFER_SIZE > 0 {
            // SAFETY: `recovered_buf` is aligned to `REQUIRED_BUFFER_ALIGNMENT`, has size
            // `C::RECOVERY_BUFFER_SIZE`, and is only mutated in `set_location` (or single-threaded
            // test `reset`), which is not called concurrently with `dump_recovered`.
            unsafe {
                self.dump_buf(
                    self.recovered_buf.get().cast::<u8>().cast_const(),
                    C::RECOVERY_BUFFER_SIZE,
                    TraceBufferType::Recovered,
                );
            }
        } else {
            self.hooks.print_warning(
                concat!(
                    "Debug tracing is not configured for persistent tracing.  There is no ",
                    "recovered buffer to dump.\n",
                )
                .as_bytes(),
            );
        }
    }

    /// Flushes CPU cache lines covering `[ptr, ptr + len)` to physical RAM when persistent.
    ///
    /// # Safety
    ///
    /// The caller must ensure that if `len > 0`, `[ptr, ptr + len)` is a valid mapped kernel
    /// virtual address range.
    #[inline]
    unsafe fn clean_cache(ptr: *mut c_void, len: usize) {
        // Trace data only needs to be flushed to physical RAM if the trace is meant
        // to be persistent.
        if C::IS_PERSISTENT == IsPersistent::Yes {
            // SAFETY: Discharged by caller per this function's `# Safety` contract.
            unsafe {
                clean_cache_range(ptr.cast::<u8>().cast_const(), len);
            }
        }
    }

    #[inline]
    pub(super) fn hdr(&self) -> *mut Header<C> {
        self.storage_ptr.load(Ordering::Acquire).cast::<Header<C>>()
    }

    /// Returns the configured trace entry capacity for unit tests.
    #[cfg(ktest)]
    #[inline]
    pub(super) fn entry_cnt(&self) -> u32 {
        self.entry_cnt.load(Ordering::Acquire)
    }

    /// Resets the trace instance state in-place for unit tests.
    #[cfg(ktest)]
    pub(super) fn reset(&mut self) {
        *self.storage_ptr.get_mut() = core::ptr::null_mut();
        *self.storage_len.get_mut() = 0;
        *self.entry_cnt.get_mut() = 0;
        *self.trace_ops_in_flight.get_mut() = 0;
        *self.after_thread_init_early.get_mut() = false;
        if C::RECOVERY_BUFFER_SIZE > 0 {
            self.recovered_buf.get_mut().as_mut().fill(0);
        }
    }

    #[sanitize(address = "off")]
    fn reserve_slot(&self, wr_atomic: &AtomicU32) -> Option<u32> {
        let entry_cnt = self.entry_cnt.load(Ordering::Acquire);
        if entry_cnt == 0 {
            core::hint::cold_path();
            return None;
        }
        let mut wr;
        loop {
            wr = wr_atomic.load(Ordering::Acquire);
            if (wr & Self::TRACE_DISABLED_FLAG) != 0 {
                core::hint::cold_path();
                return None;
            }

            let next_wr = (wr + 1) % entry_cnt;
            if wr_atomic
                .compare_exchange_weak(wr, next_wr, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
            core::hint::cold_path();
        }
        Some(wr)
    }

    fn set_trace_enabled(&self, wr_atomic: &AtomicU32, enabled: bool) {
        let mask = u32::from(!enabled) * Self::TRACE_DISABLED_FLAG;
        loop {
            let wr = wr_atomic.load(Ordering::Acquire);
            let next_wr = (wr & !Self::TRACE_DISABLED_FLAG) | mask;
            if wr_atomic
                .compare_exchange_weak(wr, next_wr, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
        }
    }

    /// Dumps the trace buffer at `[buf_ptr, buf_ptr + buf_len)`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `buf_ptr` is non-null, aligned to
    /// [`Self::REQUIRED_BUFFER_ALIGNMENT`], and refers to a live (not freed) object/allocation of
    /// at least `buf_len` initialized bytes valid for reads for the duration of this call.
    #[sanitize(address = "off")]
    unsafe fn dump_buf(&self, buf_ptr: *const u8, buf_len: usize, buf_type: TraceBufferType) {
        let mut dump_corrupted_log = zr::defer(|| {
            self.hooks.print_warning(b"JTRACE: Dumping corrupted log\n");
            // SAFETY: Caller guarantees `buf_ptr` refers to a live (not freed) allocation valid
            // for reads of `buf_len` initialized bytes, and `hexdump` is only invoked for
            // corrupted recovery buffers (which are not concurrently mutated).
            let data = unsafe { zr::slice_from_raw_parts(buf_ptr, buf_len) };
            self.hooks.hexdump(data);
        });

        let entry_cnt = self.entry_cnt.load(Ordering::Acquire);
        let total_size = core::mem::size_of::<Header<C>>()
            + ((entry_cnt as usize) * core::mem::size_of::<C::Entry>());
        if total_size > buf_len {
            let mut linebuf = [0u8; 128];
            self.hooks.print_warning(kformat!(
                &mut linebuf,
                "JTRACE: recovery buffer too small ({:u}) to hold {:u} entries\n",
                buf_len,
                entry_cnt
            ));
            return;
        }

        if !buf_ptr.addr().is_multiple_of(Self::REQUIRED_BUFFER_ALIGNMENT) {
            return;
        }
        let hdr_ptr = buf_ptr.cast::<Header<C>>();
        // SAFETY: `buf_len >= total_size == size_of::<Header<C>>() + entry_cnt * size_of::<E>()`,
        // and `buf_ptr` is aligned to `Header<C>`.
        let magic = unsafe { (*hdr_ptr).magic.load(Ordering::Relaxed) };
        if magic == Header::<C>::NO_MAGIC {
            let mut linebuf = [0u8; 128];
            self.hooks.print_info(kformat!(
                &mut linebuf,
                "JTRACE: Log appears clean, not dumping it (recoverd {:p} main {:p} len {:u})\n",
                buf_ptr,
                self.hdr(),
                total_size
            ));
            dump_corrupted_log.cancel();
            return;
        }

        // SAFETY: `hdr_ptr` is valid and aligned for `Header<C>`.
        let raw_wr = unsafe { (*hdr_ptr).wr.load(Ordering::SeqCst) };
        let wr = raw_wr & !Self::TRACE_DISABLED_FLAG;
        if magic != Header::<C>::MAGIC || wr >= entry_cnt {
            let mut linebuf = [0u8; 128];
            self.hooks.print_warning(kformat!(
                &mut linebuf,
                "JTRACE: Bad header: Magic 0x{:08x} Wr {:u} Entries {:u}\n",
                magic,
                raw_wr,
                entry_cnt
            ));
            return;
        }

        dump_corrupted_log.cancel();

        // SAFETY: `hdr_ptr.add(1)` points to `entry_cnt` initialized elements of `C::Entry`
        // within `[buf_ptr, buf_ptr + total_size)`.
        let entries_ptr = unsafe { hdr_ptr.add(1).cast::<C::Entry>() };
        let read_entry = |idx: u32| -> C::Entry {
            debug_assert!(idx < entry_cnt);
            // SAFETY: `idx < entry_cnt`, and reading each `C::Entry` by value via raw pointer
            // avoids materializing a shared `&[C::Entry]` slice over the live lock-free buffer.
            unsafe { entries_ptr.add(idx as usize).read() }
        };

        // Figure out how many entries we will dump, and where to start dumping
        // from.  We skip any initial entries which have a timestamp of zero.  We
        // never deliberately write a timestamps of zero, meaning that the entries
        // are skipping were never written, because the trace never wrapped.
        let mut rd = wr;
        let mut todo = entry_cnt;
        while todo != 0 {
            let e = read_entry(rd);
            if e.fields().ts_ticks != 0 {
                break;
            }
            rd = (rd + 1) % entry_cnt;
            todo -= 1;
        }

        let convert_timestamp = |e: &C::Entry| -> zx_time_t {
            let ticks_to_time_ratio = timer_get_ticks_to_time_ratio();
            let raw_ts = e.fields().ts_ticks;
            let ts = if raw_ts == Self::ZERO_REPLACEMENT { 0 } else { raw_ts };
            ticks_to_time_ratio.scale::<{ Round::DOWN }>(ts)
        };

        if todo != 0 {
            {
                let mut linebuf = [0u8; 64];
                self.hooks.print_info(kformat!(
                    &mut linebuf,
                    "JTRACE: Recovered {:u}/{:u} entries\n",
                    todo,
                    entry_cnt
                ));
            }
            let first_entry = read_entry(rd);
            let mut prev_ts = convert_timestamp(&first_entry);

            while todo != 0 {
                let e = read_entry(rd);
                let ts = convert_timestamp(&e);

                e.print_trace_entry(&self.hooks, buf_type, ts, ts.wrapping_sub(prev_ts));
                prev_ts = ts;
                rd = (rd + 1) % entry_cnt;
                todo -= 1;
            }

            // If we are configured to track per-cpu last events, go ahead and print
            // them out.
            if C::LAST_ENTRY_STORAGE > 0 {
                self.hooks.per_cpu_dump_started();
                self.hooks.print_info(b"\n");
                self.hooks.print_info(b"JTRACE: Last recorded per-CPU events.\n");

                if C::LAST_ENTRY_STORAGE != (max_num_cpus() as usize) {
                    let mut linebuf = [0u8; 128];
                    self.hooks.print_warning(kformat!(
                        &mut linebuf,
                        concat!(
                            "JTRACE: Warning! Configured per-cpu last entry count ({:u}) does not ",
                            "match target's number of CPUs ({:u})\n",
                        ),
                        C::LAST_ENTRY_STORAGE,
                        max_num_cpus()
                    ));
                }

                // SAFETY: `hdr_ptr` points to a valid `Header<C>` containing `LAST_ENTRY_STORAGE`
                // initialized elements of `C::Entry`.
                let raw_last_cpu = unsafe { &raw const (*hdr_ptr).last_cpu_entries };
                let last_cpu_ptr =
                    UnsafeCell::raw_get(raw_last_cpu).cast::<C::Entry>().cast_const();
                for i in 0..C::LAST_ENTRY_STORAGE {
                    // SAFETY: `i < C::LAST_ENTRY_STORAGE`, and reading by value avoids holding a
                    // shared slice over `last_cpu_entries`.
                    let e = unsafe { last_cpu_ptr.add(i).read() };
                    let ts = convert_timestamp(&e);
                    e.print_trace_entry(&self.hooks, buf_type, ts, 0);
                }
            }

            let last_sec = prev_ts / zx_sec(1);
            let last_nsec = prev_ts % zx_sec(1);
            self.hooks.print_info(b"\n");
            {
                let mut linebuf = [0u8; 64];
                self.hooks.print_info(kformat!(
                    &mut linebuf,
                    "JTRACE: Last log timestamp [{:4d}.{:09d}]\n",
                    last_sec,
                    last_nsec
                ));
            }
        } else {
            self.hooks.print_info(b"JTRACE: no entries\n");
        }
    }
}
