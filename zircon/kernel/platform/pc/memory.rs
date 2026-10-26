// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::arch_rs::x86::bootstrap16::{X86_BOOTSTRAP16_BUFFER_SIZE, x86_bootstrap16_init};
use crate::arch_rs::x86::feature::{X86CpuidLeafNum, x86_get_cpuid_leaf};
use crate::kernel::types::PAddr;
use crate::object::ResourceDispatcher;
use crate::vm::pmm;
use debug::{ltracef, tracef};
use libarch::x86::cpuid::AddressSizeInfo;
use memalloc::{Range, Type};
use zx_types::{ZX_RSRC_KIND_IOPORT, ZX_RSRC_KIND_MMIO, ZX_RSRC_KIND_SYSTEM, ZX_RSRC_SYSTEM_COUNT};

const LOCAL_TRACE: u32 = 0;

/// Discover the basic memory map.
///
/// # Safety
///
/// Must only be called once during single-threaded early boot with the physical memory map from
/// physboot. If `count > 0`, `ranges` must point to a valid array of `count` `memalloc::Range`
/// elements for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_pc_mem_init(ranges: *const Range, count: usize) {
    // SAFETY: Caller guarantees `ranges` points to `count` valid `Range` elements when `count > 0`.
    let ranges = unsafe { zr::slice_from_raw_parts(ranges, count) };
    // SAFETY: Caller guarantees single-threaded early-boot preconditions and a valid physical
    // memory map from physboot.
    unsafe { pc_mem_init(ranges) };
}

/// Discover the basic memory map.
///
/// # Safety
///
/// Must only be called once during single-threaded early boot with the physical memory map from
/// physboot.
pub unsafe fn pc_mem_init(ranges: &[Range]) {
    // SAFETY: Called once during single-threaded early boot after command-line parsing.
    unsafe { pmm::pmm_checker_init_from_cmdline() };

    // We don't want RAM under 1MiB to feature in to any arenas, so normalize that
    // here.
    let low_reserved_end =
        ranges.iter().take_while(|range| range.ty() == Type::ReservedLow).count();
    let (low_reserved, ranges) = ranges.split_at(low_reserved_end);

    // SAFETY: Called once during single-threaded early boot with the normalized physical memory
    // map from physboot.
    if let Err(status) = unsafe { pmm::init(ranges) } {
        tracef!("Error adding arenas from provided memory tables: error = {}\n", status.into_raw());
    }

    // Find an area that we can use for 16 bit bootstrapping of other SMP cores.
    const ALLOC_SIZE: u64 = X86_BOOTSTRAP16_BUFFER_SIZE as u64;
    const MIN_BASE: u64 = 2 * (page::SIZE as u64);
    let mut bootstrap16 = false;
    for range in low_reserved {
        let base = core::cmp::max(range.addr(), MIN_BASE);
        if base >= range.end() || range.end() - base < ALLOC_SIZE {
            continue;
        }

        // We have a valid range.
        ltracef!("Selected {:x} as bootstrap16 region\n", base);
        // SAFETY: `base` is page-aligned (`ReservedLow` ranges from physboot and `MIN_BASE` are
        // page-aligned), lies within a `ReservedLow` range below 1MiB with at least
        // `X86_BOOTSTRAP16_BUFFER_SIZE` bytes available, and is initialized at most once (`break`).
        unsafe { x86_bootstrap16_init(PAddr(base as usize)) };
        bootstrap16 = true;
        break;
    }
    if !bootstrap16 {
        tracef!("WARNING - Failed to assign bootstrap16 region, SMP won't work\n");
    }
}

/// Initialize the higher level PhysicalAspaceManager after the heap is initialized.
fn x86_resource_init_hook(_rl: init::LkInitLevel) {
    // An error is likely fatal if the bookkeeping is broken and driver resource allocation fails.
    let phys_addr_bits = x86_get_cpuid_leaf(X86CpuidLeafNum::ExtAddressSizes)
        .map_or(0, |leaf| AddressSizeInfo::from(leaf.a).phys_addr_bits());
    let _ =
        ResourceDispatcher::initialize_allocator(ZX_RSRC_KIND_MMIO, 0, 1usize << phys_addr_bits);
    let _ =
        ResourceDispatcher::initialize_allocator(ZX_RSRC_KIND_IOPORT, 0, (u16::MAX as usize) + 1);
    let _ = ResourceDispatcher::initialize_allocator(
        ZX_RSRC_KIND_SYSTEM,
        0,
        ZX_RSRC_SYSTEM_COUNT as usize,
    );

    // debug_uart.irq needs to be reserved here. See https://fxbug.dev/42109187.
}

init::lk_init_hook!(x86_resource_init, x86_resource_init_hook, init::LK_INIT_LEVEL_HEAP);
