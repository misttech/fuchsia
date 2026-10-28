// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Jump trace (`jtrace`) ring-buffer recorder, hooks, and console command.

mod jtrace_ffi;
mod jtrace_internal;
#[cfg(ktest)]
mod jtrace_tests;

use core::cell::UnsafeCell;
use core::ffi::{CStr, c_char, c_void};
use core::mem::{align_of, size_of};

pub use jtrace_bindings::{
    Field32, Field64, FileFuncLineInfo, IsPersistent, LargeEntry, SmallEntry, TraceBufferType,
    UseLargeEntries, UseMonoTimestamps,
};
use kprint::kformat;
use libc::stdio::FILE;
use zx_types::{zx_duration_t, zx_koid_t, zx_ticks_t, zx_time_t};

use crate::arch_rs::vm::is_kernel_address;
use crate::kernel::types::cpu_num_t;
use crate::platform_rs::timer::{zx_msec, zx_sec, zx_usec};
use crate::vm::vm_aspace::VmAspace;

use self::jtrace_internal::{AlignedBuffer, Config, Header, JTrace, TraceHooks, UNKNOWN_TID};

unsafe extern "C" {
    // Defined in //zircon/kernel/lib/io.
    #[allow(non_upper_case_globals)]
    static gStdoutUnbuffered: FILE;
}

const PERSISTENT_RAM_ALLOCATION_GRANULARITY: usize =
    zr::parse_usize(env!("PERSISTENT_RAM_ALLOCATION_GRANULARITY"))
        .expect("PERSISTENT_RAM_ALLOCATION_GRANULARITY invalid");

/// Target buffer size in bytes (`JTRACE_TARGET_BUFFER_SIZE`).
pub const TARGET_BUFFER_SIZE: usize =
    zr::parse_usize(env!("JTRACE_TARGET_BUFFER_SIZE")).expect("JTRACE_TARGET_BUFFER_SIZE invalid");

/// Per-CPU last-entry storage count (`JTRACE_LAST_ENTRY_STORAGE`).
pub const LAST_ENTRY_STORAGE: usize =
    zr::parse_usize(env!("JTRACE_LAST_ENTRY_STORAGE")).expect("JTRACE_LAST_ENTRY_STORAGE invalid");

/// Whether large entries are enabled (`JTRACE_USE_LARGE_ENTRIES`).
pub const USE_LARGE_ENTRIES: bool = cfg!(jtrace_use_large_entries);

/// Whether persistent trace buffers are enabled (`JTRACE_IS_PERSISTENT`).
pub const IS_PERSISTENT: bool = cfg!(jtrace_persistent);

/// Whether monotonic timestamps are used (`JTRACE_USE_MONO_TIMESTAMPS`).
pub const USE_MONO_TIMESTAMPS: bool = cfg!(jtrace_use_mono_timestamps);

/// Target persistent trace buffer size in bytes (`TARGET_BUFFER_SIZE` when `IS_PERSISTENT`, `0`
/// otherwise).
pub const TARGET_PERSISTENT_BUFFER_SIZE: usize = if IS_PERSISTENT { TARGET_BUFFER_SIZE } else { 0 };

zr::static_assert!(
    !IS_PERSISTENT || TARGET_BUFFER_SIZE.is_multiple_of(PERSISTENT_RAM_ALLOCATION_GRANULARITY)
);

const RECOVERY_BUFFER_SIZE: usize = TARGET_PERSISTENT_BUFFER_SIZE;
const NON_PERSISTENT_BUFFER_SIZE: usize = if IS_PERSISTENT { 0 } else { TARGET_BUFFER_SIZE };

zr::static_assert!(size_of::<TraceBufferType>() == 4);
zr::static_assert!(align_of::<TraceBufferType>() == 4);

zr::static_assert!(size_of::<FileFuncLineInfo>() == 24);
zr::static_assert!(align_of::<FileFuncLineInfo>() == 8);

