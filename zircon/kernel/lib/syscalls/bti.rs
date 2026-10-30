// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::mem::MaybeUninit;

use crate::object::{
    BusTransactionInitiatorDispatcher, HandleOwner, HandleValue, IOMMU_FLAG_PERM_EXECUTE,
    IOMMU_FLAG_PERM_READ, IOMMU_FLAG_PERM_WRITE, IommuDispatcher, PinnedMemoryTokenDispatcher,
    ProcessDispatcher, VmObjectDispatcher, dev_vaddr_t,
};
use crate::user_copy::UserOutPtr;
use crate::vm::pmm;
use debug::ltracef;
use syscalls_macro::syscall;
use zerocopy::{Immutable, IntoBytes};
use zx_status::Status;
use zx_types::{
    ZX_BTI_COMPRESS, ZX_BTI_CONTIGUOUS, ZX_BTI_PERM_EXECUTE, ZX_BTI_PERM_READ, ZX_BTI_PERM_WRITE,
    ZX_RIGHT_MAP, ZX_RIGHT_NONE, ZX_RIGHT_READ, ZX_RIGHT_WRITE, zx_paddr_t,
};

const LOCAL_TRACE: u32 = 0;

/// Helper for optimizing writing many small elements of user ptr array by allowing for a variable
/// amount of buffering.
struct BufferedUserOutPtr<T: IntoBytes + Immutable + Copy, const N: usize> {
    out_ptr: UserOutPtr<T>,
    buffer: [MaybeUninit<T>; N],
    index: usize,
}

impl<T: IntoBytes + Immutable + Copy, const N: usize> BufferedUserOutPtr<T, N> {
    fn new(out_ptr: UserOutPtr<T>) -> Self {
        // Expectation is this is going to be stack allocated, so ensure it's not too big.
        const { assert!(core::mem::size_of::<[MaybeUninit<T>; N]>() < page::SIZE) };
        Self { out_ptr, buffer: [const { MaybeUninit::uninit() }; N], index: 0 }
    }

    /// Add a single element, either appending to the buffer and/or flushing the buffer if full.
    fn write(&mut self, val: T) -> Result<(), Status> {
        self.buffer[self.index].write(val);
        self.index += 1;
        if self.index == N {
            self.flush()?;
        }
        Ok(())
    }

    /// Flush any remaining buffered items. Must be called prior to destruction.
    fn flush(&mut self) -> Result<(), Status> {
        if self.index == 0 {
            return Ok(());
        }
        let count = self.index;
        self.index = 0;
        // SAFETY: `self.buffer[..count]` has been initialized via `write`.
        let slice = unsafe { core::slice::from_raw_parts(self.buffer.as_ptr().cast::<T>(), count) };
        self.out_ptr.copy_slice_to_user(slice)?;
        self.out_ptr = self.out_ptr.element_offset(count);
        Ok(())
    }
}

impl<T: IntoBytes + Immutable + Copy, const N: usize> Drop for BufferedUserOutPtr<T, N> {
    fn drop(&mut self) {
        // Ensure Flush was called and everything got written out.
        assert!(self.index == 0);
    }
}

#[syscall]
pub fn sys_bti_create(
    iommu: HandleValue,
    options: u32,
    bti_id: u64,
    out: &mut HandleValue,
) -> Result<(), Status> {
    ltracef!("options {options:#x}, bti_id {bti_id}\n");

    if options != 0 {
        return Err(Status::INVALID_ARGS);
    }

    // TODO(teisenbe): This should probably have a right on it.
    let iommu_dispatcher = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<IommuDispatcher>(up, iommu, ZX_RIGHT_NONE)
    })?;

    let (handle, rights) =
        BusTransactionInitiatorDispatcher::create(iommu_dispatcher.iommu(), bti_id)?;
    *out = handle.make_and_add_handle(rights)?;
    Ok(())
}

#[syscall]
pub fn sys_bti_release_quarantine(handle: HandleValue) -> Result<(), Status> {
    ltracef!("handle {:#x}\n", handle.raw_value());

    let bti_dispatcher = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<BusTransactionInitiatorDispatcher>(
            up,
            handle,
            ZX_RIGHT_WRITE,
        )
    })?;

    bti_dispatcher.release_quarantine();
    Ok(())
}

