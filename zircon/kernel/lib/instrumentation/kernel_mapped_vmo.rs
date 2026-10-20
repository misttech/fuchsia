// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::types::VAddr;
use crate::object::{HandleOwner, InitialMutability, VmObjectDispatcher};
use crate::vm::arch_vm_aspace::{ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE};
use crate::vm::pinned_vm_object::PinnedVmObject;
use crate::vm::vm_address_region::flag as vmar_flag;
use crate::vm::vm_aspace::VmAspace;
use crate::vm::vm_mapping::VmMapping;
use crate::vm::vm_object::VmObject;
use fbl::RefPtr;
use zx_status::Status;
use zx_types::ZX_RIGHT_WRITE;

/// This maintains a kernel mapping from a VMO; mapped pages must stay pinned to
/// prevent kernel-mode page faults.  Destroying this object unmaps and unpins.
///
/// C++ holds this inside an `OpaqueStorage<32, 8>` and constructs/destroys it via
/// `rust_kernel_mapped_vmo_new` / `rust_kernel_mapped_vmo_drop_in_place`, so only the size and
/// alignment (not the field layout) are part of the FFI contract.
#[repr(align(8))]
pub struct KernelMappedVmo {
    pinned_vmo: PinnedVmObject,
    mapping: Option<RefPtr<VmMapping>>,
}

zr::static_assert_size_and_align!(KernelMappedVmo, 32, 8);

// SAFETY: `KernelMappedVmo` owns a `PinnedVmObject` and a `RefPtr<VmMapping>`, both of which
// refer to internally synchronized C++ objects, and is not tied to the thread that created it.
unsafe impl Send for KernelMappedVmo {}
// SAFETY: `KernelMappedVmo` has no interior mutability of its own; its fields are only written
// through `&mut self` (`init`) and `&self` methods only call methods on the internally
// synchronized C++ `VmObject` and `VmMapping`.
unsafe impl Sync for KernelMappedVmo {}

impl Default for KernelMappedVmo {
    fn default() -> Self {
        Self::new()
    }
}

impl KernelMappedVmo {
    /// Creates an uninitialized `KernelMappedVmo` with no pinned VMO or mapping.
    pub const fn new() -> Self {
        Self { pinned_vmo: PinnedVmObject::new(), mapping: None }
    }

    /// Initialize with a kernel mapping from the VMO.
    pub fn init(
        &mut self,
        vmo: RefPtr<VmObject>,
        offset: usize,
        size: usize,
        name: &[u8],
    ) -> Result<(), Status> {
        assert!(offset.is_multiple_of(page::SIZE));
        let size = page::round_up(size);
        self.pinned_vmo =
            PinnedVmObject::create(vmo, offset as u64, size as u64, /*write=*/ true)?;
        let root_vmar = VmAspace::kernel_aspace().root_vmar().expect("kernel root vmar");
        let pinned_vmo = self.pinned_vmo.vmo().expect("pinned vmo").clone();
        let map_result = root_vmar.create_vm_mapping(
            0,
            size,
            0,
            vmar_flag::CAN_MAP_READ | vmar_flag::CAN_MAP_WRITE,
            pinned_vmo,
            offset as u64,
            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
            name,
        )?;
        self.mapping = Some(map_result.mapping);
        self.mapping.as_ref().unwrap().map_range(0, size, true, false)
    }

    /// Publish this VMO to userland as a read-only handle using the given name
    /// and stream size.
    pub fn publish(&self, vmo_name: &[u8], stream_size: usize) -> Option<HandleOwner> {
        let vmo = self.pinned_vmo.vmo().expect("pinned vmo");
        let (handle, rights) =
            VmObjectDispatcher::create(vmo, stream_size as u64, InitialMutability::Mutable)
                .expect("VmObjectDispatcher::create");
        let status = handle.dispatcher().set_name(vmo_name);
        debug_assert!(status.is_ok());
        HandleOwner::make(handle, rights & !ZX_RIGHT_WRITE)
    }

    /// Return the bounds of the mapping in the kernel address space.
    pub fn base_locking(&self) -> VAddr {
        self.mapping.as_ref().expect("mapping").base()
    }

    /// Return the size of the mapping in the kernel address space.
    pub fn size_locking(&self) -> usize {
        self.mapping.as_ref().expect("mapping").size()
    }
}

