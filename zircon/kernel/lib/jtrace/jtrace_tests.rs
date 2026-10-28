// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Unit tests for Jump Trace (`jtrace`).

use core::cell::{Cell, UnsafeCell};
use core::ffi::CStr;
use core::sync::atomic::Ordering;

use fbl::{make_ref_counted, ref_counted};
use kalloc::Box;
use unittest::{
    assert_eq, assert_gt, assert_le, assert_null, assert_true, expect_eq, expect_false, expect_ge,
    expect_lt, expect_nonnull, expect_null, expect_true, unwrap_some,
};
use zx_types::{ZX_TIME_INFINITE, zx_duration_t, zx_time_t};

use crate::arch_rs::{curr_cpu_num, max_num_cpus};
use crate::jtrace::jtrace_internal::{Config, Header, JTrace, JTraceConfig, TraceHooks};
use crate::jtrace::{
    Field32, Field64, FileFuncLineInfo, IsPersistent, LargeEntry, SmallEntry, TraceBufferType,
    TraceEntry, UseLargeEntries, UseMonoTimestamps,
};
use crate::kernel::mp::SMP_MAX_CPUS;
use crate::kernel::thread::AutoPreemptDisabler;
use crate::kernel::types::cpu_num_t;

// Macro used to provide the local `record_failure!` macro required by `unittest::expect_*!` and
// `unittest::assert_*!` inside helper methods and generic test functions (which
// `#[unittest::suite]` only injects into non-generic `#[test]` functions).
macro_rules! begin_test {
    ($all_ok:ident) => {
        #[allow(unused_mut)]
        let mut $all_ok = true;
        macro_rules! record_failure {
            () => {
                #[allow(unused_assignments)]
                {
                    $all_ok = false;
                }
            };
        }
    };
}

struct TestHooks {
    expected_first_id: Cell<usize>,
    expected_per_cpu_num: Cell<cpu_num_t>,
    expected_per_cpu_id: Cell<usize>,
    warning_count: Cell<usize>,
    info_count: Cell<usize>,
    hexdump_count: Cell<usize>,
    entry_count: Cell<usize>,
    validation_failed: Cell<bool>,
    per_cpu_dump_started: Cell<bool>,
}

impl TestHooks {
    fn reset(
        &self,
        expected_first_id: usize,
        expected_per_cpu_num: cpu_num_t,
        expected_per_cpu_id: usize,
    ) {
        self.expected_first_id.set(expected_first_id);
        self.expected_per_cpu_num.set(expected_per_cpu_num);
        self.expected_per_cpu_id.set(expected_per_cpu_id);
        self.warning_count.set(0);
        self.info_count.set(0);
        self.hexdump_count.set(0);
        self.entry_count.set(0);
        self.validation_failed.set(false);
        self.per_cpu_dump_started.set(false);
    }

    /// When a dump fails, we expect to see exactly one warning, and no info,
    /// hexdump, or entry print operations.  The dump should just print a warning
    /// explaining why it cannot dump and then exit.
    fn check_dump_failed(&self) -> bool {
        begin_test!(all_ok);
        expect_eq!(1usize, self.warning_count.get());
        expect_eq!(0usize, self.info_count.get());
        expect_eq!(0usize, self.hexdump_count.get());
        expect_eq!(0usize, self.entry_count.get());
        expect_false!(self.validation_failed.get());
        all_ok
    }

    /// When a dump succeeds, we should see no warnings or hexdumps, at least one
    /// info message (usually more) and the expected number of records dumped.
    fn check_dump_succeeded(&self, expected_entry_count: usize) -> bool {
        begin_test!(all_ok);

        // If we expected to dump per-cpu last entries, and the number of CPUs in
        // the test machine is not equal to SMP_MAX_CPUS, then we expect to see an
        // extra warning during our trace buffer dump telling us that the
        // configuration does not match the actual number of CPUs in the machine.
        let expected_per_cpu_num = self.expected_per_cpu_num.get();
        let smp_max = SMP_MAX_CPUS as cpu_num_t;
        let expected_warning_count: usize =
            if expected_per_cpu_num != smp_max && max_num_cpus() != smp_max { 1 } else { 0 };

        expect_eq!(expected_warning_count, self.warning_count.get());
        expect_ge!(self.info_count.get(), 1usize);
        expect_eq!(0usize, self.hexdump_count.get());
        expect_eq!(expected_entry_count, self.entry_count.get());
        expect_false!(self.validation_failed.get());

        if self.per_cpu_dump_started.get() {
            expect_lt!(expected_per_cpu_num, smp_max);
        }

        if expected_per_cpu_num < smp_max {
            expect_true!(self.per_cpu_dump_started.get());
        }

        all_ok
    }

    /// When we attempt to dump a corrupt recovered persistent buffer, we should
    /// see exactly two warning followed by a hexdump of the corrupted buffer.  The
    /// first warning tells the user the reason that the trace is corrupt, while
    /// the second is the message just before the hexdump.
    fn check_dump_corrupt(&self) -> bool {
        begin_test!(all_ok);
        expect_eq!(2usize, self.warning_count.get());
        expect_eq!(0usize, self.info_count.get());
        expect_eq!(1usize, self.hexdump_count.get());
        expect_eq!(0usize, self.entry_count.get());
        expect_false!(self.validation_failed.get());
        all_ok
    }

