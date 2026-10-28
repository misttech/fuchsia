// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::cell::UnsafeCell;
use core::convert::Infallible;
use core::ffi::{c_int, c_void};
use core::fmt::Write;
use pin_init::PinInit;
use pmm_checker_bindings as bindings;

use crate::arch_rs::vm::is_kernel_address;
use crate::counters::define_kcounter;
use crate::platform_rs::power::platform_panic_start;
use crate::vm::page::VmPagePtr;
use crate::vm::physmap::paddr_to_physmap;
use debug::dprintf::KernelConsoleWriter;
use debug::kernel_oops;
use kprint::kprint;

pub use boot_options::CheckFailAction;

// The number of times the pmm checker has detected corruption.
define_kcounter!(COUNTER_PATTERN_VALIDATION_FAILED, "vm.pmm.checker.validation_failed", Sum);

/// The value 0x43 was chosen because it stands out when interpreted as ASCII ('C') and is an odd
/// value that is less likely to occur naturally (e.g. arm64 instructions are 4-byte aligned).
const PATTERN_ONE_BYTE: u8 = 0x43;
/// [`PATTERN_ONE_BYTE`] repeated to fill a `u64`.
const PATTERN: u64 = 0x4343434343434343;

// This is a macro because it's passed to `kernel_oops!` and `kernel_oops!` requires a literal.
macro_rules! format_message {
    ($print:ident, $kvaddr:expr, $fill_size:expr) => {
        $print!(
            "pmm checker found unexpected pattern in page at {:p}; fill size is {}\n",
            $kvaddr,
            $fill_size
        )
    };
}

unsafe extern "C" {
    /// The kernel libc's `memset` that KASan does not check.
    ///
    /// The signature matches the `__typeof(memset)` declaration in the kernel libc's string.h.
    fn __unsanitized_memset(dst: *mut c_void, c: c_int, n: usize) -> *mut c_void;
}

/// Prints a hexdump of the page at `kvaddr` to the console.
///
/// # Safety
///
/// `kvaddr` must be the physmap address of a page.
unsafe fn dump_page(_fill_size: usize, kvaddr: *const u8) {
    kprint!("dump of page follows\n");
    let mut writer = KernelConsoleWriter;
    // SAFETY: The caller guarantees `kvaddr` is the physmap address of a page, so `page::SIZE`
    // bytes starting at `kvaddr` are mapped and readable.
    let _ = unsafe {
        pretty::hexdump8_very_ex_raw(&mut writer, kvaddr, page::SIZE, kvaddr.addr() as u64)
    };
}

/// Emits a kernel OOPS reporting corruption in `page`, then dumps the page.
///
/// # Safety
///
/// `kvaddr` must be the physmap address of `page`.
unsafe fn dump_page_and_oops(_page: VmPagePtr, fill_size: usize, kvaddr: *const u8) {
    format_message!(kernel_oops, kvaddr, fill_size);
    // SAFETY: The caller guarantees `kvaddr` is the physmap address of a page.
    unsafe { dump_page(fill_size, kvaddr) };
}

/// Reports corruption in `page`, dumps the page, and panics the kernel.
///
/// # Safety
///
/// `kvaddr` must be the physmap address of `page`.
unsafe fn dump_page_and_panic(_page: VmPagePtr, fill_size: usize, kvaddr: *const u8) -> ! {
    platform_panic_start();
    format_message!(kprint, kvaddr, fill_size);
    // SAFETY: The caller guarantees `kvaddr` is the physmap address of a page.
    unsafe { dump_page(fill_size, kvaddr) };
    panic!("pmm free list corruption suspected");
}

/// `PmmChecker` is used to detect memory corruption.  It is logically part of `PmmNode`.
///
/// Usage is as follows:
///
/// ```ignore
/// let mut checker = PmmChecker::default();
///
/// // Check only the first 16 bytes of each page.
/// unsafe { checker.set_fill_size(16) };
///
/// // For all free pages...
/// for page in free_pages {
///     unsafe { checker.fill_pattern(page) };
/// }
///
/// // Now that all free pages have been filled with a pattern, we can arm the checker.
/// unsafe { checker.arm() };
/// ...
/// unsafe { checker.assert_pattern(page) };
/// ```
///
/// # Concurrency
///
/// When embedded in `PmmNode` the checker is shared across CPUs. `fill_size` and `action` are only
/// changed (through `&mut`) before `PmmNode` publishes the checker to lockless readers, after
/// which they are immutable.
///
/// `armed` is set in `PmmNode::fill_free_pages_and_arm` after the checker has been published to
/// lockless `fill_pattern` callers that may concurrently hold `&PmmChecker`. To avoid forming a
/// `&mut PmmChecker` there, `armed` is stored in an `UnsafeCell` and changed through a shared
/// reference by [`Self::arm`], with synchronizing access to it left as the caller's
/// responsibility. A call to [`Self::arm`] must not race with any other access to `armed`, that is
/// with any call to [`Self::arm`], [`Self::is_armed`], [`Self::print_status`],
/// [`Self::print_status_stdout`], [`Self::validate_pattern`] or [`Self::assert_pattern`] on the
/// same checker. Every such pair of calls must be ordered by a happens-before relationship (for
/// example, by a lock).
#[repr(C)]
pub struct PmmChecker {
    /// The number of bytes to fill/validate.
    ///
    /// Safety invariant: `fill_size` satisfies [`Self::is_valid_fill_size`], and in particular is
    /// at most `page::SIZE`. [`Self::fill_pattern`] and [`Self::validate_pattern`] rely on this.
    fill_size: usize,

