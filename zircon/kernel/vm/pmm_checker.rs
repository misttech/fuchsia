// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::marker::{PhantomData, PhantomPinned};
use pin_init::{PinInit, pin_data};
use pmm_checker_bindings as bindings;
use zr::Opaque;

use crate::vm::page::VmPagePtr;

#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CheckFailAction {
    Oops = 0,
    Panic = 1,
}

/// `PmmChecker` is used to detect memory corruption.  It is logically part of `PmmNode`.
///
/// Usage is as follows:
///
/// ```
/// stack_pin_init!(let checker = PmmChecker::init());
/// let mut checker = unsafe { checker.get_unchecked_mut() };
///
/// // Check only the first 16 bytes of each page.
/// checker.set_fill_size(16);
///
/// // For all free pages...
/// for page in free_pages {
///     checker.fill_pattern(page);
/// }
///
/// // Now that all free pages have been filled with a pattern, we can arm the checker.
/// checker.arm();
/// ...
/// checker.assert_pattern(page);
/// ```
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct PmmChecker {
    raw: Opaque<bindings::PmmChecker>,
    phantom: PhantomData<PhantomPinned>,
}

zr::unsafe_pinned_drop_ffi!(PmmChecker, bindings::cpp_pmm_checker_destroy);

impl PmmChecker {
    pub fn init() -> impl PinInit<Self, core::convert::Infallible> {
        zr::pin_init_ffi!(bindings::cpp_pmm_checker_init)
    }
    /// Domain-specific conversion: returns raw pointer for `PmmChecker`.
    pub fn as_raw(&self) -> *mut bindings::PmmChecker {
        self.raw.get()
    }

    /// Returns true if `fill_size` is a valid value.  Valid values are multiples of 8 between 8 and
    /// `page::SIZE`, inclusive.
    pub fn is_valid_fill_size(fill_size: usize) -> bool {
        // SAFETY: No preconditions.
        unsafe { bindings::cpp_pmm_checker_is_valid_fill_size(fill_size) }
    }

    /// Sets the size of the pattern to be written / validated.
    ///
    /// It is an error to call this method with an invalid fill size
    /// (see [`Self::is_valid_fill_size`]).
    ///
    /// It is an error to call this method if the checker [`Self::is_armed`].  After changing the
    /// fill size, be sure to re-fill any free pages to ensure that a future call to
    /// [`Self::validate_pattern`] or [`Self::assert_pattern`] won't spuriously report corruption.
    pub fn set_fill_size(&mut self, fill_size: usize) {
        // SAFETY: `self.as_raw()` returns a valid `PmmChecker` pointer.
        unsafe { bindings::cpp_pmm_checker_set_fill_size(self.as_raw(), fill_size) }
    }

    /// Returns the fill size.
    pub fn get_fill_size(&self) -> usize {
        // SAFETY: `self.as_raw()` returns a valid `PmmChecker` pointer.
        unsafe { bindings::cpp_pmm_checker_get_fill_size(self.as_raw()) }
    }

    /// Sets the check fail action.
    pub fn set_action(&mut self, action: CheckFailAction) {
        // SAFETY: `self.as_raw()` returns a valid `PmmChecker` pointer.
        unsafe { bindings::cpp_pmm_checker_set_action(self.as_raw(), action as u8) }
    }

    /// Returns the check fail action.
    pub fn get_action(&self) -> CheckFailAction {
        // SAFETY: `self.as_raw()` returns a valid `PmmChecker` pointer.
        let raw = unsafe { bindings::cpp_pmm_checker_get_action(self.as_raw()) };
        match raw {
            0 => CheckFailAction::Oops,
            _ => CheckFailAction::Panic,
        }
    }

    /// Returns true if armed.
    pub fn is_armed(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `PmmChecker` pointer.
        unsafe { bindings::cpp_pmm_checker_is_armed(self.as_raw()) }
    }

    /// Arms the checker.
    pub fn arm(&mut self) {
        // SAFETY: `self.as_raw()` returns a valid `PmmChecker` pointer.
        unsafe { bindings::cpp_pmm_checker_arm(self.as_raw()) }
    }

    /// Fills `page` with a pattern.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is valid and may be written to.
    pub unsafe fn fill_pattern(&self, page: VmPagePtr) {
        // SAFETY: `self.as_raw()` and `page.as_ffi()` return valid pointers.
        unsafe { bindings::cpp_pmm_checker_fill_pattern(self.as_raw(), page.as_ffi()) }
    }

    /// Returns true if `page` contains the expected fill pattern or `is_armed` is false.
    ///
    /// Otherwise, returns false.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is valid and may be read to.
    pub unsafe fn validate_pattern(&self, page: VmPagePtr) -> bool {
        // SAFETY: `self.as_raw()` and `page.as_ffi()` return valid pointers.
        unsafe { bindings::cpp_pmm_checker_validate_pattern(self.as_raw(), page.as_ffi()) }
    }

    /// Panics the kernel if `page` does not contain the expected fill pattern and `is_armed` is
    /// true.
    ///
    /// Otherwise, does nothing.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is valid and may be read to.
    pub unsafe fn assert_pattern(&self, page: VmPagePtr) {
        // SAFETY: `self.as_raw()` and `page.as_ffi()` return valid pointers.
        unsafe { bindings::cpp_pmm_checker_assert_pattern(self.as_raw(), page.as_ffi()) }
    }
}
