// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use range_check::get_intersect_offset_len;
use zx_status::Status;
use zx_types::{
    ZX_RSRC_KIND_IOPORT, ZX_RSRC_KIND_IRQ, ZX_RSRC_KIND_MMIO, ZX_RSRC_KIND_SYSTEM, zx_rsrc_kind_t,
    zx_rsrc_system_base_t,
};

use super::handle::HandleValue;
use super::process_dispatcher::ProcessDispatcher;
use super::resource_dispatcher::ResourceDispatcher;
use crate::root_resource_filter::root_resource_filter_can_access_region;

// TODO(https://fxbug.dev/42107339): Take another look at validation and consider returning
// dispatchers or move validation into the parent dispatcher itself.

/// Check if the resource referenced by `handle` is of kind `kind` AND base `base`.
///
/// # Errors
///
/// - `ZX_ERR_BAD_HANDLE` if `handle` is not a valid handle.
/// - `ZX_ERR_WRONG_TYPE` if `handle` is not the right `kind` or `base`.
pub fn validate_resource_kind_base(
    handle: HandleValue,
    kind: zx_rsrc_kind_t,
    base: zx_rsrc_system_base_t,
) -> Result<(), Status> {
    let resource = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher::<ResourceDispatcher>(up, handle)
    })?;
    if resource.get_kind() == kind && resource.get_base() == base {
        Ok(())
    } else {
        Err(Status::WRONG_TYPE)
    }
}

/// Validates that `resource` grants access to the requested range for `kind`.
///
/// # Errors
///
/// - `ZX_ERR_ACCESS_DENIED` if access to the region is denied by the root resource filter.
/// - `ZX_ERR_WRONG_TYPE` if `resource`'s kind does not match `kind`.
/// - `ZX_ERR_OUT_OF_RANGE` if the requested range is not fully contained in `resource`'s range.
pub fn validate_ranged_resource_dispatcher(
    resource: &ResourceDispatcher,
    kind: zx_rsrc_kind_t,
    base: u64,
    size: usize,
) -> Result<(), Status> {
    // Resources get access to almost everything, but there are still resource ranges they are not
    // permitted to mint. For example:
    //
    // 1) All of physical RAM is off limits (with limited platform specific exceptions). It exists
    //    on the CPU accessible physical bus (so, the domain controlled by ZX_RSRC_KIND_MMIO) and
    //    user mode program should not be able to request access to physical RAM by address, they
    //    should be forced to go through the PMM using VMO creation instead.
    // 2) Any MMIO accessible interrupt controller registers.
    // 3) Any MMIO accessible IOMMU registers.
    //
    // Enforce that policy here by disallowing resource minting for any request which touches any
    // disallowed ranges.
    if resource.is_ranged_root(kind) {
        // If we are creating an MMIO resource from one of the root resources, make sure that range
        // being requested does not share a page with any of the kernel reserved regions.
        let (check_base, check_size) = if kind == ZX_RSRC_KIND_MMIO {
            let base = base as usize;
            let aligned_base = page::round_down(base);
            (aligned_base, page::round_up(base - aligned_base + size))
        } else {
            (base as usize, size)
        };

        if !root_resource_filter_can_access_region(check_base, check_size, kind) {
            return Err(Status::ACCESS_DENIED);
        }
        return Ok(());
    }

    if resource.get_kind() != kind {
        return Err(Status::WRONG_TYPE);
    }

    let (rbase, rsize) = (resource.get_base(), resource.get_size());

    // All resources need to track their lineage back to the root resource, and the root resource is
    // specifically prohibited from producing ranges which intersect anything in the deny list.
    // Since all resource ranges need to be a subset of their parent, it should be impossible for a
    // resource object to exist with a range which intersects anything in the deny list. Check that
    // with a debug assert here.
    debug_assert!(root_resource_filter_can_access_region(rbase as usize, rsize, kind));

    let size = size as u64;

    // Check for intersection and make sure the requested base+size fits within the resource's
    // address space allocation.
    match get_intersect_offset_len(base, size, rbase, rsize as u64) {
        Some((ibase, isize)) if ibase == base && isize == size => Ok(()),
        _ => Err(Status::OUT_OF_RANGE),
    }
}

/// Check if the resource referenced by `handle` is of kind `kind`. If `kind` matches the
/// resource's kind, then range validation between `base` and `size` will be made against the
/// resource's backing address space allocation.
///
/// # Errors
///
/// - `ZX_ERR_BAD_HANDLE` if `handle` is not a valid handle.
/// - `ZX_ERR_WRONG_TYPE` if `handle` is not a valid resource handle, or `kind` is invalid for the
///   request.
/// - `ZX_ERR_OUT_OF_RANGE` if the range specified by `base` and `size` is not granted by this
///   resource.
pub fn validate_ranged_resource(
    handle: HandleValue,
    kind: zx_rsrc_kind_t,
    base: u64,
    size: usize,
) -> Result<(), Status> {
    let resource = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher::<ResourceDispatcher>(up, handle)
    })?;
    validate_ranged_resource_dispatcher(&resource, kind, base, size)
}

/// Validates access to a single system resource indexed by `base`.
pub fn validate_system_resource(handle: HandleValue, base: u64) -> Result<(), Status> {
    validate_ranged_resource(handle, ZX_RSRC_KIND_SYSTEM, base, 1)
}

/// Validates enabling ioport access bits for a given process based on a resource handle.
pub fn validate_resource_ioport(handle: HandleValue, base: u64, size: usize) -> Result<(), Status> {
    validate_ranged_resource(handle, ZX_RSRC_KIND_IOPORT, base, size)
}

