// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::cell::UnsafeCell;
#[cfg(console_enabled)]
use core::ffi::c_int;
use core::ffi::{c_char, c_void};
use core::pin::Pin;
use core::ptr::NonNull;

#[cfg(console_enabled)]
use kprint::{kprint, kprintln};
#[cfg(console_enabled)]
use zx_status::Status;

unsafe extern "C" {
    fn arch_clean_cache_range(start: usize, len: usize);
}

// Persistent RAM allocation granularity configured for the kernel build.
const PERSISTENT_RAM_ALLOCATION_GRANULARITY: usize =
    zr::parse_usize(env!("PERSISTENT_RAM_ALLOCATION_GRANULARITY"))
        .expect("PERSISTENT_RAM_ALLOCATION_GRANULARITY invalid");
zr::static_assert!(PERSISTENT_RAM_ALLOCATION_GRANULARITY.is_power_of_two());

// Verify that the persistent debuglog minimum size is a multiple of the
// persistent RAM allocation granularity, and fits within u32::MAX.
const TARGET_PERSISTENT_DEBUGLOG_SIZE: usize =
    zr::parse_usize(env!("TARGET_PERSISTENT_DEBUGLOG_SIZE"))
        .expect("TARGET_PERSISTENT_DEBUGLOG_SIZE invalid");
// Minimum reserved crashlog size must be a multiple of the persistent RAM allocation
// granularity
zr::static_assert!(
    TARGET_PERSISTENT_DEBUGLOG_SIZE.is_multiple_of(PERSISTENT_RAM_ALLOCATION_GRANULARITY)
);
zr::static_assert!(TARGET_PERSISTENT_DEBUGLOG_SIZE <= u32::MAX as usize);

/// Flushes CPU data cache lines covering `bytes` to main memory so that
/// persistent debuglog writes survive non-power-cycle warm resets.
#[inline]
fn clean_cache_range(bytes: &[u8]) {
    // SAFETY: `arch_clean_cache_range` flushes CPU data cache lines covering
    // `[addr, addr + len)` to main memory and does not dereference or mutate
    // Rust references.
    unsafe {
        arch_clean_cache_range(bytes.as_ptr().addr(), bytes.len());
    }
}

/// On-RAM header at the start of the persistent debuglog region.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct LogHeader {
    magic: u32,
    rd_ptr: u32,
}

zr::static_assert!(
    core::mem::size_of::<LogHeader>() == 8 && core::mem::align_of::<LogHeader>() == 1
);
// Persistent dlog header's alignment must be a power of 2 in size
zr::static_assert!(core::mem::align_of::<LogHeader>().is_power_of_two());

impl LogHeader {
    /// FourCC `"Plog"` in little-endian byte order (`0x676f_6c50`).
    const MAGIC: u32 = u32::from_le_bytes(*b"Plog");

    #[inline]
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: `LogHeader` is `#[repr(C, packed)]` with two `u32` fields and no padding,
        // so all `size_of::<Self>()` bytes are initialized and valid to view as `&[u8]`.
        unsafe {
            core::slice::from_raw_parts(
                core::ptr::from_ref(self).cast::<u8>(),
                core::mem::size_of::<Self>(),
            )
        }
    }

    #[inline]
    fn is_magic_valid(&self) -> bool {
        self.magic == Self::MAGIC
    }

    #[inline]
    fn validate_magic(&mut self) {
        self.magic = Self::MAGIC;
        clean_cache_range(self.as_bytes());
    }

    #[inline]
    fn invalidate_magic(&mut self) {
        self.magic = 0;
        clean_cache_range(self.as_bytes());
    }
}

/// Info about where the persisted log is in RAM, if we have a persisted log.
/// Laid out to match the 16-byte C++ `plog` representation when wrapped in
/// `Option<ActivePersistentLog>`.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct ActivePersistentLog {
    hdr: NonNull<LogHeader>,
    payload_size: u32,
}

zr::static_assert!(
    core::mem::size_of::<ActivePersistentLog>() == 16
        && core::mem::align_of::<ActivePersistentLog>() == 8
);
zr::static_assert!(
    core::mem::size_of::<Option<ActivePersistentLog>>() == 16
        && core::mem::align_of::<Option<ActivePersistentLog>>() == 8
);

// SAFETY: `ActivePersistentLog` encapsulates a persistent RAM mapping owned and
// synchronized by `PersistentDebugLog`'s spinlock.
unsafe impl Send for ActivePersistentLog {}

impl ActivePersistentLog {
    /// Creates a new `ActivePersistentLog` over a persistent RAM allocation.
    ///
    /// # Safety
    /// `hdr` must point to a valid, readable and writable `LogHeader` followed immediately by
    /// `payload_size` readable and writable bytes within the same persistent RAM allocation,
    /// and all accesses through this `ActivePersistentLog` must be synchronized by
    /// `persistent_log_lock`.
    const unsafe fn new(hdr: NonNull<LogHeader>, payload_size: u32) -> Self {
        Self { hdr, payload_size }
    }

    /// Returns a shared reference to the `LogHeader`.
    #[inline]
    fn header(&self) -> &LogHeader {
        // SAFETY: `ActivePersistentLog::new` guarantees `self.hdr` is valid and non-aliased
        // mutably while `self` is borrowed.
        unsafe { self.hdr.as_ref() }
    }

