// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! LLVM SanitizerCoverage (`trace-pc-guard`) runtime support.
//!
//! All atomic accesses to [`SANCOV_PC_TABLE`], [`SANCOV_PC_COUNTS`], and the guard/counter arrays
//! use [`Ordering::Relaxed`]: the tables are initialized once on the boot CPU at
//! `LK_INIT_LEVEL_KERNEL` before secondary CPUs are brought online (followed by a
//! [`compiler_fence(Ordering::SeqCst)`]), and the runtime coverage hit counters and one-time PC
//! slot claim flags are independent monotonic values that do not synchronize other memory accesses.

use core::mem::size_of;
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering, compiler_fence};

use crate::vm::pmm;
use crate::vm::vm_object_paged::VmObjectPaged;

use super::kernel_mapped_vmo::KernelMappedVmo;

// The sancov file format is trivial: magic number and array of PCs.
// Each word after the first is either 0 or a PC that was hit.

// TODO(mcgrathr): Move the constant into a header shared with other impls.
const MAGIC64: u64 = 0xC0BFFFFFFFFFFF64;

const COUNTS_MAGIC: u64 = 0x0023766f436e6153; // "SanCov#" (LE)

// Go back from the return address to the call site.
// Note this must exactly match the calculation in the sancov tool.
#[cfg(target_arch = "aarch64")]
// Fixed-size instructions, so go back to the previous instruction exactly.
const RETURN_ADDRESS_BIAS: usize = 4;

#[cfg(not(target_arch = "aarch64"))]
// Variable-sized instructions, so just go back one byte into the middle.
const RETURN_ADDRESS_BIAS: usize = 1;

/// Returns the byte size of the sancov PC or counter table for `guards_count` guards.
#[inline]
#[sanitize(address = "off")]
pub(super) const fn data_size(guards_count: usize) -> usize {
    (guards_count + 1) * size_of::<u64>()
}

// Instrumented code runs from the earliest point, before initialization.  The
// memory for storing the PCs and counts hasn't been set up.  However, code is
// running only on the boot CPU.  So in the pre-initialization period, we
// accumulate 32-bit counts in the __sancov_guard slots.  Then after the full
// buffers are set up, we copy those counts into the 64-bit counter slots and
// re-zero all the guard slots.  Thereafter, each guard slot serves as an
// atomic flag indicating whether its corresponding PC has been stored yet.
// This way, no early PC hits are lost in the counts.  However, for PCs whose
// only hits were before buffer setup, the nonzero counts will be paired with
// zero PC slots because the PC values are only saved in the real buffers.

static SANCOV_PC_TABLE: AtomicPtr<AtomicU64> = AtomicPtr::new(ptr::null_mut());
static SANCOV_PC_COUNTS: AtomicPtr<AtomicU64> = AtomicPtr::new(ptr::null_mut());

/// Allocates the sancov PC and counter VMOs, maps them into the kernel address space, and
/// migrates pre-initialization counts from `guards`.
#[sanitize(address = "off")]
pub(super) fn init_sancov(
    guards: &[AtomicU32],
    pc_vmo: &KernelMappedVmo,
    counts_vmo: &KernelMappedVmo,
) {
    let guards_count = guards.len();
    let vmo = VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, data_size(guards_count) as u64);
    assert!(vmo.is_ok());
    let status = pc_vmo.init(
        VmObjectPaged::into_vm_object(vmo.unwrap()),
        0,
        data_size(guards_count),
        c"sancov-pc-table",
    );
    assert!(status.is_ok());

    let vmo = VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, data_size(guards_count) as u64);
    assert!(vmo.is_ok());
    let status = counts_vmo.init(
        VmObjectPaged::into_vm_object(vmo.unwrap()),
        0,
        data_size(guards_count),
        c"sancov-pc-counts-table",
    );
    assert!(status.is_ok());

    // SAFETY: `pc_vmo.init` and `counts_vmo.init` succeeded above, mapping at least
    // `data_size(guards_count)` bytes (`guards_count + 1` `u64` elements) for each VMO.
    unsafe {
        let pc_table = ptr::with_exposed_provenance_mut::<AtomicU64>(pc_vmo.base_locking());
        let pc_counts = ptr::with_exposed_provenance_mut::<AtomicU64>(counts_vmo.base_locking());
        SANCOV_PC_TABLE.store(pc_table, Ordering::Relaxed);

        (*pc_table).store(MAGIC64, Ordering::Relaxed);
        (*pc_counts).store(COUNTS_MAGIC, Ordering::Relaxed);

        // Move the counts accumulated in the guard slots into their proper places,
        // and reset the guards.
        for (i, guard) in guards.iter().enumerate() {
            (*pc_counts.add(i + 1))
                .store(guard.swap(0, Ordering::Relaxed) as u64, Ordering::Relaxed);
        }

        SANCOV_PC_COUNTS.store(pc_counts, Ordering::Relaxed);
    }

    // Just in case of LTO or whatnot, ensure that everything is in place before
    // returning to run any instrumented code.
    compiler_fence(Ordering::SeqCst);
}