    /// When a persistent trace recovers no data (because the header was clean), we
    /// expect to see a single info message informing the user that the log was
    /// clean, and nothing else.
    fn check_nothing_recovered(&self) -> bool {
        begin_test!(all_ok);
        expect_eq!(0usize, self.warning_count.get());
        expect_eq!(1usize, self.info_count.get());
        expect_eq!(0usize, self.hexdump_count.get());
        expect_eq!(0usize, self.entry_count.get());
        expect_false!(self.validation_failed.get());
        all_ok
    }

    fn validate_entry_id<E: TraceEntry>(&self, e: &E, expected_id: usize) -> bool {
        begin_test!(all_ok);

        if self.per_cpu_dump_started.get() {
            if e.fields().tag.is_null() {
                if E::USE_LARGE_ENTRIES == UseLargeEntries::Yes {
                    expect_null!(e.fields().ffl_info);
                }
            } else {
                expect_eq!(self.expected_per_cpu_num.get(), e.fields().cpu_id);
                if E::USE_LARGE_ENTRIES == UseLargeEntries::Yes {
                    expect_nonnull!(e.fields().ffl_info);
                }
            }
        }

        if !self.per_cpu_dump_started.get()
            || (!e.fields().tag.is_null() && e.fields().cpu_id == self.expected_per_cpu_num.get())
        {
            assert_eq!(expected_id as u32, e.fields().a);
            if E::USE_LARGE_ENTRIES == UseLargeEntries::Yes {
                assert_eq!((expected_id + 1) as u32, e.fields().b);
                assert_eq!((expected_id + 2) as u32, e.fields().c);
                assert_eq!((expected_id + 3) as u32, e.fields().d);
                assert_eq!((expected_id + 4) as u64, e.fields().e);
                assert_eq!((expected_id + 5) as u64, e.fields().f);
            }
        }

        all_ok
    }

    fn internal_print_trace_entry<E: TraceEntry>(
        &self,
        e: &E,
        _buf_type: TraceBufferType,
        _ts: zx_time_t,
        _delta: zx_duration_t,
    ) {
        let expected_id = if self.per_cpu_dump_started.get() {
            self.expected_per_cpu_id.get()
        } else {
            let count = self.entry_count.get();
            self.entry_count.set(count + 1);
            self.expected_first_id.get() + count
        };
        let failed = !self.validate_entry_id(e, expected_id) || self.validation_failed.get();
        self.validation_failed.set(failed);
    }
}

impl TraceHooks for TestHooks {
    fn print_warning(&self, _msg: &[u8]) {
        self.warning_count.set(self.warning_count.get() + 1);
    }

    fn print_info(&self, _msg: &[u8]) {
        self.info_count.set(self.info_count.get() + 1);
    }

    fn hexdump(&self, _data: &[u8]) {
        self.hexdump_count.set(self.hexdump_count.get() + 1);
    }