/// Read-only view of the fields of a [`TraceEntry`].
#[derive(Clone, Copy)]
struct TraceEntryFields {
    /// Timestamp in ticks.
    pub ts_ticks: zx_ticks_t,
    /// Null-terminated tag string pointer.
    pub tag: *const c_char,
    /// Pointer to static file/function/line info (`null` for small entries).
    pub ffl_info: *const FileFuncLineInfo,
    /// Thread koid (`UNKNOWN_TID` for small entries).
    pub tid: zx_koid_t,
    /// CPU ID on which the entry was logged.
    pub cpu_id: cpu_num_t,
    /// First 32-bit payload field.
    pub a: u32,
    /// Second 32-bit payload field (`0` for small entries).
    pub b: u32,
    /// Third 32-bit payload field (`0` for small entries).
    pub c: u32,
    /// Fourth 32-bit payload field (`0` for small entries).
    pub d: u32,
    /// First 64-bit payload field (`0` for small entries).
    pub e: u64,
    /// Second 64-bit payload field (`0` for small entries).
    pub f: u64,
}

/// Mutable view of the header-populated fields of a [`TraceEntry`].
struct TraceEntryFieldsMut<'a> {
    /// Mutable reference to the entry's timestamp in ticks.
    pub ts_ticks: &'a mut zx_ticks_t,
    /// Mutable reference to the entry's CPU ID.
    pub cpu_id: &'a mut cpu_num_t,
    /// Optional mutable reference to the entry's thread koid (`None` for small entries).
    pub tid: Option<&'a mut zx_koid_t>,
}

impl TraceEntryFieldsMut<'_> {
    /// Sets the entry's thread koid if present.
    #[inline]
    pub fn set_tid(&mut self, tid: zx_koid_t) {
        if let Some(slot) = &mut self.tid {
            **slot = tid;
        }
    }
}

/// Common trait implemented by [`SmallEntry`] and [`LargeEntry`].
trait TraceEntry: Copy + Default + 'static {
    /// Whether this entry type is a large entry.
    const USE_LARGE_ENTRIES: UseLargeEntries;

    /// Returns a read-only view of all entry fields.
    fn fields(&self) -> TraceEntryFields;

    /// Returns mutable references to the runtime-populated entry fields.
    fn fields_mut(&mut self) -> TraceEntryFieldsMut<'_>;

    /// Dispatches this entry to the appropriate `TraceHooks::print_*_trace_entry` method.
    fn print_trace_entry<H: TraceHooks + ?Sized>(
        &self,
        hooks: &H,
        buf_type: TraceBufferType,
        ts: zx_time_t,
        delta: zx_duration_t,
    );

    /// Constructs a test entry with sequential field values starting at `val`.
    #[cfg(ktest)]
    fn new_test_entry(tag: &'static CStr, ffl_info: &'static FileFuncLineInfo, val: u32) -> Self;
}

zr::static_assert!(size_of::<SmallEntry>() == 24);
zr::static_assert!(align_of::<SmallEntry>() == 8);

impl TraceEntry for SmallEntry {
    const USE_LARGE_ENTRIES: UseLargeEntries = UseLargeEntries::No;

    #[inline]
    fn fields(&self) -> TraceEntryFields {
        TraceEntryFields {
            ts_ticks: self.ts_ticks,
            tag: self.tag,
            ffl_info: core::ptr::null(),
            tid: UNKNOWN_TID,
            cpu_id: self.cpu_id,
            a: self.a,
            b: 0,
            c: 0,
            d: 0,
            e: 0,
            f: 0,
        }
    }

    #[inline]
    fn fields_mut(&mut self) -> TraceEntryFieldsMut<'_> {
        TraceEntryFieldsMut { ts_ticks: &mut self.ts_ticks, cpu_id: &mut self.cpu_id, tid: None }
    }

    #[inline]
    fn print_trace_entry<H: TraceHooks + ?Sized>(
        &self,
        hooks: &H,
        buf_type: TraceBufferType,
        ts: zx_time_t,
        delta: zx_duration_t,
    ) {
        hooks.print_small_trace_entry(self, buf_type, ts, delta);
    }

    #[cfg(ktest)]
    #[inline]
    fn new_test_entry(tag: &'static CStr, ffl_info: &'static FileFuncLineInfo, val: u32) -> Self {
        Self::new(tag, ffl_info, val)
    }
}