    /// Returns an exclusive reference to the `LogHeader`.
    #[inline]
    fn header_mut(&mut self) -> &mut LogHeader {
        // SAFETY: `ActivePersistentLog::new` guarantees `self.hdr` is valid and exclusively
        // accessed while `self` is mutably borrowed.
        unsafe { self.hdr.as_mut() }
    }

    /// Returns a shared slice of the payload bytes immediately following `self.hdr`.
    #[inline]
    fn payload(&self) -> &[u8] {
        // SAFETY: `ActivePersistentLog::new` guarantees `self.hdr` is followed immediately by
        // `self.payload_size` valid bytes within the same allocation.
        unsafe {
            let payload_ptr = self.hdr.as_ptr().add(1).cast::<u8>().cast_const();
            core::slice::from_raw_parts(payload_ptr, self.payload_size as usize)
        }
    }

    /// Returns an exclusive slice of the payload bytes immediately following `self.hdr`.
    #[inline]
    fn payload_mut(&mut self) -> &mut [u8] {
        // SAFETY: `ActivePersistentLog::new` guarantees `self.hdr` is followed immediately by
        // `self.payload_size` valid writable bytes within the same allocation.
        unsafe {
            let payload_ptr = self.hdr.as_ptr().add(1).cast::<u8>();
            core::slice::from_raw_parts_mut(payload_ptr, self.payload_size as usize)
        }
    }

    /// Returns simultaneous exclusive references to the `LogHeader` and the disjoint payload
    /// byte slice immediately following it.
    #[inline]
    fn header_and_payload_mut(&mut self) -> (&mut LogHeader, &mut [u8]) {
        // SAFETY: `ActivePersistentLog::new` guarantees `self.hdr` points to a valid
        // `LogHeader` followed immediately by `self.payload_size` disjoint payload bytes in the
        // same allocation, and `&mut self` ensures exclusive access to both regions.
        unsafe {
            let hdr_ptr = self.hdr.as_ptr();
            let payload_ptr = hdr_ptr.add(1).cast::<u8>();
            (
                &mut *hdr_ptr,
                core::slice::from_raw_parts_mut(payload_ptr, self.payload_size as usize),
            )
        }
    }

    /// Invalidates the state of the persistent dlog.  This gets called every time we
    /// gracefully reboot, so that we don't end up recovering a dlog after reboot and
    /// end up producing an unnecessary crashlog.
    #[inline]
    fn invalidate(&mut self) {
        self.header_mut().invalidate_magic();
    }
}

/// We don't bother to lock this structure.  The log, if present, is recovered
/// during early boot while we are still running on a single core.  After that,
/// the recovered data is only ever accessed in a read only fashion, so there
/// is no real need to provide any explicit synchronization.
#[repr(C)]
#[derive(Debug)]
struct RecoveredPersistentLog {
    data: Option<NonNull<u8>>,
    capacity: u32,
    size: u32,
}

zr::static_assert!(
    core::mem::size_of::<RecoveredPersistentLog>() == 16
        && core::mem::align_of::<RecoveredPersistentLog>() == 8
);

// SAFETY: `RecoveredPersistentLog` points to statically allocated recovery storage
// populated during single-core early boot (or under test control) and read-only thereafter.
unsafe impl Send for RecoveredPersistentLog {}
// SAFETY: After single-core early boot initialization, `RecoveredPersistentLog` is accessed
// in a read-only fashion.
unsafe impl Sync for RecoveredPersistentLog {}

impl RecoveredPersistentLog {
    const fn new(data: Option<NonNull<u8>>, capacity: u32) -> Self {
        Self { data, capacity, size: 0 }
    }

    /// Used only by testing to reset a log to its "pre SetLocation'ed" state.
    #[cfg(ktest)]
    #[inline]
    fn reset(&mut self) {
        self.size = 0;
    }

    /// Fetch a string view which references the recovered crashlog (if any).
    /// Returns `None` when `size == 0`.
    #[inline]
    fn as_bytes(&self) -> Option<&[u8]> {
        if self.size == 0 {
            return None;
        }
        let ptr = self.data?;
        // SAFETY: When `self.data` is `Some(ptr)`, it points to a backing store of at least
        // `self.capacity` bytes, and `self.size <= self.capacity` bytes have been initialized.
        Some(unsafe { core::slice::from_raw_parts(ptr.as_ptr().cast_const(), self.size as usize) })
    }

    /// Recovers valid log contents from a circular persistent payload buffer starting at `rd_ptr`.
    ///
    /// # Safety
    /// If `self.data` is `Some(ptr)` and `self.capacity > 0`, `ptr` must point to a writable
    /// allocation of at least `self.capacity` bytes.
    unsafe fn recover_from(&mut self, payload: &[u8], rd_ptr: u32) {
        self.size = 0;
        let Some(destination_ptr) = self.data else {
            return;
        };
        if self.capacity == 0 || payload.is_empty() {
            return;
        }

        // Save as much as we can in our static buffer, discarding the oldest data first if we
        // cannot fit it all.
        // C++ DEBUG_ASSERTs capacity <= u32::MAX here; capacity is a u32 so this holds by
        // construction.
        let payload_size = payload.len();
        let capacity = self.capacity as usize;
        let (bytes_to_process, mut read_offset) = if payload_size <= capacity {
            (payload_size, (rd_ptr as usize) % payload_size)
        } else {
            (capacity, ((rd_ptr as usize) + (payload_size - capacity)) % payload_size)
        };
        let mut write_offset: usize = 0;

        // SAFETY: Caller guarantees `destination_ptr` points to at least `self.capacity`
        // writable bytes.
        let destination =
            unsafe { core::slice::from_raw_parts_mut(destination_ptr.as_ptr(), capacity) };

        for _ in 0..bytes_to_process {
            let byte = payload[read_offset];
            read_offset += 1;
            if read_offset >= payload_size {
                read_offset = 0;
            }

            // Skip all zeros.
            if byte == 0 {
                continue;
            }

            destination[write_offset] =
                if byte.is_ascii_graphic() || byte == b' ' || byte == b'\n' { byte } else { b'?' };
            write_offset += 1;
        }
        self.size = write_offset as u32;
    }
}