#[syscall]
pub fn sys_bti_pin(
    handle: HandleValue,
    options: u32,
    vmo: HandleValue,
    offset: u64,
    size: u64,
    addrs: UserOutPtr<zx_paddr_t>,
    addrs_count: usize,
    pmt: &mut HandleValue,
) -> Result<(), Status> {
    ltracef!(
        "handle {:#x}, options {options:#x}, vmo {:#x}, offset {offset:#x}, size {size:#x}, addrs_count {addrs_count}\n",
        handle.raw_value(),
        vmo.raw_value(),
    );

    let up = ProcessDispatcher::get_current();
    let bti_dispatcher =
        up.handle_table().get_dispatcher_with_rights::<BusTransactionInitiatorDispatcher>(
            &up,
            handle,
            ZX_RIGHT_MAP,
        )?;

    // Address count is currently limited to the amount of addresses that can fit on 64 pages. This
    // is large enough for all current usage of bti_pin, but protects against the case of an
    // arbitrarily large array being allocated on the heap.
    const MAX_ADDRS: usize = (page::SIZE * 64) / core::mem::size_of::<dev_vaddr_t>();
    if !page::is_aligned(offset as usize)
        || !page::is_aligned(size as usize)
        || addrs_count > MAX_ADDRS
    {
        return Err(Status::INVALID_ARGS);
    }

    let (vmo_dispatcher, vmo_rights) = up
        .handle_table()
        .get_dispatcher_with_rights_and_actual::<VmObjectDispatcher>(&up, vmo, ZX_RIGHT_MAP)?;

    // Convert requested permissions and check against VMO rights
    let mut iommu_perms = 0u32;
    let mut compress_results = false;
    let mut contiguous = false;
    let mut remaining_options = options;

    if (remaining_options & ZX_BTI_PERM_READ) != 0 {
        if (vmo_rights & ZX_RIGHT_READ) == 0 {
            return Err(Status::ACCESS_DENIED);
        }
        iommu_perms |= IOMMU_FLAG_PERM_READ;
        remaining_options &= !ZX_BTI_PERM_READ;
    }
    if (remaining_options & ZX_BTI_PERM_WRITE) != 0 {
        if (vmo_rights & ZX_RIGHT_WRITE) == 0 {
            return Err(Status::ACCESS_DENIED);
        }
        iommu_perms |= IOMMU_FLAG_PERM_WRITE;
        remaining_options &= !ZX_BTI_PERM_WRITE;
    }
    if (remaining_options & ZX_BTI_PERM_EXECUTE) != 0 {
        // Note: We check ZX_RIGHT_READ instead of ZX_RIGHT_EXECUTE
        // here because the latter applies to execute permission of
        // the host CPU, whereas ZX_BTI_PERM_EXECUTE applies to
        // transactions initiated by the bus device.
        if (vmo_rights & ZX_RIGHT_READ) == 0 {
            return Err(Status::ACCESS_DENIED);
        }
        iommu_perms |= IOMMU_FLAG_PERM_EXECUTE;
        remaining_options &= !ZX_BTI_PERM_EXECUTE;
    }
    if !((remaining_options & ZX_BTI_COMPRESS != 0) && (remaining_options & ZX_BTI_CONTIGUOUS != 0))
    {
        if (remaining_options & ZX_BTI_COMPRESS) != 0 {
            compress_results = true;
            remaining_options &= !ZX_BTI_COMPRESS;
        }
        if (remaining_options & ZX_BTI_CONTIGUOUS) != 0 && vmo_dispatcher.vmo().is_contiguous() {
            contiguous = true;
            remaining_options &= !ZX_BTI_CONTIGUOUS;
        }
    }
    if remaining_options != 0 {
        return Err(Status::INVALID_ARGS);
    }

    let (new_pmt_handle, new_pmt_rights) =
        bti_dispatcher.pin(vmo_dispatcher.vmo().clone(), offset, size, iommu_perms)?;

    // Until `clear_kernel_cleanup_needed()` is called below, `new_pmt_handle`
    // owns the responsibility for unpinning on error: if anything fails before
    // or during handle allocation, dropping `new_pmt_handle` invokes
    // `on_zero_handles()`, which unpins the PMT before notifying it of zero
    // handles so that it is not placed on the BTI's quarantine list.

    // Based on the passed in options, determine what size chunks we are going to report to the
    // user, and how many of those there will be.
    let (target_contig, expected_addrs) = if compress_results {
        let min_contig = bti_dispatcher.minimum_contiguity() as usize;
        let expected = (size as usize).div_ceil(min_contig);
        (min_contig, expected)
    } else if contiguous {
        (size as usize, 1)
    } else {
        (page::SIZE, size as usize / page::SIZE)
    };

    if addrs_count != expected_addrs {
        return Err(Status::INVALID_ARGS);
    }

    // Define a helper closure with some state that can fetch potentially large ranges from the
    // PMT, but return them gradually. This just serves as an optimization around repeatedly
    // querying the PMT for a range that it knows is contiguous, but where we need to fill out
    // multiple addresses for the user.
    struct ConsumeState {
        offset: u64,
        addr: dev_vaddr_t,
        remaining: usize,
    }
    let mut consume_state = ConsumeState { offset: 0, addr: 0, remaining: 0 };

    let mut consume_addr = |expected_contig: usize| -> Result<dev_vaddr_t, Status> {
        // If the amount of contiguous memory we are tracking in our consume_state
        // is not enough to satisfy the request, we need to perform another query.
        // Otherwise, we can just consume from the existing consume state.
        if expected_contig > consume_state.remaining {
            let remain = (size - consume_state.offset) as usize;
            let range = new_pmt_handle.dispatcher().query_address(consume_state.offset, remain)?;
            if expected_contig > range.size {
                // This happening suggests an error with contiguity calculations and/or the
                // underlying IOMMU implementation not reporting its contiguity correctly.
                //
                // TODO(teisenbe): consider making this a louder error.
                return Err(Status::INVALID_ARGS);
            }
            consume_state.addr = range.device_vaddr;
            consume_state.remaining = range.size;
        }

        let ret = consume_state.addr;
        consume_state.offset += expected_contig as u64;
        consume_state.addr += expected_contig as dev_vaddr_t;
        consume_state.remaining -= expected_contig;
        Ok(ret)
    };

    const {
        assert!(
            core::mem::size_of::<dev_vaddr_t>() == core::mem::size_of::<zx_paddr_t>(),
            "mismatched types"
        );
    };

    // Calculate / lookup the addresses.
    let mut buffered_addrs =
        BufferedUserOutPtr::<dev_vaddr_t, 32>::new(addrs.reinterpret::<dev_vaddr_t>());
    for i in 0..addrs_count {
        let expected_size = core::cmp::min(size as usize - (target_contig * i), target_contig);
        let addr = consume_addr(expected_size)?;
        buffered_addrs.write(addr)?;
    }

    buffered_addrs.flush()?;
    debug_assert_eq!(buffered_addrs.index, 0);

    // Allocate the `HandleOwner` before clearing `kernel_cleanup_needed` and
    // before adding the handle to the process's handle table:
    // 1. `HandleOwner::make` is the only fallible step. If it fails for want of
    //    memory, `new_pmt_handle` is dropped inside `HandleOwner::make` while
    //    `kernel_cleanup_needed` is still `true`, so `on_zero_handles()` unpins
    //    the PMT rather than quarantining it.
    // 2. Once `HandleOwner::make` succeeds, `clear_kernel_cleanup_needed()` must
    //    run *before* `up.handle_table().add_handle(handle_owner)` publishes the
    //    handle to the process's handle table (at which point another thread in
    //    the process could close the handle and trigger `on_zero_handles()`,
    //    which must quarantine the PMT if `zx_pmt_unpin` was not called).
    // 3. Downcasting `handle_owner.dispatcher()` avoids cloning the `RefPtr`
    //    across `HandleOwner::make` (which moves the `RefPtr` out of
    //    `new_pmt_handle` with no extra atomic reference count updates).
    let Some(handle_owner) = HandleOwner::make(new_pmt_handle, new_pmt_rights) else {
        // See comment in `ProcessDispatcher::MakeAndAddHandle`.
        pmm::node().report_alloc_failure(pmm::AllocFailure {
            r#type: pmm::AllocFailureType::Handle,
            size: 1,
            free_count: 0,
        });
        return Err(Status::NO_MEMORY);
    };
    handle_owner
        .dispatcher()
        .downcast::<PinnedMemoryTokenDispatcher>()
        .unwrap()
        .clear_kernel_cleanup_needed();
    let handle = up.handle_table().map_handle_to_value(&handle_owner);
    up.handle_table().add_handle(handle_owner);
    *pmt = handle;
    Ok(())
}