    /// The action taken when [`Self::assert_pattern`] detects corruption.
    action: CheckFailAction,

    /// Whether the checker is armed. See the type-level documentation for the synchronization
    /// requirements. `UnsafeCell<bool>` has the same in-memory representation as `bool`.
    armed: UnsafeCell<bool>,
}

// Compile-time layout assertions against the C++ PmmChecker type via bindgen.
zr::static_assert!(
    core::mem::size_of::<PmmChecker>() == core::mem::size_of::<bindings::PmmChecker>()
);
zr::static_assert!(
    core::mem::align_of::<PmmChecker>() == core::mem::align_of::<bindings::PmmChecker>()
);
zr::static_assert!(
    core::mem::offset_of!(PmmChecker, fill_size)
        == core::mem::offset_of!(bindings::PmmChecker, fill_size_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmChecker, action)
        == core::mem::offset_of!(bindings::PmmChecker, action_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmChecker, armed) == core::mem::offset_of!(bindings::PmmChecker, armed_)
);

impl Default for PmmChecker {
    fn default() -> Self {
        Self { fill_size: page::SIZE, action: Self::DEFAULT_ACTION, armed: UnsafeCell::new(false) }
    }
}

impl PmmChecker {
    /// See //zircon/kernel/lib/boot-options/include/lib/boot-options/options.inc
    /// for default value.
    pub const DEFAULT_ACTION: CheckFailAction = match bindings::PmmChecker_kDefaultActionRaw {
        v if v == CheckFailAction::Oops as u8 => CheckFailAction::Oops,
        v if v == CheckFailAction::Panic as u8 => CheckFailAction::Panic,
        _ => panic!("unknown CheckFailAction"),
    };

    /// Returns an initializer for an unarmed checker with the default fill size and action.
    pub fn init() -> impl PinInit<Self, Infallible> {
        Self::default()
    }

    /// Returns true if `fill_size` is a valid value.  Valid values are multiples of 8 between 8 and
    /// `page::SIZE`, inclusive.
    pub fn is_valid_fill_size(fill_size: usize) -> bool {
        (8..=page::SIZE).contains(&fill_size) && fill_size.is_multiple_of(8)
    }

    /// Sets the size of the pattern to be written / validated.
    ///
    /// It is an error to call this method with an invalid fill size
    /// (see [`Self::is_valid_fill_size`]).
    ///
    /// It is an error to call this method if the checker [`Self::is_armed`].  After changing the
    /// fill size, be sure to re-fill any free pages to ensure that a future call to
    /// [`Self::validate_pattern`] or [`Self::assert_pattern`] won't spuriously report corruption.
    ///
    /// # Safety
    ///
    /// `fill_size` must satisfy [`Self::is_valid_fill_size`]; [`Self::fill_pattern`] and
    /// [`Self::validate_pattern`] rely on `fill_size <= page::SIZE`.
    pub unsafe fn set_fill_size(&mut self, fill_size: usize) {
        debug_assert!(Self::is_valid_fill_size(fill_size));
        debug_assert!(!*self.armed.get_mut());
        self.fill_size = fill_size;
    }

    /// Returns the fill size.
    pub fn get_fill_size(&self) -> usize {
        self.fill_size
    }

    /// Sets the check fail action.
    pub fn set_action(&mut self, action: CheckFailAction) {
        self.action = action;
    }

    /// Returns the check fail action.
    pub fn get_action(&self) -> CheckFailAction {
        self.action
    }