/// Core persistent debuglog state machine managing the spinlock-protected active persistent RAM
/// ring buffer and the recovered previous-boot crashlog buffer.
#[ksync::guarded]
struct PersistentDebugLog {
    #[mutex]
    persistent_log_lock: ksync::KMutex<ksync::RawSpinlock>,

    /// Info about where the persisted log is in RAM, if we have a persisted log.
    #[guarded_by(persistent_log_lock)]
    plog: Option<ActivePersistentLog>,

    /// We don't bother to lock this structure.  The log, if present, is recovered
    /// during early boot while we are still running on a single core.  After that,
    /// the recovered data is only ever accessed in a read only fashion, so there
    /// is no real need to provide any explicit synchronization.
    recovered_persistent_log: UnsafeCell<RecoveredPersistentLog>,
}

// SAFETY: `PersistentDebugLog` synchronizes all mutable access to `plog` and
// `recovered_persistent_log` using `persistent_log_lock` (`IrqSavePolicy`), and
// `recovered_persistent_log` is only read after single-core early boot initialization.
unsafe impl Send for PersistentDebugLog {}
// SAFETY: Shared references to `PersistentDebugLog` synchronize mutations via
// `persistent_log_lock` (`IrqSavePolicy`) and only read `recovered_persistent_log` after
// early boot recovery.
unsafe impl Sync for PersistentDebugLog {}

impl core::fmt::Debug for PersistentDebugLog {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PersistentDebugLog")
            .field("persistent_log_lock", &self.persistent_log_lock)
            .field("plog", &self.plog)
            .finish_non_exhaustive()
    }
}

impl PersistentDebugLog {
    /// Returns a pin-initializer for `PersistentDebugLog` backed by `recovered_data` of length
    /// `recovered_capacity`.
    fn init(
        recovered_data: Option<NonNull<u8>>,
        recovered_capacity: u32,
    ) -> impl pin_init::PinInit<Self, core::convert::Infallible> {
        pin_init::pin_init!(Self {
            persistent_log_lock <- ksync::KMutex::init(),
            plog: ksync::KCell::new(None),
            recovered_persistent_log: UnsafeCell::new(RecoveredPersistentLog::new(
                recovered_data,
                recovered_capacity,
            )),
        })
    }

    /// Called very early in boot.  Attempts to recover any previously persisted
    /// log by first performing consistency checks on the header, and then
    /// recovering as much as it can into the recovery buffer.
    ///
    /// # Safety
    /// If `virt` is non-null and `len > size_of::<LogHeader>()`, `virt` must point to a valid,
    /// readable and writable persistent RAM mapping of `len` bytes that remains valid while this
    /// `PersistentDebugLog` is active (or until `force_reset` is called in tests).
    unsafe fn set_location(&self, virt: *mut u8, len: usize) {
        // The location of the persistent dlog must be compatible with the header's
        // alignment, and the amount of space for the log payload must be positive,
        // otherwise we cannot effectively use the memory.
        let Some(virt_ptr) = NonNull::new(virt) else {
            return;
        };
        let header_size = core::mem::size_of::<LogHeader>();
        let header_align = core::mem::align_of::<LogHeader>();
        if (virt_ptr.addr().get() & (header_align - 1)) != 0 || len <= header_size {
            return;
        }

        // If we already have a persistent dlog location, then this function has been
        // (improperly) called twice.  It is tempting to assert here, but we are
        // currently very early in boot, which would make debugging the assert
        // extremely difficult.  Instead, just ignore this request.
        //
        // TODO(johngro): come back here and try to put a warning/OOPS in to the dlog
        // buffer?
        ksync::lock!(
            let mut guard = self.lock_persistent_log_lock_policy::<ksync::IrqSavePolicy>()
        );
        if guard.plog().is_some() {
            return;
        }

        // Check our log header to see if we have a recovered persistent log.  If we
        // do, recover the log into our recovery buffer so that it can be picked up
        // later on and sent up to usermode in the crashlog.
        //
        // Note: we are at a point in boot where the heap has not been brought up yet,
        // which is why the recovery buffer needs to be statically allocated.  If this
        // becomes an issue some day, we can shift to a strategy where we remember
        // where the persistent log is, but don't actually start to use it until we
        // get a chance to save it off into a dynamically allocated buffer.
        let header_ptr = virt_ptr.cast::<LogHeader>();
        let payload_size = core::cmp::min(u32::MAX as usize, len - header_size) as u32;
        // SAFETY: Caller guarantees `virt` points to a valid persistent RAM buffer of `len`
        // bytes (`>= size_of::<LogHeader>() + payload_size`), and all accesses to `active_log`
        // are synchronized by `persistent_log_lock`.
        let mut active_log = unsafe { ActivePersistentLog::new(header_ptr, payload_size) };

        let header = active_log.header();
        let initial_rd_ptr = header.rd_ptr;
        if header.is_magic_valid() && initial_rd_ptr < payload_size {
            // This looks as good as it is going to.  Our magic number is valid, and our
            // read pointer lies within the available payload size.  Save as much as we
            // can in our static buffer, discarding the oldest data first if we cannot
            // fit it all.
            let payload = active_log.payload();
            // SAFETY: We hold `persistent_log_lock` exclusively during early boot (or after
            // `force_reset` in single-threaded unit tests), and `payload` is disjoint from
            // `self.recovered_persistent_log`.
            unsafe {
                let recovered_log = &mut *self.recovered_persistent_log.get();
                recovered_log.recover_from(payload, initial_rd_ptr);
            }
        }

        // Reset the log, then install it, and we are done.
        let header = active_log.header_mut();
        header.validate_magic();
        header.rd_ptr = 0;
        clean_cache_range(header.as_bytes());
        let payload = active_log.payload_mut();
        payload.fill(0);
        clean_cache_range(payload);
        *guard.as_mut().plog_mut() = Some(active_log);
    }