zr::static_assert!(size_of::<LargeEntry>() == 72);
zr::static_assert!(align_of::<LargeEntry>() == 8);

impl TraceEntry for LargeEntry {
    const USE_LARGE_ENTRIES: UseLargeEntries = UseLargeEntries::Yes;

    #[inline]
    fn fields(&self) -> TraceEntryFields {
        TraceEntryFields {
            ts_ticks: self.ts_ticks,
            tag: self.tag,
            ffl_info: self.ffl_info,
            tid: self.tid,
            cpu_id: self.cpu_id,
            a: self.a,
            b: self.b,
            c: self.c,
            d: self.d,
            e: self.e,
            f: self.f,
        }
    }

    #[inline]
    fn fields_mut(&mut self) -> TraceEntryFieldsMut<'_> {
        TraceEntryFieldsMut {
            ts_ticks: &mut self.ts_ticks,
            cpu_id: &mut self.cpu_id,
            tid: Some(&mut self.tid),
        }
    }

    #[inline]
    fn print_trace_entry<H: TraceHooks + ?Sized>(
        &self,
        hooks: &H,
        buf_type: TraceBufferType,
        ts: zx_time_t,
        delta: zx_duration_t,
    ) {
        hooks.print_large_trace_entry(self, buf_type, ts, delta);
    }

    #[cfg(ktest)]
    #[inline]
    fn new_test_entry(tag: &'static CStr, ffl_info: &'static FileFuncLineInfo, val: u32) -> Self {
        Self::new(
            tag,
            ffl_info,
            val,
            val + 1,
            val + 2,
            val + 3,
            u64::from(val + 4),
            u64::from(val + 5),
        )
    }
}

/// Make an attempt to validate a virtual address as being a valid kernel virtual
/// address.  Do not allow this to be called when blocking is not allowed
/// (holding a spinlock, hard IRQ time, etc) as blocking mutexes in the VM
/// subsystem will need to be acquired in order to perform the validation.
fn validate_vaddr(addr: usize) -> bool {
    if !is_kernel_address(addr) {
        return false;
    }

    let aspace = VmAspace::kernel_aspace();
    let err = aspace.arch_aspace().query(addr);
    err.is_ok()
}

/// SafeString is a small helper class which does its best to validate that
/// string literal pointers recovered from a persistent trace buffer are valid
/// before attempting to render them.  Persistent trace buffers are stored in
/// "persistent" RAM passed to the kernel by the bootloader, and _could_ have
/// suffered from corruption during a spontaneous reboot, so it is important to
/// ensure that they represent a valid kernel virtual address before attempting
/// to render them.
enum SafeString {
    // Safety invariant: Points to a valid null-terminated C string for the lifetime of `Self`.
    Valid(*const c_char),
    Replaced([u8; 32]),
}

impl SafeString {
    /// Creates a `SafeString` from `str_ptr`, replacing null or unmapped recovered addresses.
    ///
    /// # Safety
    ///
    /// The caller must ensure that if `str_ptr` is non-null (and mapped in the kernel address
    /// space when `buf_type == TraceBufferType::Recovered`), `str_ptr` points to a valid
    /// null-terminated C string that remains valid for reads for the lifetime of the returned
    /// `SafeString` and any pointer obtained from [`SafeString::get`].
    unsafe fn new(str_ptr: *const c_char, buf_type: TraceBufferType) -> Self {
        // Only attempt to validate the string's virtual address if we are
        // attempting to print a recovered log.  If we are in the process of dumping
        // the current log, there is a very good chance that we are in the middle of
        // a panic and unable to validate virtual addresses due to the VM locking
        // requirements.
        //
        // Also check for nullptr while we are at it.  It is possible for a dumped
        // jtrace to have nullptr entries for last-per-cpu records in the case that
        // there were never any records for a given CPU.
        if str_ptr.is_null()
            || (buf_type == TraceBufferType::Recovered && !validate_vaddr(str_ptr.addr()))
        {
            let mut replacement_buf = [0u8; 32];
            let _ = kformat!(&mut replacement_buf, "<Invalid {:p}>", str_ptr);
            Self::Replaced(replacement_buf)
        } else {
            Self::Valid(str_ptr)
        }
    }