impl Drop for KernelMappedVmo {
    fn drop(&mut self) {
        if let Some(mapping) = self.mapping.take() {
            let _ = mapping.destroy();
            // pinned_vmo's destructor will un-pin the pages just unmapped.
        }
    }
}

/// Kernel unit tests for `KernelMappedVmo`.
#[cfg(ktest)]
#[unittest::suite(name = "kernel_mapped_vmo_tests")]
mod tests {
    use crate::vm::pmm::ALLOC_FLAG_ANY;
    use crate::vm::scanner::AutoVmScannerDisable;
    use crate::vm::vm_object_paged::VmObjectPaged;
    use core::mem::MaybeUninit;
    use unittest::{
        expect_eq, expect_err, expect_ne, expect_ok, expect_true, unwrap_ok, unwrap_some,
    };
    use zx_types::{ZX_MAX_NAME_LEN, ZX_RIGHT_READ, ZX_RIGHT_WRITE};

    /// Tests initializing, writing, publishing, and dropping a `KernelMappedVmo`.
    #[test]
    fn test_kernel_mapped_vmo_init_publish_and_drop() {
        let _scanner_disable = AutoVmScannerDisable::new();

        let paged = unwrap_ok!(VmObjectPaged::create(ALLOC_FLAG_ANY, 0, (2 * page::SIZE) as u64));
        let vmo = VmObjectPaged::into_vm_object(paged);

        let mut mapped = KernelMappedVmo::new();
        let stream_size = 128;
        unwrap_ok!(mapped.init(vmo.clone(), page::SIZE, stream_size, b"test-kernel-mapped-vmo"));

        let base = mapped.base_locking().0;
        expect_ne!(base, 0usize);
        // The mapping size is rounded up to a whole page.
        expect_eq!(mapped.size_locking(), page::SIZE);

        let pattern = [0x5au8; 16];
        // SAFETY: `base` points to `page::SIZE` bytes of pinned, writable kernel memory mapped by
        // `mapped.init`.
        unsafe {
            let dst = core::ptr::with_exposed_provenance_mut::<u8>(base);
            core::ptr::copy_nonoverlapping(pattern.as_ptr(), dst, pattern.len());
        }

        let mut read_buf = [MaybeUninit::<u8>::uninit(); 16];
        let read_slice = unwrap_ok!(vmo.read(page::SIZE as u64, &mut read_buf));
        expect_true!(read_slice == pattern);

        // The mapped page is pinned, so it cannot be decommitted...
        expect_err!(vmo.decommit_range(page::SIZE as u64, page::SIZE as u64), Status::BAD_STATE);

        let handle = unwrap_some!(mapped.publish(b"test-published-vmo", stream_size));
        expect_ne!(handle.rights() & ZX_RIGHT_READ, 0);
        expect_eq!(handle.rights() & ZX_RIGHT_WRITE, 0);

        let mut name_buf = [0u8; ZX_MAX_NAME_LEN];
        vmo.get_name(&mut name_buf);
        expect_true!(name_buf.starts_with(b"test-published-vmo\0"));

        // ...until the `KernelMappedVmo` is dropped, which unmaps and unpins it.
        drop(mapped);
        expect_ok!(vmo.decommit_range(page::SIZE as u64, page::SIZE as u64));
        drop(handle);
    }

    /// Tests that a failed `init` leaves the `KernelMappedVmo` empty and droppable.
    #[test]
    fn test_kernel_mapped_vmo_init_out_of_range() {
        let _scanner_disable = AutoVmScannerDisable::new();

        let paged = unwrap_ok!(VmObjectPaged::create(ALLOC_FLAG_ANY, 0, page::SIZE as u64));
        let vmo = VmObjectPaged::into_vm_object(paged);

        let mut mapped = KernelMappedVmo::new();
        expect_err!(
            mapped.init(vmo.clone(), page::SIZE, page::SIZE, b"test-out-of-range"),
            Status::OUT_OF_RANGE
        );
        expect_true!(mapped.mapping.is_none());
        expect_true!(mapped.pinned_vmo.vmo().is_none());
        drop(mapped);
    }

    /// Tests that a never-initialized `KernelMappedVmo` can be dropped, as C++ owners may do.
    #[test]
    fn test_kernel_mapped_vmo_drop_uninitialized() {
        let mapped = KernelMappedVmo::new();
        drop(mapped);
    }
}