    /// Writes `str` to the persistent dlog, if enabled.  Otherwise, this is a
    /// no-op.
    fn write(&self, str: &[u8]) {
        ksync::lock!(
            let mut guard = self.lock_persistent_log_lock_policy::<ksync::IrqSavePolicy>()
        );

        // If we have no persistent log, just get out.
        let Some(plog) = guard.as_mut().plog_mut() else {
            return;
        };

        // Copy the data into our log
        let payload_size = plog.payload_size as usize;
        let (hdr, payload) = plog.header_and_payload_mut();
        let todo: usize;
        let offset: usize;
        if str.len() > payload_size {
            todo = payload_size;
            offset = str.len() - payload_size;
        } else {
            todo = str.len();
            offset = 0;
        }

        let rd_ptr = hdr.rd_ptr as usize;
        let space = payload_size - rd_ptr;
        if space > todo {
            payload[rd_ptr..rd_ptr + todo].copy_from_slice(&str[offset..offset + todo]);
            clean_cache_range(&payload[rd_ptr..rd_ptr + todo]);

            hdr.rd_ptr += todo as u32;
            clean_cache_range(hdr.as_bytes());
        } else {
            payload[rd_ptr..rd_ptr + space].copy_from_slice(&str[offset..offset + space]);
            payload[..todo - space].copy_from_slice(&str[offset + space..offset + todo]);
            clean_cache_range(&payload[rd_ptr..rd_ptr + space]);
            clean_cache_range(&payload[..todo - space]);

            hdr.rd_ptr = (todo - space) as u32;
            clean_cache_range(hdr.as_bytes());
        }
    }

    /// Fetch a string view which references the recovered crashlog (if any).
    #[inline]
    fn recovered_log(&self) -> Option<&[u8]> {
        // SAFETY: We don't bother to lock this structure.  The log, if present, is recovered
        // during early boot while we are still running on a single core.  After that,
        // the recovered data is only ever accessed in a read only fashion, so there
        // is no real need to provide any explicit synchronization.
        let recovered_log = unsafe { &*self.recovered_persistent_log.get() };
        recovered_log.as_bytes()
    }

    /// Invalidates the state of the persistent dlog.  This gets called every time we
    /// gracefully reboot, so that we don't end up recovering a dlog after reboot and
    /// end up producing an unnecessary crashlog.
    fn invalidate(&self) {
        ksync::lock!(
            let mut guard = self.lock_persistent_log_lock_policy::<ksync::IrqSavePolicy>()
        );
        if let Some(active_log) = guard.as_mut().plog_mut() {
            active_log.invalidate();
        }
    }

    /// Used only by testing to reset a log to its "pre SetLocation'ed" state.
    #[cfg(ktest)]
    fn force_reset(&self) {
        ksync::lock!(
            let mut guard = self.lock_persistent_log_lock_policy::<ksync::IrqSavePolicy>()
        );
        *guard.as_mut().plog_mut() = None;
        // SAFETY: `force_reset` is used only by single-threaded testing while holding
        // `persistent_log_lock` with `IrqSavePolicy`.
        let recovered_log = unsafe { &mut *self.recovered_persistent_log.get() };
        recovered_log.reset();
    }
}

/// Global persistent debuglog state and static recovery buffer parameterized by
/// `STORAGE_SIZE`. When `STORAGE_SIZE == 0`, all operations are no-ops and
/// `cmd` returns `-1`.
struct PersistentDebuglogGlobals<const STORAGE_SIZE: usize> {
    instance: lazy_init::LazyInit<PersistentDebugLog, lazy_init::NoCheck>,
    storage: UnsafeCell<[u8; STORAGE_SIZE]>,
}

// SAFETY: `PersistentDebuglogGlobals` synchronizes active log mutations via
// `PersistentDebugLog`'s spinlock (`IrqSavePolicy`) and only mutates `storage`
// during single-core early boot initialization before read-only access.
unsafe impl<const STORAGE_SIZE: usize> Send for PersistentDebuglogGlobals<STORAGE_SIZE> {}
// SAFETY: Shared references to `PersistentDebuglogGlobals` synchronize active log
// mutations via `PersistentDebugLog`'s spinlock (`IrqSavePolicy`) and only read
// `storage` after single-core early boot recovery.
unsafe impl<const STORAGE_SIZE: usize> Sync for PersistentDebuglogGlobals<STORAGE_SIZE> {}

