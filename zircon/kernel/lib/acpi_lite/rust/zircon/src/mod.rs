// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use unittest as _;

use crate::kernel::types::PAddr;
use crate::vm::arch_vm_aspace::ARCH_MMU_FLAG_PERM_READ;
use crate::vm::vm_address_region as vmar;
use crate::vm::vm_aspace::VmAspace;
use crate::vm::vm_mapping::VmMapping;
use crate::vm::vm_object_physical::VmObjectPhysical;
use acpi_lite::structures::AcpiSdtHeader;
use acpi_lite::{AcpiParser, AcpiParserInterface, PhysMemReader};
use core::mem::MaybeUninit;
use core::ptr::NonNull;
use fbl::{RefPtr, SinglyLinkedList, SinglyLinkedListContainable, SinglyLinkedListNode, UniquePtr};
use ksync::{KMutex, guarded, lock};
use pin_init::pin_init;
use zx_status::Status;

#[derive(fbl::Recyclable, SinglyLinkedListContainable)]
pub struct Mapping {
    mapping: RefPtr<VmMapping>,
    #[sll_node]
    node: SinglyLinkedListNode<Mapping>,
}

impl Mapping {
    pub fn new(mapping: RefPtr<VmMapping>) -> Result<UniquePtr<Self>, kalloc::AllocError> {
        UniquePtr::try_new(Self { mapping, node: SinglyLinkedListNode::new() })
    }
}

#[guarded]
pub struct ZirconPhysmemReader {
    #[mutex]
    mutex: KMutex,

    #[guarded_by(mutex)]
    mappings: SinglyLinkedList<UniquePtr<Mapping>>,
}

impl ZirconPhysmemReader {
    pub fn init() -> impl pin_init::PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            mutex <- KMutex::init(),
            mappings: SinglyLinkedList::new().into(),
        })
    }
}

impl PhysMemReader for ZirconPhysmemReader {
    fn phys_to_slice(&self, phys: usize, length: usize) -> Result<&[u8], Status> {
        if length == 0 || phys == 0 {
            return Err(Status::INVALID_ARGS);
        }
        if phys > usize::MAX - length {
            return Err(Status::OUT_OF_RANGE);
        }

        let paddr_base = phys & !page::MASK;
        let size = (phys + length - 1).next_multiple_of(page::SIZE) - paddr_base;

        lock!(let mut guard = self.lock_mutex());
        let fields = guard.as_mut().fields_mut();

        let kernel_aspace = VmAspace::kernel_aspace();
        let arch_aspace = kernel_aspace.arch_aspace();

        for mapping_node in fields.mappings.iter() {
            let mapping = &mapping_node.mapping;
            let (map_paddr, _mmu_flags) = {
                let res = arch_aspace.query(mapping.base())?;
                (res.0.0, res.1)
            };
            if map_paddr <= paddr_base && paddr_base + size <= map_paddr + mapping.size() {
                let offset = phys - map_paddr;
                let ptr = core::ptr::with_exposed_provenance::<u8>(mapping.base() + offset);
                let slice = unsafe { core::slice::from_raw_parts(ptr, length) };
                return Ok(slice);
            }
        }

        let vmo = VmObjectPhysical::create(PAddr(paddr_base), size)?;
        let vmo = unsafe {
            RefPtr::from_raw(
                VmObjectPhysical::cast(NonNull::new_unchecked(
                    RefPtr::into_raw(vmo) as *mut VmObjectPhysical
                ))
                .as_ptr(),
            )
        };

        let root_vmar = kernel_aspace.root_vmar().ok_or(Status::BAD_STATE)?;

        let map_result = root_vmar.create_vm_mapping(
            0,
            size,
            0,
            vmar::flag::CAN_MAP_READ,
            vmo,
            0,
            ARCH_MMU_FLAG_PERM_READ,
            c"acpi",
        )?;

        if let Err(err) = map_result.mapping.map_range(0, size, true, false) {
            let _ = map_result.mapping.destroy();
            return Err(err);
        }

        let pl = Mapping::new(map_result.mapping.clone()).map_err(|_| {
            let _ = map_result.mapping.destroy();
            Status::NO_MEMORY
        })?;

        fields.mappings.push_front(pl);

        let offset = phys - paddr_base;
        let ptr = core::ptr::with_exposed_provenance::<u8>(map_result.base + offset);
        let slice = unsafe { core::slice::from_raw_parts(ptr, length) };
        Ok(slice)
    }
}