/// This is run along with static constructors, pretty early in startup.
/// It's always run on the boot CPU before secondary CPUs are started up.
#[sanitize(address = "off")]
pub(super) fn sanitizer_cov_trace_pc_guard_init(
    start: *const u32,
    end: *const u32,
    expected_start: *const u32,
    expected_end: *const u32,
) {
    // It's always called with the bounds of the section, which for the
    // kernel are known statically anyway.
    assert!(ptr::eq(start, expected_start));
    assert!(ptr::eq(end, expected_end));
}

/// This is called every time through a covered event.
/// This might be run before `sanitizer_cov_trace_pc_guard_init` has run.
///
/// # Safety
///
/// - `guard_ptr` must point to a valid, aligned element within the `__sancov_guards` array
///   starting at `guards_start` (with index `< guards.len()` passed to `init_sancov` if
///   initialized).
/// - Pre-initialization (`SANCOV_PC_COUNTS` is null), this must only be called on the boot CPU
///   without concurrent racing accesses to `guard_ptr`.
#[sanitize(address = "off")]
pub(super) unsafe fn sanitizer_cov_trace_pc_guard(
    guard_ptr: *mut u32,
    guards_start: *const u32,
    return_address: usize,
    executable_start: usize,
) {
    // SAFETY: Caller guarantees `guard_ptr` points to a valid, aligned `u32` slot in
    // `__sancov_guards`.
    let guard = unsafe { AtomicU32::from_ptr(guard_ptr) };

    let pc_counts = SANCOV_PC_COUNTS.load(Ordering::Relaxed);
    if pc_counts.is_null() {
        // Pre-initialization, just count the hit in the guard slot.  See above.
        guard.fetch_add(1, Ordering::Relaxed);
        return;
    }

    // Compute the table index based just on the address of the guard.
    // The SANCOV_PC_TABLE and SANCOV_PC_COUNTS arrays parallel the guards,
    // but the first slot in each of those is reserved for the magic number.
    // SAFETY: Caller guarantees `guard_ptr` lies within the array starting at `guards_start`.
    let idx = unsafe { guard_ptr.offset_from_unsigned(guards_start) }.wrapping_add(1);

    // Every time through, increment the counter.
    // SAFETY: `pc_counts` was initialized with `guards.len() + 1` elements and
    // `1 <= idx <= guards.len()`.
    let count = unsafe { &*pc_counts.add(idx) };
    count.fetch_add(1, Ordering::Relaxed);

    // Use the guard as a simple flag to indicate whether the PC has been stored.
    if guard.load(Ordering::Relaxed) == 0 && guard.swap(1, Ordering::Relaxed) == 0 {
        // This is really the first time through this PC on any CPU.
        // This is now the only path that will ever use this slot in
        // the table, so storing there doesn't need to be atomic.
        let pc_table = SANCOV_PC_TABLE.load(Ordering::Relaxed);
        // SAFETY: Winning the `0 -> 1` swap on `guard` guarantees exclusive write access to
        // `pc_table.add(idx)`.
        unsafe {
            (*pc_table.add(idx)).store(
                return_address
                    // Take the raw return address and adjust it to point into the call instruction.
                    .wrapping_sub(RETURN_ADDRESS_BIAS)
                    // Adjust it from runtime to link-time addresses so no
                    // further adjustment is needed to decode the data.
                    .wrapping_sub(executable_start) as u64,
                Ordering::Relaxed,
            );
        }
    }
}