impl<const STORAGE_SIZE: usize> PersistentDebuglogGlobals<STORAGE_SIZE> {
    /// Constructs a new `PersistentDebuglogGlobals` with an internal recovery buffer of
    /// `STORAGE_SIZE` bytes.
    const fn new() -> Self {
        const {
            assert!(STORAGE_SIZE <= u32::MAX as usize);
        }
        Self {
            instance: lazy_init::LazyInit::uninit(),
            storage: UnsafeCell::new([0; STORAGE_SIZE]),
        }
    }

    /// Called once from lib/debuglog during _very_ early init.
    fn init_early(&'static self) {
        if STORAGE_SIZE == 0 {
            return;
        }
        let storage_ptr = NonNull::new(self.storage.get().cast::<u8>());
        // SAFETY: `init_early` is called once from `lib/debuglog` during single-core early boot
        // before any other access to `self.instance`, and `self.storage` is valid for
        // `STORAGE_SIZE` bytes for the `'static` lifetime of `self`.
        let _ = unsafe {
            Pin::static_ref(&self.instance)
                .init_pin(PersistentDebugLog::init(storage_ptr, STORAGE_SIZE as u32))
        };
    }

    /// Sets the virtual address of where to store the persistent, assuming that we
    /// have one.  This needs to happen early in boot, usually during ZBI header
    /// processing, before we start up the secondary CPUs.
    ///
    /// # Safety
    /// `init_early` must have been called before calling this method (guaranteed by
    /// `lib/debuglog`). If `STORAGE_SIZE > 0`, `vaddr` is non-null, and
    /// `len > size_of::<LogHeader>()`, `vaddr` must point to a valid, readable and
    /// writable persistent RAM mapping of `len` bytes that remains valid while this
    /// log is active.
    unsafe fn set_location(&self, vaddr: *mut u8, len: usize) {
        if STORAGE_SIZE == 0 {
            return;
        }
        // SAFETY: Caller guarantees `init_early` has been called before `set_location` and
        // upholds the memory validity contract for `vaddr` and `len`.
        unsafe {
            self.instance.get_unchecked().set_location(vaddr, len);
        }
    }

    /// Safe wrapper around [`PersistentDebuglogGlobals::set_location`] taking a static mutable
    /// byte slice.
    fn set_location_slice(&self, buffer: &'static mut [u8]) {
        if STORAGE_SIZE == 0 {
            return;
        }
        // SAFETY: `init_early` is guaranteed by `lib/debuglog` to run during very early boot
        // before platform ZBI handoff calls `set_location_slice`, and `buffer` is a live,
        // uniquely borrowed `'static` byte slice of `buffer.len()` bytes that remains valid for
        // the lifetime of the kernel.
        unsafe {
            self.set_location(buffer.as_mut_ptr(), buffer.len());
        }
    }

    /// Writes a string to the persistent dlog, if enabled.  Otherwise, this is a
    /// no-op.
    fn write(&self, bytes: &[u8]) {
        if STORAGE_SIZE == 0 {
            return;
        }
        // SAFETY: `init_early` is called during very early boot in `lib/debuglog` before any
        // debuglog or console writes occur.
        unsafe {
            self.instance.get_unchecked().write(bytes);
        }
    }

    /// Invalidates the state of the persistent dlog.  This gets called every time we
    /// gracefully reboot, so that we don't end up recovering a dlog after reboot and
    /// end up producing an unnecessary crashlog.
    fn invalidate(&self) {
        if STORAGE_SIZE == 0 {
            return;
        }
        // SAFETY: `init_early` is called during very early boot before any reboot/shutdown
        // path calls `invalidate`.
        unsafe {
            self.instance.get_unchecked().invalidate();
        }
    }

    /// Fetch a string view which references the recovered crashlog (if any).
    #[inline]
    fn recovered_log(&self) -> Option<&[u8]> {
        if STORAGE_SIZE == 0 {
            return None;
        }
        // SAFETY: `init_early` is called during very early boot before any crashlog recovery
        // queries `recovered_log`.
        unsafe { self.instance.get_unchecked().recovered_log() }
    }

    /// Executes the `"pdlog"` kernel console command.
    ///
    /// # Safety
    /// If `argument_count > 0`, `argument_vector` must be non-null and point to at least
    /// `argument_count` initialized [`crate::console::CmdArgs`] elements.
    #[cfg(console_enabled)]
    unsafe fn cmd(
        &self,
        argument_count: c_int,
        argument_vector: *const crate::console::CmdArgs,
        _flags: u32,
    ) -> c_int {
        if STORAGE_SIZE == 0 {
            return -1;
        }

        let argument_count = usize::try_from(argument_count).unwrap_or(0);
        // SAFETY: Caller guarantees `argument_vector` points to `argument_count` initialized
        // `CmdArgs` elements whenever `argument_count > 0`; a zero or negative count yields `&[]`.
        let arguments = unsafe { zr::slice_from_raw_parts(argument_vector, argument_count) };

        let command_name = arguments.first().map(|arg| arg.as_str()).unwrap_or("pdlog");
        let usage = || -> c_int {
            kprintln!("usage:");
            kprintln!("{:s} dump : dump the recovered persistent debug log", command_name);
            Status::INTERNAL.into_raw()
        };

        if arguments.len() < 2 {
            kprintln!("not enough arguments");
            return usage();
        }

        if arguments[1].as_str() == "dump" {
            if let Some(recovered) = self.recovered_log()
                && !recovered.is_empty()
            {
                kprintln!("Recovered {} bytes from the persistent debug log.", recovered.len());
                kprintln!("---- BEGIN ----");
                kprint!("{:s}", recovered);
                kprintln!("---- END ----");
            } else {
                kprintln!("There was no persistent debug log recovered!");
            }
        } else {
            kprintln!("unknown command");
            return usage();
        }

        zx_status::sys::ZX_OK
    }
}