    #[inline]
    fn get(&self) -> *const c_char {
        match self {
            Self::Valid(ptr) => *ptr,
            Self::Replaced(buf) => buf.as_ptr().cast::<c_char>(),
        }
    }
}

/// Production [`TraceHooks`] implementation writing directly to unbuffered stdout.
struct ProductionTraceHooks;

impl ProductionTraceHooks {
    /// Renders a single trace entry into a 256-byte buffer and writes it directly
    /// to unbuffered stdout (`gStdoutUnbuffered.write`) instead of rendering into
    /// the current thread's linebuffer, which is too short to hold an entire
    /// large entry on a single line (and expanding every thread's linebuffer just
    /// for `jtrace` would waste memory across all threads).
    ///
    /// `jtrace` buffer dump operations deliberately do not acquire any internal
    /// lock to serialize concurrent dumps, by design, for four reasons:
    ///
    /// 1) Kernel stacks are small (8 KB by default), so we avoid keeping multiple
    ///    large formatting buffers live on the stack at once: header and footer
    ///    messages in `JTrace::dump_buf` use short-lived scoped buffers that are
    ///    dropped before `print_trace_entry` is called, ensuring only this single
    ///    256-byte `linebuf` is on the stack during entry formatting.
    /// 2) Dumping a `jtrace` buffer usually happens during a kernel panic, where
    ///    acquiring any lock must be avoided (for example, panicking during a
    ///    trace dump while holding a dump-serialization lock would deadlock the
    ///    panic dump).
    /// 3) Aside from a panic, the only other place a trace buffer is dumped in
    ///    production is from the kernel console (`cmd_jtrace`), where commands
    ///    are already serialized by the singleton console `CommandLock`.
    /// 4) Even if concurrent dumps ever occurred, `kformat!` formats into the
    ///    caller's `linebuf` and returns an explicit length-bounded `&[u8]` slice
    ///    passed directly to `FILE::write` without relying on NUL termination, so
    ///    at worst output lines could interleave without ever reading past the
    ///    end of `linebuf`.
    fn internal_print_trace_entry<E: TraceEntry>(
        &self,
        entry: &E,
        buf_type: TraceBufferType,
        ts: zx_time_t,
        delta: zx_duration_t,
    ) {
        let e = entry.fields();
        let ts_sec = ts / zx_sec(1);
        let ts_nsec = ts % zx_sec(1);

        // SAFETY: `e.tag` is either validated via `validate_vaddr` (when `Recovered`) or a
        // static `.rodata` string literal recorded during normal execution.
        let tag = unsafe { SafeString::new(e.tag, buf_type) };

        let delta_usec = delta / zx_usec(1);
        let delta_nsec = delta % zx_usec(1);

        let mut linebuf = [0u8; 256];

        if E::USE_LARGE_ENTRIES == UseLargeEntries::No {
            let formatted = kformat!(
                &mut linebuf,
                "[{:4d}.{:09d}][cpu {:u}] : {:08x} : ({:5d}.{:03d} uSec) : ({:cs})\n",
                ts_sec,
                ts_nsec,
                e.cpu_id,
                e.a,
                delta_usec,
                delta_nsec,
                tag.get()
            );
            self.print_info(formatted);
        } else {
            static FALLBACK: FileFuncLineInfo =
                FileFuncLineInfo::new(c"<bad FFL pointer>", c"<bad FFL pointer>", 0);

            let mut ffl_info = e.ffl_info;
            let ffl_addr = ffl_info.addr();
            let ffl_end = ffl_addr.wrapping_add(size_of::<FileFuncLineInfo>() - 1);
            if ffl_info.is_null()
                || !ffl_info.is_aligned()
                || (buf_type == TraceBufferType::Recovered
                    && (!validate_vaddr(ffl_addr) || !validate_vaddr(ffl_end)))
            {
                ffl_info = &raw const FALLBACK;
            }

            // SAFETY: `ffl_info` is non-null, aligned, and either `&FALLBACK` or a validated
            // kernel `.rodata` `FileFuncLineInfo` pointer.
            let ffl = unsafe { &*ffl_info };
            // SAFETY: `ffl.file` and `ffl.func` are validated when `Recovered` or point to
            // static `.rodata` string literals.
            let file = unsafe { SafeString::new(ffl.file, buf_type) };
            // SAFETY: See above.
            let func = unsafe { SafeString::new(ffl.func, buf_type) };
            // SAFETY: `file.get()` returns a valid null-terminated C string.
            let trimmed_file = unsafe { Self::trim_filename(file.get()) };

            let mut tid_buffer;
            let tid_str: &[u8] = if e.tid != UNKNOWN_TID {
                tid_buffer = [0u8; 16];
                kformat!(&mut tid_buffer, "{:8u}", e.tid)
            } else {
                b" Unknown"
            };

            let formatted = kformat!(
                &mut linebuf,
                concat!(
                    "[{:4d}.{:09d}][cpu {:u} tid {:s}] : {:08x} {:08x} {:08x} {:08x} {:016x} ",
                    "{:016x} : ({:8d}.{:03d} uSec) : {:cs}:{:cs}:{:d} ({:cs})\n",
                ),
                ts_sec,
                ts_nsec,
                e.cpu_id,
                tid_str,
                e.a,
                e.b,
                e.c,
                e.d,
                e.e,
                e.f,
                delta_usec,
                delta_nsec,
                trimmed_file,
                func.get(),
                ffl.line,
                tag.get()
            );
            self.print_info(formatted);
        }
    }