/// Validates mapping an MMIO range based on a resource handle.
pub fn validate_resource_mmio(handle: HandleValue, base: u64, size: usize) -> Result<(), Status> {
    validate_ranged_resource(handle, ZX_RSRC_KIND_MMIO, base, size)
}

/// Validates creation of an interrupt object based on a resource handle.
pub fn validate_resource_irq(handle: HandleValue, irq: u32) -> Result<(), Status> {
    validate_ranged_resource(handle, ZX_RSRC_KIND_IRQ, u64::from(irq), 1)
}

/// Kernel unit tests for resource validation functions.
#[cfg(ktest)]
#[unittest::suite(name = "resource_validation_tests")]
mod tests {
    use crate::object::resource_dispatcher::{ResourceDispatcher, ResourceStorage};
    use pin_init::stack_pin_init;
    use zx_status::Status;
    use zx_types::{ZX_RSRC_KIND_MMIO, ZX_RSRC_KIND_SYSTEM};

    use super::validate_ranged_resource_dispatcher;

    /// Tests validate_ranged_resource_dispatcher with ranged root and child MMIO resources.
    #[test]
    fn test_validate_ranged_resource_dispatcher() {
        stack_pin_init!(let storage = ResourceStorage::init());

        // Find an MMIO range that is not denied by the root resource filter (i.e. not in RAM).
        let step = 0x1000_0000u64;
        let mut test_base = 0u64;
        let found = loop {
            if root_resource_filter_can_access_region(
                test_base as usize,
                0x10_0000,
                ZX_RSRC_KIND_MMIO,
            ) {
                break true;
            }
            if test_base >= u64::MAX - step {
                break false;
            }
            test_base += step;
        };
        assert!(found, "could not find non-denied MMIO region");

        // Initialize MMIO allocator for tests
        ResourceDispatcher::initialize_allocator_with_storage(
            ZX_RSRC_KIND_MMIO,
            test_base,
            0x10_0000,
            &storage,
        )
        .expect("initialize MMIO allocator failed");

        // Create a ranged root resource for MMIO
        let (ranged_root_handle, _) = ResourceDispatcher::create_ranged_root_with_storage(
            ZX_RSRC_KIND_MMIO,
            b"mmio-root",
            &storage,
        )
        .expect("create ranged root failed");

        unittest::expect_ok!(validate_ranged_resource_dispatcher(
            ranged_root_handle.dispatcher(),
            ZX_RSRC_KIND_MMIO,
            test_base + 0x2000,
            0x1000,
        ));
        unittest::expect_err!(
            validate_ranged_resource_dispatcher(
                ranged_root_handle.dispatcher(),
                ZX_RSRC_KIND_SYSTEM,
                1,
                1,
            ),
            Status::WRONG_TYPE
        );

        // Create a concrete MMIO resource covering [child_base, child_base + child_size).
        let child_base = test_base + page::SIZE as u64 * 16;
        let child_size = page::SIZE * 2;
        let (child_handle, _) = ResourceDispatcher::create_with_storage(
            ZX_RSRC_KIND_MMIO,
            child_base,
            child_size,
            0,
            b"mmio-child",
            &storage,
        )
        .expect("create child MMIO failed");

        // Exactly contained in [child_base, child_base + child_size)
        unittest::expect_ok!(validate_ranged_resource_dispatcher(
            child_handle.dispatcher(),
            ZX_RSRC_KIND_MMIO,
            child_base,
            child_size,
        ));

        // Valid sub-ranges
        unittest::expect_ok!(validate_ranged_resource_dispatcher(
            child_handle.dispatcher(),
            ZX_RSRC_KIND_MMIO,
            child_base,
            page::SIZE,
        ));
        unittest::expect_ok!(validate_ranged_resource_dispatcher(
            child_handle.dispatcher(),
            ZX_RSRC_KIND_MMIO,
            child_base + page::SIZE as u64,
            page::SIZE,
        ));

        // Out of range (starts before parent)
        unittest::expect_err!(
            validate_ranged_resource_dispatcher(
                child_handle.dispatcher(),
                ZX_RSRC_KIND_MMIO,
                child_base - page::SIZE as u64,
                child_size,
            ),
            Status::OUT_OF_RANGE
        );

        // Out of range (extends past parent)
        unittest::expect_err!(
            validate_ranged_resource_dispatcher(
                child_handle.dispatcher(),
                ZX_RSRC_KIND_MMIO,
                child_base,
                child_size + page::SIZE,
            ),
            Status::OUT_OF_RANGE
        );

        // Out of range entirely
        unittest::expect_err!(
            validate_ranged_resource_dispatcher(
                child_handle.dispatcher(),
                ZX_RSRC_KIND_MMIO,
                test_base + 0x20000,
                0x1000,
            ),
            Status::OUT_OF_RANGE
        );

        // Overflow boundaries (u64::MAX, usize::MAX)
        unittest::expect_err!(
            validate_ranged_resource_dispatcher(
                child_handle.dispatcher(),
                ZX_RSRC_KIND_MMIO,
                u64::MAX,
                page::SIZE,
            ),
            Status::OUT_OF_RANGE
        );
        unittest::expect_err!(
            validate_ranged_resource_dispatcher(
                child_handle.dispatcher(),
                ZX_RSRC_KIND_MMIO,
                child_base,
                usize::MAX,
            ),
            Status::OUT_OF_RANGE
        );
    }
}