static GLOBAL_PERSISTENT_DEBUGLOG: PersistentDebuglogGlobals<TARGET_PERSISTENT_DEBUGLOG_SIZE> =
    PersistentDebuglogGlobals::new();

/// Called once from lib/debuglog during _very_ early init.
pub fn persistent_dlog_init_early() {
    GLOBAL_PERSISTENT_DEBUGLOG.init_early();
}

/// Sets where the persistent debuglog lives, assuming that we have one.  This
/// needs to happen early in boot, usually during ZBI header processing, before
/// we start up the secondary CPUs.  The persistent debuglog keeps using
/// `buffer` for the life of the kernel.
pub fn persistent_dlog_set_location(buffer: &'static mut [u8]) {
    GLOBAL_PERSISTENT_DEBUGLOG.set_location_slice(buffer);
}

/// Writes `bytes` to the persistent debuglog, if enabled.  Otherwise, this is a
/// no-op.
pub fn persistent_dlog_write(bytes: &[u8]) {
    GLOBAL_PERSISTENT_DEBUGLOG.write(bytes);
}

/// Invalidates the state of the persistent dlog.  This gets called every time we
/// gracefully reboot, so that we don't end up recovering a dlog after reboot and
/// end up producing an unnecessary crashlog.
pub fn persistent_dlog_invalidate() {
    GLOBAL_PERSISTENT_DEBUGLOG.invalidate();
}

/// Fetch a string view which references the recovered crashlog (if any).
pub fn persistent_dlog_get_recovered_log() -> Option<&'static [u8]> {
    GLOBAL_PERSISTENT_DEBUGLOG.recovered_log()
}

/// FFI entry point for C++ `persistent_dlog_init_early()`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_persistent_dlog_init_early() {
    persistent_dlog_init_early();
}

/// FFI entry point for C++ `persistent_dlog_set_location(void* vaddr, size_t len)`.
///
/// # Safety
/// `persistent_dlog_init_early` must have been called before calling this function.
/// If `vaddr` is non-null and `len > size_of::<LogHeader>()`, `vaddr` must point to a valid,
/// readable and writable persistent RAM mapping of `len` bytes that remains valid for the
/// lifetime of the kernel.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_persistent_dlog_set_location(vaddr: *mut c_void, len: usize) {
    // SAFETY: Caller upholds the early-init ordering and persistent RAM mapping validity
    // contract for `vaddr` and `len`.
    unsafe {
        GLOBAL_PERSISTENT_DEBUGLOG.set_location(vaddr.cast::<u8>(), len);
    }
}

/// FFI entry point for C++ `persistent_dlog_write(ktl::string_view str)`.
///
/// # Safety
/// If `len > 0`, `ptr` must be non-null and point to at least `len` initialized, readable
/// bytes that are not mutated for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_persistent_dlog_write(ptr: *const c_char, len: usize) {
    // SAFETY: Caller guarantees `ptr` is valid for `len` bytes whenever `len > 0`; the helper
    // yields an empty slice when `len == 0` regardless of `ptr`.
    let bytes = unsafe { zr::slice_from_raw_parts(ptr.cast::<u8>(), len) };
    persistent_dlog_write(bytes);
}

/// FFI entry point for C++ `persistent_dlog_invalidate()`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_persistent_dlog_invalidate() {
    persistent_dlog_invalidate();
}

/// FFI entry point for C++ `persistent_dlog_get_recovered_log()`.
///
/// # Safety
/// If `out_len` is non-null, it must point to a writable `usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_persistent_dlog_get_recovered_log(
    out_len: *mut usize,
) -> *const c_char {
    match persistent_dlog_get_recovered_log() {
        Some(bytes) => {
            if let Some(out_len_ptr) = NonNull::new(out_len) {
                // SAFETY: Caller guarantees `out_len` is writable when non-null.
                unsafe {
                    out_len_ptr.as_ptr().write(bytes.len());
                }
            }
            bytes.as_ptr().cast::<c_char>()
        }
        None => {
            if let Some(out_len_ptr) = NonNull::new(out_len) {
                // SAFETY: Caller guarantees `out_len` is writable when non-null.
                unsafe {
                    out_len_ptr.as_ptr().write(0);
                }
            }
            core::ptr::null()
        }
    }
}