    /// Returns a pointer to the filename component after the last `'/'` in `fname`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `fname` points to a valid null-terminated C string for the
    /// duration of this call.
    unsafe fn trim_filename(fname: *const c_char) -> *const c_char {
        // SAFETY: Caller guarantees `fname` is a valid null-terminated C string.
        let bytes = unsafe { CStr::from_ptr(fname) }.to_bytes();
        bytes.iter().rposition(|&b| b == b'/').map_or(fname, |pos| {
            // SAFETY: `pos + 1 <= bytes.len()`, which lies within the same C string allocation.
            unsafe { fname.add(pos + 1) }
        })
    }
}

impl TraceHooks for ProductionTraceHooks {
    fn print_warning(&self, msg: &[u8]) {
        // SAFETY: `gStdoutUnbuffered` is a C++ global `FILE` that is never reassigned.
        unsafe {
            gStdoutUnbuffered.write(msg);
        }
    }

    fn print_info(&self, msg: &[u8]) {
        // SAFETY: `gStdoutUnbuffered` is a C++ global `FILE` that is never reassigned.
        unsafe {
            gStdoutUnbuffered.write(msg);
        }
    }

    #[sanitize(address = "off")]
    fn hexdump(&self, data: &[u8]) {
        let _ = pretty::hexdump8_very_ex_rs(
            &mut debug::dprintf::KernelConsoleWriter,
            data,
            data.as_ptr().addr() as u64,
        );
    }

    fn print_large_trace_entry(
        &self,
        e: &LargeEntry,
        buf_type: TraceBufferType,
        ts: zx_time_t,
        delta: zx_duration_t,
    ) {
        self.internal_print_trace_entry(e, buf_type, ts, delta);
    }

    fn print_small_trace_entry(
        &self,
        e: &SmallEntry,
        buf_type: TraceBufferType,
        ts: zx_time_t,
        delta: zx_duration_t,
    ) {
        self.internal_print_trace_entry(e, buf_type, ts, delta);
    }
}

/// Active trace entry type (`LargeEntry` or `SmallEntry`) selected by `jtrace_use_large_entries`.
#[cfg(jtrace_use_large_entries)]
pub type ProdEntry = LargeEntry;
/// Active trace entry type (`LargeEntry` or `SmallEntry`) selected by `jtrace_use_large_entries`.
#[cfg(not(jtrace_use_large_entries))]
pub type ProdEntry = SmallEntry;

const PROD_IS_PERSISTENT: i32 =
    if IS_PERSISTENT { IsPersistent::Yes as i32 } else { IsPersistent::No as i32 };
const PROD_USE_LARGE_ENTRIES: i32 =
    if USE_LARGE_ENTRIES { UseLargeEntries::Yes as i32 } else { UseLargeEntries::No as i32 };