static mut READER: MaybeUninit<ZirconPhysmemReader> = MaybeUninit::uninit();

/// Initialize the global `READER` instance and return an `AcpiParser` using it. May only be called
/// once (even on error).
///
/// # Safety
///
/// This caller guarantees that this method has not already been called.
#[allow(static_mut_refs)]
pub unsafe fn acpi_parser_init(rsdp_pa: PAddr) -> Result<AcpiParser<'static>, Status> {
    // As this method may only be called once we know that both that READER is not yet initialized,
    // and that there are no other references (mutable or otherwise) to it.
    unsafe {
        let _ = pin_init::PinInit::__pinned_init(ZirconPhysmemReader::init(), READER.as_mut_ptr());
        AcpiParser::init(READER.assume_init_mut(), rsdp_pa.0)
    }
}

zr::static_assert!(core::mem::size_of::<AcpiParser<'static>>() == 56);
zr::static_assert!(core::mem::align_of::<AcpiParser<'static>>() == 8);

#[unsafe(no_mangle)]
pub extern "C" fn rust_acpi_parser_init(
    rsdp_pa: PAddr,
    out_parser: *mut AcpiParser<'static>,
) -> Status {
    match unsafe { acpi_parser_init(rsdp_pa) } {
        Ok(parser) => {
            unsafe { out_parser.write(parser) };
            Status::OK
        }
        Err(e) => e,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_acpi_parser_dump_tables(parser: *const AcpiParser<'static>) {
    unsafe { parser.as_ref_unchecked() }.dump_tables();
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_acpi_parser_num_tables(parser: *const AcpiParser<'static>) -> usize {
    unsafe { parser.as_ref_unchecked() }.num_tables()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_acpi_parser_get_table_at_index(
    parser: *const AcpiParser<'static>,
    index: usize,
) -> *const AcpiSdtHeader {
    unsafe { parser.as_ref_unchecked() }
        .get_table_at_index(index)
        .map(|x| x as *const AcpiSdtHeader)
        .unwrap_or(core::ptr::null())
}

/// ACPI Lite Zircon tests.
#[cfg(ktest)]
#[unittest::suite]
mod acpi_lite_zircon_tests {
    use unittest::{assert_true, expect_eq, expect_true};

    /// Test ZirconPhysmemReader methods in kernel context.
    //
    // This duplicates a test in the acpi_lite test suite, but we repeat it here as a
    // basic check to ensure acpi_lite functions in a kernel context.
    #[test]
    fn test_zircon_physmem_reader() {
        pin_init::stack_pin_init!(let reader = ZirconPhysmemReader::init());

        expect_true!(reader.phys_to_slice(0, 0).is_err());
        expect_true!(reader.phys_to_slice(0, 1).is_err());
        expect_true!(reader.phys_to_slice(1, 0).is_err());

        static TEST_TOKEN: u64 = 0xabcd_1234_dead_beef;
        let paddr =
            crate::vm::vm::vaddr_to_paddr(&TEST_TOKEN as *const u64 as *const core::ffi::c_void);
        if paddr.0 != 0 {
            let res = reader.phys_to_slice(paddr.0, core::mem::size_of::<u64>());
            assert_true!(res.is_ok());
            if let Ok(slice) = res {
                expect_eq!(slice.len(), 8);
                let val = u64::from_ne_bytes(slice.try_into().unwrap());
                expect_eq!(val, TEST_TOKEN);
            }
        }

        expect_true!(reader.phys_to_slice(usize::MAX, 1).is_err());
        if paddr.0 != 0 {
            expect_true!(reader.phys_to_slice(paddr.0 + 2, usize::MAX).is_err());
        }
    }

    /// Test parsing system ACPI tables.
    //
    // We don't really know what we will find (auto-detection might legitimately fail,
    // for example), but we just try and exercise the code.
    #[test]
    fn test_parse_system() {
        #[allow(static_mut_refs)]
        let res = unsafe { AcpiParser::init(READER.assume_init_mut(), 0) };
        if let Ok(_parser) = res {
            unsafe {
                unittest::printf(c"Successfully parsed the current system's tables.\n".as_ptr()
                    as *const core::ffi::c_char);
            }
        } else {
            unsafe {
                unittest::printf(c"Could not parse the current system's tables.\n".as_ptr()
                    as *const core::ffi::c_char);
            }
        }
    }
}