    /// Returns true if armed.
    ///
    /// # Safety
    ///
    /// The caller must ensure that this call does not race with a call to [`Self::arm`] on the same
    /// checker; see the type-level documentation.
    pub unsafe fn is_armed(&self) -> bool {
        // SAFETY: The caller guarantees no concurrent call to `arm`, which is the only writer of
        // `armed` through a shared reference, so this read does not race with a write.
        unsafe { *self.armed.get() }
    }

    /// Arms the checker.
    ///
    /// # Safety
    ///
    /// The caller must ensure that this call does not race with any other access to `armed` on the
    /// same checker, that is with any concurrent call to [`Self::arm`], [`Self::is_armed`],
    /// [`Self::print_status`], [`Self::print_status_stdout`], [`Self::validate_pattern`] or
    /// [`Self::assert_pattern`]; see the type-level documentation.
    pub unsafe fn arm(&self) {
        // SAFETY: The caller guarantees no other access to `armed` is concurrent with this write,
        // and no Rust reference to `armed` is held across calls.
        unsafe { *self.armed.get() = true };
    }

    /// Prints the status of the checker to `f`.
    ///
    /// # Safety
    ///
    /// The caller must uphold the requirements of [`Self::is_armed`].
    pub unsafe fn print_status<W: Write>(&self, f: &mut W) {
        // SAFETY: The caller upholds the requirements of `is_armed`.
        let armed = unsafe { self.is_armed() };
        let _ = write!(
            f,
            "PMM: pmm checker {}, fill size is {}, action is ",
            if armed { "enabled" } else { "disabled" },
            self.fill_size
        );
        // Print the action the way `BootOptions::PrintValue` does, using the names given to the
        // values by `Enum<CheckFailAction>` in //zircon/kernel/lib/boot-options/enum.h.
        // LINT.IfChange
        let _ = f.write_str(match self.action {
            CheckFailAction::Oops => "oops",
            CheckFailAction::Panic => "panic",
        });
        // LINT.ThenChange(//zircon/kernel/lib/boot-options/enum.h)
        let _ = f.write_char('\n');
    }

    /// Prints the status of the checker to standard output.
    ///
    /// # Safety
    ///
    /// The caller must uphold the requirements of [`Self::is_armed`].
    pub unsafe fn print_status_stdout(&self) {
        // SAFETY: The caller upholds the requirements of `is_armed`.
        unsafe { self.print_status(&mut KernelConsoleWriter) };
    }

    /// Fills `page` with a pattern.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that `page` is valid, so that its metadata may be read, and that
    /// the first `fill_size` bytes of its physmap mapping may be written, with no Rust reference
    /// to those bytes live.
    pub unsafe fn fill_pattern(&self, page: VmPagePtr) {
        // SAFETY: The caller guarantees `page` is valid.
        let kvaddr = paddr_to_physmap(unsafe { page.paddr() });
        debug_assert!(is_kernel_address(kvaddr.0));
        // SAFETY: `kvaddr` is the physmap address of `page`, which the caller guarantees may be
        // written to. The physmap is mapped outside of Rust's abstract machine so its provenance
        // is exposed, making `with_exposed_provenance_mut` valid. `fill_size` is at most
        // `page::SIZE` by the safety invariant on the `fill_size` field.
        unsafe {
            __unsanitized_memset(
                core::ptr::with_exposed_provenance_mut(kvaddr.0),
                c_int::from(PATTERN_ONE_BYTE),
                self.fill_size,
            )
        };
    }

    /// Returns true if `page` contains the expected fill pattern or `is_armed` is false.
    ///
    /// Otherwise, returns false.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that `page` is valid, so that its metadata may be read, and that
    /// the first `fill_size` bytes of its physmap mapping may be read, with no mutable Rust
    /// reference to those bytes live. The caller must also uphold the requirements of
    /// [`Self::is_armed`].
    //
    // TODO(https://fxbug.dev/521834554): KAsan doesn't instrument Rust code, so the reads of the
    // page below are unsanitized. When KAsan is enabled this routine should be annotated to
    // disable instrumentation.
    #[must_use]
    pub unsafe fn validate_pattern(&self, page: VmPagePtr) -> bool {
        // SAFETY: The caller upholds the requirements of `is_armed`.
        if !unsafe { self.is_armed() } {
            return true;
        }

        // Validate the pattern.  There's a decent chance that, on arm64, checking 8 bytes at a
        // time will be faster than 1 byte at time.
        // SAFETY: The caller guarantees `page` is valid.
        let kvaddr =
            core::ptr::with_exposed_provenance::<u64>(paddr_to_physmap(unsafe { page.paddr() }).0);
        for j in 0..self.fill_size / 8 {
            // SAFETY: `kvaddr` is the page-aligned physmap address of `page`, which the caller
            // guarantees may be read. The physmap is mapped outside of Rust's abstract machine so
            // its provenance is exposed, making `with_exposed_provenance` valid. `j * 8` is less
            // than `fill_size`, which is at most `page::SIZE` by the safety invariant on the
            // `fill_size` field.
            if unsafe { kvaddr.add(j).read() } != PATTERN {
                return false;
            }
        }
        true
    }