const PROD_USE_MONO_TIMESTAMPS: i32 =
    if USE_MONO_TIMESTAMPS { UseMonoTimestamps::Yes as i32 } else { UseMonoTimestamps::No as i32 };

struct NonPersistentBuffer {
    data: UnsafeCell<AlignedBuffer<NON_PERSISTENT_BUFFER_SIZE>>,
}

// SAFETY: `NonPersistentBuffer` is handed to `G_TRACE.set_location` once during single-core
// early boot and thereafter managed via atomic operations on `Header` in `JTrace`.
unsafe impl Sync for NonPersistentBuffer {}

static NON_PERSISTENT_BUFFER: NonPersistentBuffer =
    NonPersistentBuffer { data: UnsafeCell::new(AlignedBuffer([0u8; NON_PERSISTENT_BUFFER_SIZE])) };

impl NonPersistentBuffer {
    fn get() -> (*mut u8, usize) {
        (NON_PERSISTENT_BUFFER.data.get().cast::<u8>(), NON_PERSISTENT_BUFFER_SIZE)
    }
}

type JTraceConfig = Config<
    ProdEntry,
    TARGET_BUFFER_SIZE,
    LAST_ENTRY_STORAGE,
    RECOVERY_BUFFER_SIZE,
    PROD_IS_PERSISTENT,
    PROD_USE_LARGE_ENTRIES,
    PROD_USE_MONO_TIMESTAMPS,
>;
type ProdJTrace = JTrace<JTraceConfig, ProductionTraceHooks>;

zr::static_assert!(ProdJTrace::REQUIRED_BUFFER_ALIGNMENT == align_of::<Header<JTraceConfig>>());

static G_TRACE: ProdJTrace =
    JTrace::new(ProductionTraceHooks, AlignedBuffer([0u8; RECOVERY_BUFFER_SIZE]));

// Thunks
//
/// Initializes the global `jtrace` subsystem during early boot.
pub fn jtrace_init() {
    if TARGET_BUFFER_SIZE == 0 {
        return;
    }
    // Note: jtrace_init is called very early on in boot, before global
    // constructors have been called. Do not add any behavior which depends on
    // global ctors at this point in the code.
    if !IS_PERSISTENT {
        // SAFETY: `jtrace_init` is called once during single-core early boot, and
        // `NonPersistentBuffer::get()` returns a static 8-byte aligned zero-initialized live
        // buffer.
        unsafe {
            G_TRACE.set_location(NonPersistentBuffer::get());
        }
    }
}

/// Notifies `jtrace` that `thread_init_early` has completed.
///
/// # Safety
///
/// The caller must ensure that early thread initialization is complete so that
/// [`ThreadPtr::current()`](crate::kernel::thread::ThreadPtr::current) is valid on the calling
/// CPU.
pub unsafe fn jtrace_set_after_thread_init_early() {
    if TARGET_BUFFER_SIZE == 0 {
        return;
    }
    // SAFETY: Caller guarantees early thread initialization is complete.
    unsafe {
        G_TRACE.set_after_thread_init_early();
    }
}

/// Sets the memory location of the global `jtrace` buffer.
///
/// # Safety
///
/// The caller must ensure that this function is not called concurrently with itself or
/// `jtrace_dump(TraceBufferType::Recovered)`. If `ptr` is non-null, properly aligned, and `len`
/// is large enough to hold at least one entry, the caller must ensure that `ptr` refers to a live
/// (not freed) object/allocation of `len` initialized (e.g. zero-initialized or previously
/// initialized persistent RAM) readable and writable bytes that remains valid for the lifetime of
/// the kernel.
pub unsafe fn jtrace_set_location(ptr: *mut c_void, len: usize) {
    if TARGET_BUFFER_SIZE == 0 {
        return;
    }
    // SAFETY: Caller upholds the buffer validity, initialization, and lifetime preconditions.
    unsafe {
        G_TRACE.set_location((ptr.cast::<u8>(), len));
    }
}

/// Invalidates the global `jtrace` header magic.
pub fn jtrace_invalidate() {
    if TARGET_BUFFER_SIZE == 0 {
        return;
    }
    G_TRACE.invalidate();
}