    fn per_cpu_dump_started(&self) {
        self.per_cpu_dump_started.set(true);
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

struct TestState<C: JTraceConfig> {
    trace: JTrace<C, TestHooks>,
    trace_storage: UnsafeCell<C::StorageBuffer>,
    recovery_template: UnsafeCell<C::StorageBuffer>,
}

impl<C: JTraceConfig> TestState<C> {
    const EXPECTED_ENTRIES: usize = (C::TARGET_BUFFER_SIZE - core::mem::size_of::<Header<C>>())
        / core::mem::size_of::<C::Entry>();

    fn new() -> Option<Box<Self>> {
        let uninit = Box::<Self>::try_new_zeroed().ok()?;
        // SAFETY: All fields of `TestState<C>` (atomics, `Cell`s, and `UnsafeCell` byte arrays)
        // are valid when zero-initialized.
        let state = unsafe { uninit.assume_init() };
        state.trace.hooks.expected_per_cpu_num.set(SMP_MAX_CPUS as cpu_num_t);
        Some(state)
    }

    #[inline]
    fn storage_ptr(&self) -> *mut u8 {
        self.trace_storage.get().cast::<u8>()
    }

    fn reset_trace(&mut self) {
        // Reset our JTrace instance state in-place.
        self.trace.reset();
    }

    fn save_template(&self) {
        const {
            assert!(core::mem::size_of::<C::StorageBuffer>() == C::TARGET_BUFFER_SIZE);
        }
        // SAFETY: `trace_storage` and `recovery_template` are distinct `UnsafeCell` allocations
        // of `C::TARGET_BUFFER_SIZE` initialized bytes, and tests are single-threaded.
        unsafe {
            core::ptr::copy_nonoverlapping(
                self.trace_storage.get().cast::<u8>().cast_const(),
                self.recovery_template.get().cast::<u8>(),
                C::TARGET_BUFFER_SIZE,
            );
        }
    }

    fn restore_template(&self) {
        const {
            assert!(core::mem::size_of::<C::StorageBuffer>() == C::TARGET_BUFFER_SIZE);
        }
        // SAFETY: `trace_storage` and `recovery_template` are distinct `UnsafeCell` allocations
        // of `C::TARGET_BUFFER_SIZE` initialized bytes, and tests are single-threaded.
        unsafe {
            core::ptr::copy_nonoverlapping(
                self.recovery_template.get().cast::<u8>().cast_const(),
                self.trace_storage.get().cast::<u8>(),
                C::TARGET_BUFFER_SIZE,
            );
        }
    }

    fn set_header_magic(&self, val: u32) {
        const {
            assert!(core::mem::size_of::<C::StorageBuffer>() >= core::mem::size_of::<Header<C>>());
            assert!(
                core::mem::align_of::<C::StorageBuffer>() >= core::mem::align_of::<Header<C>>()
            );
        }
        let ptr = self.trace_storage.get().cast::<Header<C>>();
        // SAFETY: `trace_storage` was zero-initialized in `TestState::new` and meets the size
        // and alignment requirements of `Header<C>`.
        unsafe {
            (*ptr).magic.store(val, Ordering::Relaxed);
        }
    }

    fn set_header_wr(&self, val: u32) {
        const {
            assert!(core::mem::size_of::<C::StorageBuffer>() >= core::mem::size_of::<Header<C>>());
            assert!(
                core::mem::align_of::<C::StorageBuffer>() >= core::mem::align_of::<Header<C>>()
            );
        }
        let ptr = self.trace_storage.get().cast::<Header<C>>();
        // SAFETY: `trace_storage` was zero-initialized in `TestState::new` and meets the size
        // and alignment requirements of `Header<C>`.
        unsafe {
            (*ptr).wr.store(val, Ordering::Relaxed);
        }
    }
}

static TEST_ENTRY_FFL_INFO: FileFuncLineInfo =
    FileFuncLineInfo::new(c"jtrace_tests.rs", c"<no function>", line!() as i32);

const NO_PERSIST: i32 = IsPersistent::No as i32;
const PERSIST: i32 = IsPersistent::Yes as i32;
const SMALL: i32 = UseLargeEntries::No as i32;
const LARGE: i32 = UseLargeEntries::Yes as i32;
const BOOT_TS: i32 = UseMonoTimestamps::No as i32;

type CfgLargeEntries = Config<LargeEntry, 1024, 0, 0, NO_PERSIST, LARGE, BOOT_TS>;
type CfgSmallEntries = Config<SmallEntry, 1024, 0, 0, NO_PERSIST, SMALL, BOOT_TS>;
type CfgPersistLargeEntries = Config<LargeEntry, 1024, 0, 1024, PERSIST, LARGE, BOOT_TS>;
type CfgPersistSmallEntries = Config<SmallEntry, 1024, 0, 1024, PERSIST, SMALL, BOOT_TS>;

trait TestConfig: JTraceConfig {
    type PerCpu: JTraceConfig;
}

impl TestConfig for CfgLargeEntries {
    type PerCpu = Config<LargeEntry, 4096, SMP_MAX_CPUS, 0, NO_PERSIST, LARGE, BOOT_TS>;
}
impl TestConfig for CfgSmallEntries {
    type PerCpu = Config<SmallEntry, 4096, SMP_MAX_CPUS, 0, NO_PERSIST, SMALL, BOOT_TS>;
}
impl TestConfig for CfgPersistLargeEntries {
    type PerCpu = Config<LargeEntry, 4096, SMP_MAX_CPUS, 4096, PERSIST, LARGE, BOOT_TS>;
}
impl TestConfig for CfgPersistSmallEntries {
    type PerCpu = Config<SmallEntry, 4096, SMP_MAX_CPUS, 4096, PERSIST, SMALL, BOOT_TS>;
}

// Introduce a local `jtrace!` macro with a couple of small tweaks to allow us
// to target the trace instance under test, and which generates a simple value
// pattern that we use when verifying the trace dump.
macro_rules! jtrace {
    ($tgt:expr, $tag:expr, $val:expr) => {{
        let mut entry = TraceEntry::new_test_entry($tag, &TEST_ENTRY_FFL_INFO, $val);
        // SAFETY: `new_test_entry` populates `tag` and `ffl_info` from valid `'static` C strings
        // and `TEST_ENTRY_FFL_INFO`.
        unsafe {
            ($tgt).log(&mut entry);
        }
    }};
}

/// Helper constants and macros that construct instances of large and small
/// entries with call-site file/function/line metadata, used in `Tests::entries`.
const EXPECTED_MAKE_ENTRY_FUNCTION: &CStr = c"make_entry_function()";
const EXPECTED_MAKE_ENTRY_FILE: &CStr = c"jtrace_tests.rs";

/// Macro used to construct a `SmallEntry` with a call-site `static FFL_INFO` capturing caller
/// `line!()`.
macro_rules! small_entry {
    ($tag:expr) => {{
        static FFL_INFO: FileFuncLineInfo = FileFuncLineInfo::new(
            EXPECTED_MAKE_ENTRY_FILE,
            EXPECTED_MAKE_ENTRY_FUNCTION,
            line!() as i32,
        );
        SmallEntry::new($tag, &FFL_INFO, 0u32)
    }};
    ($tag:expr, $a:expr) => {{
        static FFL_INFO: FileFuncLineInfo = FileFuncLineInfo::new(
            EXPECTED_MAKE_ENTRY_FILE,
            EXPECTED_MAKE_ENTRY_FUNCTION,
            line!() as i32,
        );
        SmallEntry::new($tag, &FFL_INFO, $a)
    }};
}

// Macro used to construct a `LargeEntry` with a call-site `static FFL_INFO` capturing caller
// `line!()`.
macro_rules! large_entry {
    (@at_line $tag:expr, $line:expr) => {{
        static FFL_INFO: FileFuncLineInfo =
            FileFuncLineInfo::new(EXPECTED_MAKE_ENTRY_FILE, EXPECTED_MAKE_ENTRY_FUNCTION, $line);
        LargeEntry::new($tag, &FFL_INFO, 0u32, 0u32, 0u32, 0u32, 0u64, 0u64)
    }};
    ($tag:expr) => {{
        static FFL_INFO: FileFuncLineInfo = FileFuncLineInfo::new(
            EXPECTED_MAKE_ENTRY_FILE,
            EXPECTED_MAKE_ENTRY_FUNCTION,
            line!() as i32,
        );
        LargeEntry::new($tag, &FFL_INFO, 0u32, 0u32, 0u32, 0u32, 0u64, 0u64)
    }};
    ($tag:expr, $a:expr) => {{
        static FFL_INFO: FileFuncLineInfo = FileFuncLineInfo::new(
            EXPECTED_MAKE_ENTRY_FILE,
            EXPECTED_MAKE_ENTRY_FUNCTION,
            line!() as i32,
        );
        LargeEntry::new($tag, &FFL_INFO, $a, 0u32, 0u32, 0u32, 0u64, 0u64)
    }};
    ($tag:expr, $a:expr, $b:expr, $c:expr, $d:expr, $e:expr) => {{
        static FFL_INFO: FileFuncLineInfo = FileFuncLineInfo::new(
            EXPECTED_MAKE_ENTRY_FILE,
            EXPECTED_MAKE_ENTRY_FUNCTION,
            line!() as i32,
        );
        LargeEntry::new($tag, &FFL_INFO, $a, $b, $c, $d, $e, 0u64)
    }};
}

// Macro used to implement `From<Enum*>` for `Field32` and `Field64` across the 9 local test
// enum types in `Tests::entries`.
macro_rules! impl_test_enum_from {
    (32: $($t32:ty),* $(,)?; 64: $($t64:ty),* $(,)?) => {
        $(
            impl From<$t32> for Field32 {
                fn from(v: $t32) -> Self {
                    Field32::new(v as u32)
                }
            }
        )*
        $(
            impl From<$t64> for Field64 {
                fn from(v: $t64) -> Self {
                    Field64::new(v as u64)
                }
            }
        )*
    };
}

struct Tests;

impl Tests {
    #[allow(unused_assignments, clippy::disallowed_names)]
    fn entries() -> bool {
        begin_test!(all_ok);

        #[repr(i32)]
        #[derive(Clone, Copy)]
        enum EnumDefault {
            Val = 1,
        }
        #[repr(u8)]
        #[derive(Clone, Copy)]
        enum EnumU8 {
            Val = 1,
        }
        #[repr(u16)]
        #[derive(Clone, Copy)]
        enum EnumU16 {
            Val = 1,
        }
        #[repr(u32)]
        #[derive(Clone, Copy)]
        enum EnumU32 {
            Val = 1,
        }
        #[repr(u64)]
        #[derive(Clone, Copy)]
        enum EnumU64 {
            Val = 1,
        }
        #[repr(i8)]
        #[derive(Clone, Copy)]
        enum EnumI8 {
            Val = 1,
        }
        #[repr(i16)]
        #[derive(Clone, Copy)]
        enum EnumI16 {
            Val = 1,
        }
        #[repr(i32)]
        #[derive(Clone, Copy)]
        enum EnumI32 {
            Val = 1,
        }
        #[repr(i64)]
        #[derive(Clone, Copy)]
        enum EnumI64 {
            Val = 1,
        }

        impl_test_enum_from!(
            32: EnumDefault, EnumU8, EnumU16, EnumU32, EnumI8, EnumI16, EnumI32;
            64: EnumDefault, EnumU8, EnumU16, EnumU32, EnumU64, EnumI8, EnumI16, EnumI32, EnumI64
        );

        // The Entry<> struct defined by the JTrace subsystem is supposed to make it
        // easy to log either 32 or 64 bit arguments without needing to do a bunch
        // of explicit casting.  This compile time test makes sure that this is true
        // by exercising a number of different cases for both the large and small
        // entry types.
        let mut _small = SmallEntry::default();

        // Default
        _small = small_entry!(c"Test tag");

        // Numbers
        _small = small_entry!(c"Test tag", 1u8);
        _small = small_entry!(c"Test tag", 1u16);
        _small = small_entry!(c"Test tag", 1u32);
        _small = small_entry!(c"Test tag", '\x01');
        _small = small_entry!(c"Test tag", 1i8);
        _small = small_entry!(c"Test tag", 1i16);
        _small = small_entry!(c"Test tag", 1i32);

        // class enums
        _small = small_entry!(c"Test tag", EnumDefault::Val);
        _small = small_entry!(c"Test tag", EnumU8::Val);
        _small = small_entry!(c"Test tag", EnumU16::Val);
        _small = small_entry!(c"Test tag", EnumU32::Val);
        _small = small_entry!(c"Test tag", EnumI8::Val);
        _small = small_entry!(c"Test tag", EnumI16::Val);
        _small = small_entry!(c"Test tag", EnumI32::Val);

        let mut large = LargeEntry::default();

        // Default
        large = large_entry!(c"Test tag");

        // Numbers (32-bit fields)
        large = large_entry!(c"Test tag", 1u8);
        large = large_entry!(c"Test tag", 1u16);
        large = large_entry!(c"Test tag", 1u32);
        large = large_entry!(c"Test tag", '\x01');
        large = large_entry!(c"Test tag", 1i8);
        large = large_entry!(c"Test tag", 1i16);
        large = large_entry!(c"Test tag", 1i32);

        // class enums (32-bit fields)
        large = large_entry!(c"Test tag", EnumDefault::Val);
        large = large_entry!(c"Test tag", EnumU8::Val);
        large = large_entry!(c"Test tag", EnumU16::Val);
        large = large_entry!(c"Test tag", EnumU32::Val);
        large = large_entry!(c"Test tag", EnumI8::Val);
        large = large_entry!(c"Test tag", EnumI16::Val);
        large = large_entry!(c"Test tag", EnumI32::Val);

        // Numbers (64-bit fields)
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, 1u8);
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, 1u16);
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, 1u32);
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, 1u64);
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, '\x01');
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, 1i8);
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, 1i16);
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, 1i32);
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, 1i64);

        // class enums (64-bit fields)
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, EnumDefault::Val);
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, EnumU8::Val);
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, EnumU16::Val);
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, EnumU32::Val);
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, EnumU64::Val);
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, EnumI8::Val);
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, EnumI16::Val);
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, EnumI32::Val);
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, EnumI64::Val);

        // Pointers
        /// Local struct used for pointer-field test cases.
        struct Foo {
            _val: u32,
        }
        #[ref_counted]
        #[derive(fbl::Recyclable)]
        #[repr(C)]
        struct Bar {
            _val: u32,
        }

        let foo = unwrap_some!(Box::try_new(Foo { _val: 0 }).ok());
        let const_foo = unwrap_some!(Box::try_new(Foo { _val: 0 }).ok());
        let bar = unwrap_some!(make_ref_counted!(Bar { _val: 0 }).ok());
        let const_bar = unwrap_some!(make_ref_counted!(Bar { _val: 0 }).ok());

        large = large_entry!(
            c"Test tag",
            0u32,
            0u32,
            0u32,
            0u32,
            core::ptr::without_provenance_mut::<u32>(1)
        );
        large = large_entry!(
            c"Test tag",
            0u32,
            0u32,
            0u32,
            0u32,
            core::ptr::without_provenance::<u32>(1)
        );
        large = large_entry!(
            c"Test tag",
            0u32,
            0u32,
            0u32,
            0u32,
            core::ptr::without_provenance_mut::<Foo>(1)
        );
        large = large_entry!(
            c"Test tag",
            0u32,
            0u32,
            0u32,
            0u32,
            core::ptr::without_provenance::<Foo>(1)
        );
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, &foo);
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, &const_foo);
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, &bar);
        large = large_entry!(c"Test tag", 0u32, 0u32, 0u32, 0u32, &const_bar);
        let _ = large;

        // Make sure that the file/function/line info recorded in a LARGE_ENTRY
        // makes sense.
        let expected_line_number = (line!() + 1) as i32;
        large = large_entry!(@at_line c"Test tag", line!() as i32);
        // SAFETY: `large.ffl_info` points to static `FFL_INFO`.
        let ffl = unsafe { &*large.ffl_info };
        expect_eq!(EXPECTED_MAKE_ENTRY_FILE.as_ptr(), ffl.file);
        expect_eq!(EXPECTED_MAKE_ENTRY_FUNCTION.as_ptr(), ffl.func);
        expect_eq!(expected_line_number, ffl.line);

        all_ok
    }

    fn basic<C: JTraceConfig>() -> bool {
        begin_test!(all_ok);

        // Create our trace instance.
        let state = unwrap_some!(TestState::<C>::new());

        // Make sure that the trace buffer is large enough to run the tests below.

        const {
            assert!(
                C::TARGET_BUFFER_SIZE
                    > (2 * core::mem::size_of::<Header<C>>()) + core::mem::size_of::<C::Entry>()
            );
        }

        // We have not set the location of our trace storage yet, so the internal
        // header and entry count fields should still be nullptr/0.
        assert_null!(state.trace.hdr());
        assert_eq!(0u32, state.trace.entry_cnt());

        // Attempts to set a null storage location with a non-zero length should be
        // silently rejected.
        {
            let null: *mut u8 = core::ptr::null_mut();
            // SAFETY: `set_location` rejects null pointers without dereferencing them.
            unsafe {
                state.trace.set_location((null, C::TARGET_BUFFER_SIZE));
            }
            assert_null!(state.trace.hdr());
            assert_eq!(0u32, state.trace.entry_cnt());
        }

        let storage_ptr = state.storage_ptr();
        let hdr_size = core::mem::size_of::<Header<C>>();

        // Attempts to set storage which is too small to hold a header should be
        // silently rejected.
        // SAFETY: `storage_ptr` points to `trace_storage`.
        unsafe {
            state.trace.set_location((storage_ptr, 1));
        }
        assert_null!(state.trace.hdr());
        assert_eq!(0u32, state.trace.entry_cnt());

        // Attempts to set storage which can hold a header, but is too small to hold
        // a single entry should be silently rejected.
        // SAFETY: `storage_ptr` points to `trace_storage`.
        unsafe {
            state.trace.set_location((storage_ptr, hdr_size + 1));
        }
        assert_null!(state.trace.hdr());
        assert_eq!(0u32, state.trace.entry_cnt());

        // Attempts to set storage which not aligned to a trace header should be
        // silently rejected.
        // SAFETY: `set_location` rejects unaligned pointers without dereferencing them.
        unsafe {
            state.trace.set_location((storage_ptr.wrapping_add(1), C::TARGET_BUFFER_SIZE - 1));
        }
        assert_null!(state.trace.hdr());
        assert_eq!(0u32, state.trace.entry_cnt());

        // We do not have any trace storage configured yet, but attempts to log a
        // new trace entry should still not crash the system.
        jtrace!(state.trace, c"Test tag", 1);
        state.trace.hooks.reset(0, SMP_MAX_CPUS as cpu_num_t, 0);
        state.trace.dump(ZX_TIME_INFINITE);
        assert_true!(state.trace.hooks.check_dump_failed());

        // Now actually set the location of the trace storage, and check that the set stuck.
        // SAFETY: `storage_ptr` is 8-byte aligned and `TARGET_BUFFER_SIZE` bytes long.
        unsafe {
            state.trace.set_location((storage_ptr, C::TARGET_BUFFER_SIZE));
        }
        assert_eq!(storage_ptr.cast::<Header<C>>(), state.trace.hdr());
        assert_eq!(TestState::<C>::EXPECTED_ENTRIES as u32, state.trace.entry_cnt());

        // Try to reset the location of our trace storage again, with another legal
        // location.  This should be silently ignored.
        // SAFETY: `set_location` ignores subsequent calls once a location is set.
        unsafe {
            state.trace.set_location((
                storage_ptr.wrapping_add(hdr_size),
                C::TARGET_BUFFER_SIZE - hdr_size,
            ));
        }
        assert_eq!(storage_ptr.cast::<Header<C>>(), state.trace.hdr());
        assert_eq!(TestState::<C>::EXPECTED_ENTRIES as u32, state.trace.entry_cnt());

        // Now that storage is configured, log some trace entries, and verify that
        // they show up when we dump the trace.
        const EXPECTED_ENTRY_COUNT: u32 = 10;
        assert_le!(EXPECTED_ENTRY_COUNT, state.trace.entry_cnt());
        for i in 0..EXPECTED_ENTRY_COUNT {
            jtrace!(state.trace, c"Test tag", i + 1);
        }
        state.trace.hooks.reset(1, SMP_MAX_CPUS as cpu_num_t, 0);
        state.trace.dump(ZX_TIME_INFINITE);
        assert_true!(state.trace.hooks.check_dump_succeeded(EXPECTED_ENTRY_COUNT as usize));

        all_ok
    }

    fn wrapping<C: JTraceConfig>() -> bool {
        begin_test!(all_ok);

        // Create our trace instance and configure its storage.
        let state = unwrap_some!(TestState::<C>::new());
        let storage_ptr = state.storage_ptr();
        // SAFETY: `storage_ptr` is 8-byte aligned and `TARGET_BUFFER_SIZE` bytes long.
        unsafe {
            state.trace.set_location((storage_ptr, C::TARGET_BUFFER_SIZE));
        }

        // Fill the storage up with log entries, but do not cause it to wrap just yet.
        let expected_entries = TestState::<C>::EXPECTED_ENTRIES;
        let entry_cnt = state.trace.entry_cnt();
        assert_eq!(expected_entries as u32, entry_cnt);
        let mut id: u32 = 0;
        for _ in 0..entry_cnt {
            id += 1;
            jtrace!(state.trace, c"Test tag", id);
        }

        // Verify that the entries we wrote are in the trace when we dump it.  The
        // entry IDs in the trace should currently be on the range
        // [1, entry_cnt]
        state.trace.hooks.reset(1, SMP_MAX_CPUS as cpu_num_t, 0);
        state.trace.dump(ZX_TIME_INFINITE);
        assert_true!(state.trace.hooks.check_dump_succeeded(expected_entries));

        // Now wrap the trace, overwriting all but one of the original entries.
        assert_gt!(entry_cnt, 1u32);
        for _ in 0..(entry_cnt - 1) {
            id += 1;
            jtrace!(state.trace, c"Test tag", id);
        }

        // Verify that the entries we wrote are in the trace when we dump it.  The
        // entry IDs in the trace should currently be on the range
        // [entry_cnt, (2 * entry_cnt) - 1]
        state.trace.hooks.reset(entry_cnt as usize, SMP_MAX_CPUS as cpu_num_t, 0);
        state.trace.dump(ZX_TIME_INFINITE);
        assert_true!(state.trace.hooks.check_dump_succeeded(expected_entries));

        all_ok
    }

    fn recovery<C: JTraceConfig>() -> bool {
        begin_test!(all_ok);

        // Create our trace instance and configure its storage.
        let mut state = unwrap_some!(TestState::<C>::new());
        let storage_ptr = state.storage_ptr();
        // SAFETY: `storage_ptr` is 8-byte aligned and `TARGET_BUFFER_SIZE` bytes long.
        unsafe {
            state.trace.set_location((storage_ptr, C::TARGET_BUFFER_SIZE));
        }
        let expected_entries_cap = TestState::<C>::EXPECTED_ENTRIES;
        assert_eq!(expected_entries_cap as u32, state.trace.entry_cnt());

        // Attempt to dump the recovered log.  For non-persistent configurations,
        // this should simply print a warning and exit.  For persistent configs,
        // this should print a single informational message indicating that the log
        // was "clean" (the magic number was zero) and therefore there was nothing
        // to dump.
        state.trace.hooks.reset(0, SMP_MAX_CPUS as cpu_num_t, 0);
        state.trace.dump_recovered();
        if C::IS_PERSISTENT == IsPersistent::No {
            assert_true!(state.trace.hooks.check_dump_failed());
            return all_ok;
        } else {
            assert_true!(state.trace.hooks.check_nothing_recovered());
        }

        // Add some entries to the trace, then make a copy of the storage.  We will
        // use this image of a valid trace state as our template for the next parts
        // of the recovery test.
        const GENERATED_ENTRY_COUNT: usize = 1000;
        for i in 0..GENERATED_ENTRY_COUNT {
            jtrace!(state.trace, c"Test tag", (i + 1) as u32);
        }
        state.save_template();

        // The "recovered" trace should still contain nothing.
        state.trace.hooks.reset(0, SMP_MAX_CPUS as cpu_num_t, 0);
        state.trace.dump_recovered();
        assert_true!(state.trace.hooks.check_nothing_recovered());

        // Reset our trace instance, then assign the our storage back to the new
        // instance.  We should successfully recover the trace from the "previous
        // boot" and be able to dump the entries.
        let expected_entries = core::cmp::min(expected_entries_cap, GENERATED_ENTRY_COUNT);
        let expected_first_id = 1 + GENERATED_ENTRY_COUNT.saturating_sub(expected_entries_cap);

        state.reset_trace();
        let storage_ptr = state.storage_ptr();
        // SAFETY: `storage_ptr` is 8-byte aligned and `TARGET_BUFFER_SIZE` bytes long.
        unsafe {
            state.trace.set_location((storage_ptr, C::TARGET_BUFFER_SIZE));
        }
        state.trace.hooks.reset(expected_first_id, SMP_MAX_CPUS as cpu_num_t, 0);
        state.trace.dump_recovered();
        assert_true!(state.trace.hooks.check_dump_succeeded(expected_entries));

        // Reset our trace instance, and corrupt the magic number before assigning
        // our storage and recovering the trace.  We should receive a warning that
        // the header was corrupted along with a hexdump of the trace buffer when we
        // attempt to dump the recovered trace.
        state.reset_trace();
        state.set_header_magic(0x1234_5678);
        let storage_ptr = state.storage_ptr();
        // SAFETY: `storage_ptr` is 8-byte aligned and `TARGET_BUFFER_SIZE` bytes long.
        unsafe {
            state.trace.set_location((storage_ptr, C::TARGET_BUFFER_SIZE));
        }
        state.trace.hooks.reset(0, SMP_MAX_CPUS as cpu_num_t, 0);
        state.trace.dump_recovered();
        assert_true!(state.trace.hooks.check_dump_corrupt());

        // Reset our trace instance, restore our template, then corrupt the write
        // pointer.  Verify that the recovered trace is reported as corrupt when we
        // attempt to dump it.
        state.reset_trace();
        state.restore_template();
        state.set_header_wr((expected_entries_cap + 10) as u32);
        let storage_ptr = state.storage_ptr();
        // SAFETY: `storage_ptr` is 8-byte aligned and `TARGET_BUFFER_SIZE` bytes long.
        unsafe {
            state.trace.set_location((storage_ptr, C::TARGET_BUFFER_SIZE));
        }
        state.trace.hooks.reset(0, SMP_MAX_CPUS as cpu_num_t, 0);
        state.trace.dump_recovered();
        assert_true!(state.trace.hooks.check_dump_corrupt());

        all_ok
    }

    fn per_cpu_last_entries<BaseConfig: TestConfig>() -> bool {
        begin_test!(all_ok);

        // Define a version of our config with per-cpu last entries enabled.  Set
        // the storage to be equal to the maximum number of SMP CPUs currently
        // supported.
        type Cfg<B> = <B as TestConfig>::PerCpu;

        // Create our trace instance and configure its storage.
        let state = unwrap_some!(TestState::<Cfg<BaseConfig>>::new());
        let storage_ptr = state.storage_ptr();
        // SAFETY: `storage_ptr` is 8-byte aligned and `TARGET_BUFFER_SIZE` bytes long, and unit
        // tests run after early thread initialization is complete.
        unsafe {
            state.trace.set_location((storage_ptr, Cfg::<BaseConfig>::TARGET_BUFFER_SIZE));
            state.trace.set_after_thread_init_early();
        }

        // Turn off preemption so that we cannot migrate to a new CPU, then create a few trace
        // entries. Make sure that we take note of the CPU we were running on when we made the
        // entries.
        let mut id: u32 = 0;
        let expected_cpu: cpu_num_t;
        {
            let _preempt_disable = AutoPreemptDisabler::new();
            expected_cpu = curr_cpu_num();
            id += 1;
            jtrace!(state.trace, c"Test tag", id);
            id += 1;
            jtrace!(state.trace, c"Test tag", id);
            id += 1;
            jtrace!(state.trace, c"Test tag", id);
        }

        // Set up our hooks to know which CPU should have created the per_cpu last entry;
        state.trace.hooks.reset(1, expected_cpu, id as usize);
        state.trace.dump(ZX_TIME_INFINITE);
        assert_true!(state.trace.hooks.check_dump_succeeded(3));

        all_ok
    }
}