    /// Panics the kernel if `page` does not contain the expected fill pattern and `is_armed` is
    /// true.
    ///
    /// Otherwise, does nothing.
    ///
    /// # Safety
    ///
    /// The caller must uphold the requirements of [`Self::validate_pattern`].
    pub unsafe fn assert_pattern(&self, page: VmPagePtr) {
        // SAFETY: The caller upholds the requirements of `validate_pattern`.
        if !unsafe { self.validate_pattern(page) } {
            COUNTER_PATTERN_VALIDATION_FAILED.add(1);
            // SAFETY: The caller guarantees `page` is valid.
            let kvaddr = core::ptr::with_exposed_provenance::<u8>(
                paddr_to_physmap(unsafe { page.paddr() }).0,
            );
            match self.action {
                // SAFETY: `kvaddr` is the physmap address of `page`.
                CheckFailAction::Oops => unsafe {
                    dump_page_and_oops(page, self.fill_size, kvaddr)
                },
                // SAFETY: `kvaddr` is the physmap address of `page`.
                CheckFailAction::Panic => unsafe {
                    dump_page_and_panic(page, self.fill_size, kvaddr)
                },
            }
        }
    }

    /// Returns the number of times the pmm checker has detected corruption.
    pub fn get_validation_failed_count() -> i64 {
        COUNTER_PATTERN_VALIDATION_FAILED.sum_across_all_cpus()
    }
}

#[unsafe(no_mangle)]
extern "C" fn rust_pmm_checker_is_valid_fill_size(fill_size: usize) -> bool {
    PmmChecker::is_valid_fill_size(fill_size)
}

// The `checker` parameters of the functions below are references, so C++ callers must pass a
// non-null pointer to a valid `PmmChecker` that is not mutated by another CPU for the duration of
// the call (other than `armed`, see below). Functions that access `armed` carry the requirements of
// the `PmmChecker` method they call; see `PmmChecker`'s documentation.

/// # Safety
///
/// The caller must uphold the requirements of [`PmmChecker::is_armed`].
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_pmm_checker_is_armed(checker: &PmmChecker) -> bool {
    // SAFETY: The caller upholds the requirements of `is_armed`.
    unsafe { checker.is_armed() }
}

/// # Safety
///
/// The caller must uphold the requirements of [`PmmChecker::print_status`].
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_pmm_checker_print_status(
    checker: &PmmChecker,
    f: &mut libc::stdio::FILE,
) {
    // SAFETY: The caller upholds the requirements of `print_status`.
    unsafe { checker.print_status(f) };
}

/// Unit tests for PmmChecker.
#[cfg(ktest)]
#[unittest::suite(name = "pmm_checker_rust")]
mod pmm_checker_rust {
    use super::PmmChecker;
    use crate::vm::page_state::VmPageState;
    use crate::vm::physmap::paddr_to_physmap;
    use crate::vm::pmm;
    use page::SIZE as PAGE_SIZE;
    use page_bindings::vm_page_state;
    use unittest::{expect_eq, expect_false, expect_ne, expect_true, unwrap_ok};