/// Records `e` in the global `jtrace` buffer.
///
/// # Safety
///
/// The caller must ensure that `e`'s pointer fields (`tag`, and for [`LargeEntry`] `ffl_info` and
/// its `file`/`func` pointers) are either null or point to valid `'static` null-terminated C
/// strings / [`FileFuncLineInfo`].
pub unsafe fn jtrace_log(e: &mut ProdEntry) {
    if TARGET_BUFFER_SIZE == 0 {
        return;
    }
    // SAFETY: Caller guarantees `e`'s pointer fields are null or valid `'static` C strings /
    // `FileFuncLineInfo`.
    unsafe {
        G_TRACE.log(e);
    }
}

/// Dumps the specified `jtrace` buffer.
pub fn jtrace_dump(which: TraceBufferType) {
    if TARGET_BUFFER_SIZE == 0 {
        return;
    }
    if which == TraceBufferType::Recovered {
        G_TRACE.dump_recovered();
    } else {
        G_TRACE.dump(zx_msec(50));
    }
}

#[cfg(console_enabled)]
mod console {
    use core::ffi::c_int;
    use kprint::kprintln;

    use super::{
        G_TRACE, IS_PERSISTENT, LAST_ENTRY_STORAGE, TARGET_BUFFER_SIZE, TraceBufferType,
        USE_LARGE_ENTRIES, jtrace_dump,
    };
    use crate::arch_rs::max_num_cpus;
    use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};

    /// CLI command for `jtrace`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `argc >= 1` and `argv` points to a valid array of at least
    /// `argc` [`CmdArgs`] elements, where each element's `arg_str` is a valid null-terminated C
    /// string for the duration of this call.
    unsafe extern "C" fn cmd_jtrace(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
        if argc <= 0 || argv.is_null() {
            return -1;
        }
        // SAFETY: `argc > 0` and `argv` is non-null; caller guarantees `argv` is valid for `argc`
        // elements.
        let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let program = argv[0].as_str();
        let usage = || -> c_int {
            kprintln!("usage: {:s} [-r|-i]", program);
            kprintln!(
                "  -r : dump the recovered trace buffer instead of the current trace buffer."
            );
            kprintln!("  -i : dump information about the current JTRACE configuration.");
            -1
        };

        match argc {
            1 => {
                jtrace_dump(TraceBufferType::Current);
            }
            2 => {
                if argv[1].as_str() == "-r" {
                    jtrace_dump(TraceBufferType::Recovered);
                } else if argv[1].as_str() == "-i" {
                    if TARGET_BUFFER_SIZE == 0 {
                        kprintln!("Debug tracing is not enabled in this build.");
                    } else {
                        let (loc_data, loc_size) = G_TRACE.get_location();
                        kprintln!("JTRACE configuration");
                        kprintln!("--------------------");
                        kprintln!("Requested Buffer Size  : {}", TARGET_BUFFER_SIZE);
                        kprintln!("Allocated Buffer Size  : {}", loc_size);
                        kprintln!("Allocated Buffer Loc   : {:p}", loc_data);
                        kprintln!("Per-CPU last entry cnt : {}", LAST_ENTRY_STORAGE);
                        kprintln!(
                            "Large entries          : {:s}",
                            if USE_LARGE_ENTRIES { "yes" } else { "no" }
                        );
                        kprintln!(
                            "Persistent             : {:s}",
                            if IS_PERSISTENT { "yes" } else { "no" }
                        );

                        if (LAST_ENTRY_STORAGE != 0)
                            && (LAST_ENTRY_STORAGE != max_num_cpus() as usize)
                        {
                            kprintln!(
                                concat!(
                                    "\nWarning! Configured per-cpu last entry count ({}) ",
                                    "does not match target's number of CPUs ({})",
                                ),
                                LAST_ENTRY_STORAGE,
                                max_num_cpus()
                            );
                        }
                    }
                } else {
                    return usage();
                }
            }
            _ => {
                return usage();
            }
        }

        0
    }

    static_command!(
        CMD_JTRACE,
        c"jtrace".as_ptr(),
        c"dump the current or recovered jtrace".as_ptr(),
        cmd_jtrace,
        CMD_AVAIL_NORMAL
    );
}