/// Console command entry point for `"pdlog"`.
///
/// # Safety
/// If `argument_count > 0`, `argument_vector` must be non-null and point to at least
/// `argument_count` initialized [`crate::console::CmdArgs`] elements.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_pdlog(
    argument_count: c_int,
    argument_vector: *const crate::console::CmdArgs,
    flags: u32,
) -> c_int {
    // SAFETY: Caller guarantees `argument_vector` points to `argument_count` initialized
    // `CmdArgs` elements whenever `argument_count > 0`.
    unsafe { GLOBAL_PERSISTENT_DEBUGLOG.cmd(argument_count, argument_vector, flags) }
}

// C++ compiled this command out with #if TARGET_PERSISTENT_DEBUGLOG_SIZE > 0. We cannot
// #[cfg] on an env!() value, so register it with an empty availability mask instead, which
// makes it unreachable from both normal and panic consoles.
#[cfg(console_enabled)]
crate::console::static_command!(
    CMD_PDLOG,
    c"pdlog".as_ptr(),
    c"pdlog".as_ptr(),
    cmd_pdlog,
    if TARGET_PERSISTENT_DEBUGLOG_SIZE != 0 { crate::console::CMD_AVAIL_ALWAYS } else { 0 }
);

/// Persistent Debuglog Tests
#[cfg(ktest)]
#[unittest::suite(name = "pdlog_rust")]
mod tests {
    use super::{LogHeader, PersistentDebugLog};
    use core::ptr::NonNull;
    use unittest::{assert_eq, assert_false, assert_ge, assert_true, unwrap_some};

    struct Buffer {
        storage: kalloc::Box<[u8]>,
        capacity: u32,
    }

    impl Buffer {
        fn setup(new_capacity: u32) -> Option<Self> {
            if new_capacity == 0 {
                return None;
            }
            let storage = kalloc::Box::<[u8]>::try_new_zeroed_slice(new_capacity as usize).ok()?;
            Some(Self { storage, capacity: new_capacity })
        }
    }

    struct TestEnvironment {
        log: kalloc::Box<PersistentDebugLog>,
        log_buffer: Buffer,
        _recovered_log_buffer: Buffer,
        hdr: NonNull<LogHeader>,
    }

    impl TestEnvironment {
        fn setup(log_size: u32, recovered_size: u32) -> Option<Self> {
            let mut log_buffer = Buffer::setup(log_size)?;
            let mut recovered_log_buffer = Buffer::setup(recovered_size)?;
            let mut uninit_log = kalloc::Box::<PersistentDebugLog>::try_new_uninit().ok()?;
            let recovered_ptr = NonNull::new(recovered_log_buffer.storage.as_mut_ptr());
            let init = PersistentDebugLog::init(recovered_ptr, recovered_log_buffer.capacity);
            // SAFETY: `uninit_log.as_mut_ptr()` points to a valid, writable heap allocation for
            // `PersistentDebugLog` that remains pinned inside `kalloc::Box`.
            unsafe {
                let _ = pin_init::PinInit::__pinned_init(init, uninit_log.as_mut_ptr());
            }
            // SAFETY: `uninit_log` was initialized in-place by `__pinned_init` above.
            let log = unsafe { uninit_log.assume_init() };
            let hdr = NonNull::new(log_buffer.storage.as_mut_ptr().cast::<LogHeader>())?;
            Some(Self { log, log_buffer, _recovered_log_buffer: recovered_log_buffer, hdr })
        }

        fn set_location(&mut self) {
            // SAFETY: `self.log_buffer.storage` is a valid heap buffer of
            // `self.log_buffer.capacity` bytes that outlives `self.log`.
            unsafe {
                self.log.set_location(
                    self.log_buffer.storage.as_mut_ptr(),
                    self.log_buffer.capacity as usize,
                );
            }
        }

        fn hdr(&self) -> &LogHeader {
            // SAFETY: `self.hdr` points to the start of `self.log_buffer.storage` (which has at
            // least `size_of::<LogHeader>()` bytes, and `LogHeader` has alignment 1).
            unsafe { self.hdr.as_ref() }
        }

        fn hdr_mut(&mut self) -> &mut LogHeader {
            // SAFETY: `self.hdr` points to the start of `self.log_buffer.storage` with exclusive
            // access via `&mut self`.
            unsafe { self.hdr.as_mut() }
        }
    }

    fn check_recovered_log_is_empty(log: &PersistentDebugLog) -> bool {
        (unittest::subtest!(|log: &PersistentDebugLog| {
            assert_true!(log.recovered_log().is_none());
        }))(log)
    }

    fn check_recovered_log_matches(log: &PersistentDebugLog, test_vectors: &[&[u8]]) -> bool {
        (unittest::subtest!(|log: &PersistentDebugLog, test_vectors: &[&[u8]]| {
            // Check that our string view representation of the recovered log matches the
            // test vectors.
            let recovered = unwrap_some!(log.recovered_log());
            let mut offset = 0usize;
            for test_vector in test_vectors {
                let log_amt = recovered.len() - offset;
                assert_ge!(log_amt, test_vector.len());
                assert_true!(&recovered[offset..offset + test_vector.len()] == *test_vector);
                offset += test_vector.len();
            }
            assert_eq!(recovered.len(), offset);
        }))(log, test_vectors)
    }