/// Debug trace tests
#[unittest::suite(name = "jtrace")]
mod jtrace_tests {
    use unittest::assert_true;

    use super::{
        CfgLargeEntries, CfgPersistLargeEntries, CfgPersistSmallEntries, CfgSmallEntries, Tests,
    };

    /// entries
    #[test]
    fn entries() {
        assert_true!(Tests::entries());
    }

    /// basic (large entries, no persist)
    #[test]
    fn basic_large_entries_no_persist() {
        assert_true!(Tests::basic::<CfgLargeEntries>());
    }

    /// basic (small entries, no persist)
    #[test]
    fn basic_small_entries_no_persist() {
        assert_true!(Tests::basic::<CfgSmallEntries>());
    }

    /// basic (large entries, persist)
    #[test]
    fn basic_large_entries_persist() {
        assert_true!(Tests::basic::<CfgPersistLargeEntries>());
    }

    /// basic (small entries, persist)
    #[test]
    fn basic_small_entries_persist() {
        assert_true!(Tests::basic::<CfgPersistSmallEntries>());
    }

    /// wrapping (large entries, no persist)
    #[test]
    fn wrapping_large_entries_no_persist() {
        assert_true!(Tests::wrapping::<CfgLargeEntries>());
    }

    /// wrapping (small entries, no persist)
    #[test]
    fn wrapping_small_entries_no_persist() {
        assert_true!(Tests::wrapping::<CfgSmallEntries>());
    }