    macro_rules! pmm_checker_test_with_fill_size {
        ($fill_size:expr) => {{
            let fill_size = $fill_size;
            let mut checker = PmmChecker::default();

            // Starts off unarmed.
            // SAFETY: `checker` is local to this thread, so there is no concurrent `arm`.
            expect_false!(unsafe { checker.is_armed() });

            // Borrow a real page from the PMM, ask the checker to validate it. See that because
            // the checker is not armed, |validate_pattern| still returns true even though the page
            // has no pattern.
            let (page, pa) = unwrap_ok!(pmm::alloc_page(0));
            // SAFETY: This test owns the freshly allocated page.
            unsafe { page.set_state(VmPageState(vm_page_state::FREE)) };
            let p: *mut u8 = core::ptr::with_exposed_provenance_mut(paddr_to_physmap(pa).0);
            // SAFETY: `p` is the physmap address of a page owned by this test, the write stays
            // within the page, and no reference to the page's contents is live.
            unsafe { p.write_bytes(0, PAGE_SIZE) };
            // SAFETY: `page` is a valid page owned by this test, and `checker` is local to this
            // thread so there is no concurrent `arm`.
            expect_true!(unsafe { checker.validate_pattern(page) });
            // SAFETY: `page` is a valid page owned by this test, and `checker` is local to this
            // thread so there is no concurrent `arm`.
            unsafe { checker.assert_pattern(page) };

            // Set the fill size and see that |get_fill_size| returns the size.
            // SAFETY: Every fill size this test uses is valid.
            unsafe { checker.set_fill_size(fill_size) };
            expect_eq!(fill_size, checker.get_fill_size());

            // Arm the checker and see that |validate_pattern| returns false.
            // SAFETY: `checker` is local to this thread, so no other access to `armed` is
            // concurrent.
            unsafe { checker.arm() };
            // SAFETY: `checker` is local to this thread, so there is no concurrent `arm`.
            expect_true!(unsafe { checker.is_armed() });
            // SAFETY: `page` is a valid page owned by this test, and `checker` is local to this
            // thread so there is no concurrent `arm`.
            expect_false!(unsafe { checker.validate_pattern(page) });

            // Fill with pattern one less than the fill size and see that it does not pass
            // validation.
            // SAFETY: `p` is the physmap address of a page owned by this test, the write stays
            // within the page, and no reference to the page's contents is live.
            unsafe { p.write_bytes(0, fill_size - 1) };
            // SAFETY: `page` is a valid page owned by this test, and `checker` is local to this
            // thread so there is no concurrent `arm`.
            expect_false!(unsafe { checker.validate_pattern(page) });

            // Fill with the full pattern and see that it validates.
            // SAFETY: `page` is a valid page owned by this test, and `checker` is local to this
            // thread so there is no concurrent `arm`.
            unsafe { checker.fill_pattern(page) };
            for i in 0..fill_size {
                // SAFETY: `p` is the physmap address of a page owned by this test, `i` is less
                // than `PAGE_SIZE`, and no reference to the page's contents is live.
                expect_ne!(0, unsafe { p.add(i).read() });
            }
            // SAFETY: `page` is a valid page owned by this test, and `checker` is local to this
            // thread so there is no concurrent `arm`.
            expect_true!(unsafe { checker.validate_pattern(page) });

            // Corrupt the page after the first |fill_size| bytes and see that the corruption is
            // not detected.
            if fill_size < PAGE_SIZE {
                // SAFETY: `p` is the physmap address of a page owned by this test, `fill_size` is
                // less than `PAGE_SIZE`, and no reference to the page's contents is live.
                unsafe { p.add(fill_size).write(1) };
                // SAFETY: `page` is a valid page owned by this test, and `checker` is local to this
                // thread so there is no concurrent `arm`.
                expect_true!(unsafe { checker.validate_pattern(page) });
            }

            // Corrupt the page within the first |fill_size| bytes and see that the corruption is
            // detected.
            // SAFETY: `p` is the physmap address of a page owned by this test, `fill_size - 1` is
            // less than `PAGE_SIZE`, and no reference to the page's contents is live.
            unsafe { p.add(fill_size - 1).write(1) };
            // SAFETY: `page` is a valid page owned by this test, and `checker` is local to this
            // thread so there is no concurrent `arm`.
            expect_false!(unsafe { checker.validate_pattern(page) });

            // SAFETY: This test owns the page.
            unsafe { page.set_state(VmPageState(vm_page_state::ALLOC)) };
            // SAFETY: The page was allocated by `pmm::alloc_page` and is in the ALLOC state.
            unsafe { pmm::free_page(page) };
        }};
    }

    /// Tests PMM checker validation and pattern filling.
    #[test]
    fn checker() {
        pmm_checker_test_with_fill_size!(8);
        pmm_checker_test_with_fill_size!(16);
        pmm_checker_test_with_fill_size!(512);
        pmm_checker_test_with_fill_size!(PAGE_SIZE);
    }

    /// Tests validation of fill sizes for PmmChecker.
    #[test]
    fn is_valid_fill_size() {
        expect_false!(PmmChecker::is_valid_fill_size(0));
        expect_false!(PmmChecker::is_valid_fill_size(7));
        expect_false!(PmmChecker::is_valid_fill_size(9));
        expect_false!(PmmChecker::is_valid_fill_size(PAGE_SIZE + 8));
        expect_false!(PmmChecker::is_valid_fill_size(PAGE_SIZE * 2));

        expect_true!(PmmChecker::is_valid_fill_size(8));
        expect_true!(PmmChecker::is_valid_fill_size(16));
        expect_true!(PmmChecker::is_valid_fill_size(24));
        expect_true!(PmmChecker::is_valid_fill_size(512));
        expect_true!(PmmChecker::is_valid_fill_size(PAGE_SIZE));
    }
}