    /// Tests basic persistent debuglog initialization, writes, and crashlog recovery.
    #[test]
    fn pdlog_basic_test() {
        let mut env = unwrap_some!(TestEnvironment::setup(128, 128));

        // Our allocated buffer starts filled with zeros.  The log header should
        // consider this to be an invalid value.
        assert_false!(env.hdr().is_magic_valid());
        assert_eq!(0u32, env.hdr().magic);

        // Set the location of the persistent log ram.  This will attempt to recover
        // the (currently empty) log, and initialize the active log's header in the
        // process.
        env.set_location();
        assert_true!(env.hdr().is_magic_valid());
        assert_eq!(0u32, env.hdr().rd_ptr);

        // The recovered log should be empty.
        assert_true!(check_recovered_log_is_empty(&env.log));

        // Perform some writes to the log
        const TEST_STRINGS: [&[u8]; 4] =
            [b"Test pattern 1\n", b"This has no newline", b"ABCDEF0123456789\n", b"Foo Bar Baz\n"];

        let mut expected_rd_ptr = 0u32;
        for sv in TEST_STRINGS {
            env.log.write(sv);
            expected_rd_ptr += sv.len() as u32;
        }

        // The recovered log should be empty.
        assert_true!(check_recovered_log_is_empty(&env.log));

        // But the read pointer should have advanced to match the length of the
        // strings we have written so far.
        assert_eq!(expected_rd_ptr, env.hdr().rd_ptr);

        // Simulate a reboot by resetting our debug log, then setting the location of
        // our persistent buffer once again.  This will cause the implementation to
        // attempt to recover our log, at which point in time we can verify that
        // everything recovered correctly.
        env.log.force_reset();
        env.set_location();
        assert_true!(check_recovered_log_matches(&env.log, &TEST_STRINGS));
    }

    /// Tests persistent debuglog recovery when writes wrap around the ring buffer.
    #[test]
    fn pdlog_logwrap_test() {
        let mut env = unwrap_some!(TestEnvironment::setup(128, 128));
        env.set_location();

        // Perform enough writes to the log that it wraps at least once.
        const TEST_STR: &[u8] = b"0123456789AB\n";
        const REPEAT_COUNT: usize = 23;
        for _ in 0..REPEAT_COUNT {
            env.log.write(TEST_STR);
        }

        // "reboot" and recover the log.
        env.log.force_reset();
        env.set_location();

        // We have 120 bytes worth of payload, and our test string is 13 bytes long
        // and repeated 23 times. So, the total number of bytes we write into the
        // buffer is 299 meaning that we will wrap the buffer two complete times.  In
        // addition, our recovered log will start 120 % 13 == 3 bytes from the end of
        // an instance of our test string.
        // Update test vectors to match change in header size!
        zr::static_assert!(core::mem::size_of::<LogHeader>() == 8);
        zr::static_assert!(TEST_STR.len() == 13);
        let test_strings: [&[u8]; 10] = [
            &TEST_STR[TEST_STR.len() - 3..],
            TEST_STR,
            TEST_STR,
            TEST_STR,
            TEST_STR,
            TEST_STR,
            TEST_STR,
            TEST_STR,
            TEST_STR,
            TEST_STR,
        ];
        assert_true!(check_recovered_log_matches(&env.log, &test_strings));
    }

    // We expect that all embedded nulls get removed from strings when the log is
    // recovered.
    /// Tests that embedded null bytes are stripped when recovering the persistent debuglog.
    #[test]
    fn pdlog_zeros_removed_test() {
        let mut env = unwrap_some!(TestEnvironment::setup(128, 128));
        env.set_location();

        // Perform some writes to the log which have 0s embedded in them.
        const WITH_NULLS: [&[u8]; 2] = [b"This \0has\0nulls\n", b"\0\0even\0more\0nulls\0\0\0\n"];
        for sv in WITH_NULLS {
            env.log.write(sv);
        }

        // "reboot" and recover the log.
        env.log.force_reset();
        env.set_location();

        // Verify that the nulls are removed during recovery.
        const WITHOUT_NULLS: [&[u8]; 2] = [b"This hasnulls\n", b"evenmorenulls\n"];
        assert_true!(check_recovered_log_matches(&env.log, &WITHOUT_NULLS));
    }

    /// Tests that persistent debuglog recovery rejects a corrupted header magic value.
    #[test]
    fn pdlog_rejects_bad_magic_test() {
        // Set up a log, put some data into it, then "reboot"
        let mut env = unwrap_some!(TestEnvironment::setup(128, 128));
        env.set_location();
        env.log.write(b"I'm in your base, corrupting your magic numbers!\n");
        env.log.force_reset();

        // Before we attempt to recover the log, deliberately corrupt the magic
        // number.
        env.hdr_mut().magic = env.hdr().magic.wrapping_add(1);

        // Now attempt to recover the log, and verify that we get nothing.
        env.set_location();
        assert_true!(check_recovered_log_is_empty(&env.log));
    }

    /// Tests that persistent debuglog recovery rejects an out-of-bounds header read pointer.
    #[test]
    fn pdlog_rejects_bad_rd_ptr_test() {
        // Set up a log, put some data into it, then "reboot"
        let mut env = unwrap_some!(TestEnvironment::setup(128, 128));
        env.set_location();
        env.log.write(b"I'm in your base, corrupting your read pointer!\n");
        env.log.force_reset();

        // Before we attempt to recover the log, set the read pointer of the log to
        // something impossible.
        env.hdr_mut().rd_ptr = 0x12356;

        // Now attempt to recover the log, and verify that we get nothing.
        env.set_location();
        assert_true!(check_recovered_log_is_empty(&env.log));
    }
}