    /// wrapping (large entries, persist)
    #[test]
    fn wrapping_large_entries_persist() {
        assert_true!(Tests::wrapping::<CfgPersistLargeEntries>());
    }

    /// wrapping (small entries, persist)
    #[test]
    fn wrapping_small_entries_persist() {
        assert_true!(Tests::wrapping::<CfgPersistSmallEntries>());
    }

    /// recovery (large entries, no persist)
    #[test]
    fn recovery_large_entries_no_persist() {
        assert_true!(Tests::recovery::<CfgLargeEntries>());
    }

    /// recovery (small entries, no persist)
    #[test]
    fn recovery_small_entries_no_persist() {
        assert_true!(Tests::recovery::<CfgSmallEntries>());
    }

    /// recovery (large entries, persist)
    #[test]
    fn recovery_large_entries_persist() {
        assert_true!(Tests::recovery::<CfgPersistLargeEntries>());
    }

    /// recovery (small entries, persist)
    #[test]
    fn recovery_small_entries_persist() {
        assert_true!(Tests::recovery::<CfgPersistSmallEntries>());
    }

    /// per_cpu_last_entries (large entries, no persist)
    #[test]
    fn per_cpu_last_entries_large_entries_no_persist() {
        assert_true!(Tests::per_cpu_last_entries::<CfgLargeEntries>());
    }

    /// per_cpu_last_entries (small entries, no persist)
    #[test]
    fn per_cpu_last_entries_small_entries_no_persist() {
        assert_true!(Tests::per_cpu_last_entries::<CfgSmallEntries>());
    }

    /// per_cpu_last_entries (large entries, persist)
    #[test]
    fn per_cpu_last_entries_large_entries_persist() {
        assert_true!(Tests::per_cpu_last_entries::<CfgPersistLargeEntries>());
    }

    /// per_cpu_last_entries (small entries, persist)
    #[test]
    fn per_cpu_last_entries_small_entries_persist() {
        assert_true!(Tests::per_cpu_last_entries::<CfgPersistSmallEntries>());
    }
}
