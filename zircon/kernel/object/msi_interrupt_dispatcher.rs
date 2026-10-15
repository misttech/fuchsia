// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_void;
use core::mem::size_of;
use core::ptr::{NonNull, fn_addr_eq, read_volatile, with_exposed_provenance_mut, write_volatile};
use debug::ltracef;
use fbl::RefPtr;
use kprint::kformat;
use ksync::lock;
use zx_status::Status;
use zx_types::{
    ZX_CACHE_POLICY_UNCACHED_DEVICE, ZX_MAX_NAME_LEN, ZX_MSI_MODE_MSI_X, zx_off_t, zx_paddr_t,
    zx_rights_t,
};

use super::handle::KernelHandle;
use super::interrupt_dispatcher::{
    INTERRUPT_MASK_POSTWAIT, INTERRUPT_UNMASK_PREWAIT, InterruptDispatcher, InterruptKind,
    WakeVectorDiagnostics,
};
use super::msi_allocation::MsiAllocation;
use crate::counters::define_kcounter;
use crate::dev_interrupt::{
    InterruptHandler, MsiBlock, msi_mask_unmask, msi_register_handler, msi_supports_masking,
};
use crate::vm::arch_vm_aspace::{ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE};
use crate::vm::vm_aspace::VmAspace;
use crate::vm::vm_mapping::VmMapping;
use crate::vm::vm_object::VmObject;
use libarch::device_memory_barrier;

const LOCAL_TRACE: u32 = 0;

define_kcounter!(DISPATCHER_MSI_CREATE_COUNT, "msi_dispatcher.create", Sum);
define_kcounter!(DISPATCHER_MSI_INTERRUPT_COUNT, "msi_dispatcher.interrupts", Sum);
define_kcounter!(DISPATCHER_MSI_MASK_COUNT, "msi_dispatcher.mask", Sum);
define_kcounter!(DISPATCHER_MSI_UNMASK_COUNT, "msi_dispatcher.unmask", Sum);
define_kcounter!(DISPATCHER_MSI_DESTROY_COUNT, "msi_dispatcher.destroy", Sum);

type RegisterIntFn = unsafe extern "C" fn(*const MsiBlock, u32, InterruptHandler);

// MSI / MSI-X interrupts share a masking approach and should be masked while
// being serviced and unmasked while waiting for an interrupt message to arrive.
const FLAGS: u32 = INTERRUPT_UNMASK_PREWAIT | INTERRUPT_MASK_POSTWAIT;

// Message Signaled Interrupts --
// This derived interrupt dispatcher handles operation of Messaged Signaled
// Interrupts (MSIs) and their associated interactions with userspace drivers.
// MSIs are allocated at the platform interrupt controller in contiguous blocks
// and then assigned as a group to a given PCI device. A PCI device may support
// 1 or more interrupts, and may or may not support masking of individual
// vectors. Operation of the MSI functionality is largely controlled in the
// device's capability space via its MSI Capability Structure. This includes
// enabling MSI, configuring vectors, and masking/unmasking vectors. To reduce
// interrupt latency all masking and unmasking at interrupt time is handled by
// this dispatcher, but all configuration is (will be: https://fxbug.dev/42108122) handled by the
// userspace PCI Bus Driver. To facilitate safe interactions between the two,
// all access to MSI configuration registers are synchronized via the MSI
// allocation lock. Userspace will rarely be accessing this outside of
// initialization so the performance overhead is minimal due to a lack of
// congestion and interrupts all being handled by the bootstrap CPU.
// Since the dispatcher only needs access to the Id register and Mask Bits register
// we are fortunately able to ignore the different formats due to the 32 bit and 64
// bit mask registers lining up.
// PCI Local Bus Specification rev 3.0 figure 6-9.
const MSI_CAPABILITY_ID: u8 = 0x5;
const MSI_64BIT_SUPPORTED: u16 = 1 << 7;
const MSI_PVM_SUPPORTED: u16 = 1 << 8;

#[repr(C)]
struct MsiCapability {
    id: u8,
    reserved0: u8, // Next pointer
    control: u16,
    reserved1: [u32; 2], // For 32 bit this is Address, Data, and a reserved field.
    // For 64 bit this is Address and Address Upper.
    mask_bits_32: u32, // For 64 bit this is Data and a reserved field.
    mask_bits_64: u32,
    reserved2: u32, // Pending Bits
}

zr::static_assert!(core::mem::offset_of!(MsiCapability, control) == 0x02);
zr::static_assert!(core::mem::offset_of!(MsiCapability, mask_bits_32) == 0x0C);
zr::static_assert!(core::mem::offset_of!(MsiCapability, mask_bits_64) == 0x10);
zr::static_assert_size_and_align!(MsiCapability, 24, 4);

// For MSI-X, the kernel only needs to interact with a given table entry for a specific vector.
// Furthermore, since each vector has its own entry and MsiInterruptDispatchers hold a reference to
// their allocation there is no need to lock any of the accesses. If the PCI bus driver wishes
// to disable MSI-X on the device function then it can do so with the function level disable
// in the capability before tearing down any interrupts.
//
// MSI-X table entries are covered in the PCI Local Bus Spec v3.0 section 6.8.2.7
#[repr(C)]
struct MsixTableEntry {
    msg_addr: u32,
    msg_upper_addr: u32,
    msg_data: u32,
    vector_control: u32,
}

zr::static_assert_size_and_align!(MsixTableEntry, 16, 4);

const MSIX_VECTOR_CONTROL_MASK_BIT: u32 = 0;

const fn msix_table_offset(id: u32) -> usize {
    id as usize * size_of::<MsixTableEntry>()
}

#[repr(C)]
enum MsiVariant {
    Msi(MsiInterruptDispatcherImpl),
    Msix(MsixInterruptDispatcherImpl),
}

/// The common interface for all MSI related interrupt handling. This encompasses MSI and MSI-X.
pub struct MsiInterruptDispatcher {
    disp: NonNull<InterruptDispatcher>,
    variant: MsiVariant,
    // The config space of the MSI capability controlling this MSI vector.
    mapping: RefPtr<VmMapping>,
    // The MSI allocation block this dispatcher shares.
    alloc: RefPtr<MsiAllocation>,
    // A pointer to the function to register the msi interrupt handler. Allows
    // tests to override msi_register_handler.
    register_int_fn: RegisterIntFn,
    // The specific MSI id within the block that this dispatcher services.
    msi_id: u32,
}

// SAFETY: `MsiInterruptDispatcher` is owned inline by `InterruptDispatcher`. `disp` points to the
// thread-safe enclosing `InterruptDispatcher` (`Send + Sync`), `MsiCapability` register accesses in
// `MsiInterruptDispatcherImpl` are synchronized by `self.alloc.lock()`, and `MsixTableEntry`
// accesses in `MsixInterruptDispatcherImpl` are isolated to the exclusively reserved `self.msi_id`
// entry while `self.mapping` is alive.
unsafe impl Send for MsiInterruptDispatcher {}
// SAFETY: Shared references only access immutable fields or synchronize MMIO register accesses via
// `self.alloc.lock()` (MSI) or exclusive per-vector `self.msi_id` reservation (MSI-X).
unsafe impl Sync for MsiInterruptDispatcher {}

impl Drop for MsiInterruptDispatcher {
    fn drop(&mut self) {
        // Run derived MSI-X cleanup first (matching C++ `~MsixInterruptDispatcherImpl()` before
        // `~MsiInterruptDispatcher()`) while `self.msi_id` is still reserved in `self.alloc`
        // and `self.mapping` is still alive.
        if let MsiVariant::Msix(msix) = &mut self.variant {
            msix.cleanup();
        }

        let st = self.alloc.release_id(self.msi_id);
        if let Err(st) = st {
            ltracef!(
                "MsiInterruptDispatcher: Failed to release MSI id {} (vector {}): {}\n",
                self.msi_id,
                self.vector(),
                st.into_raw()
            );
        }
        ltracef!("MsiInterruptDispatcher: cleaning up MSI id {}\n", self.msi_id);
        DISPATCHER_MSI_DESTROY_COUNT.add(1);
        // SAFETY: `self.disp` points to the enclosing `InterruptDispatcher`, which is valid
        // during `~InterruptDispatcher`.
        unsafe { self.disp.as_ref() }.destroy_wake_event();
    }
}

impl MsiInterruptDispatcher {
    /// Creates a derived `MsiInterruptDispatcher` determined by the flags passed in.
    pub fn create(
        alloc: RefPtr<MsiAllocation>,
        msi_id: u32,
        vmo: &RefPtr<VmObject>,
        vmo_offset: zx_paddr_t,
        mut options: u32,
        register_int_fn: Option<RegisterIntFn>,
    ) -> Result<(KernelHandle<InterruptDispatcher>, zx_rights_t), Status> {
        let register_int_fn = register_int_fn.unwrap_or(msi_register_handler);
        ltracef!(
            "vmo: {}, {}, {}\nsize = {}, vmo_offset = {}, options = {:#x}, cache policy = {}\n",
            if vmo.is_paged() { "paged" } else { "physical" },
            if vmo.is_contiguous() { "contiguous" } else { "not contiguous" },
            if vmo.is_resizable() { "resizable" } else { "not resizable" },
            vmo.size(),
            vmo_offset,
            options,
            vmo.get_mapping_cache_policy()
        );

        let is_msix = (options & ZX_MSI_MODE_MSI_X) == ZX_MSI_MODE_MSI_X;
        options &= !ZX_MSI_MODE_MSI_X;

        if vmo.is_paged()
            || !vmo.is_contiguous()
            || vmo_offset as u64 >= vmo.size()
            || options != 0
            || u32::from(vmo.get_mapping_cache_policy()) != ZX_CACHE_POLICY_UNCACHED_DEVICE
        {
            return Err(Status::INVALID_ARGS);
        }

        if msi_id >= alloc.block().num_irq {
            ltracef!(
                "msi_id {} is out of range for the block (num_irqs: {})\n",
                msi_id,
                alloc.block().num_irq
            );
            return Err(Status::INVALID_ARGS);
        }
        let base_irq_id = alloc.block().base_irq_id;

        if let Err(status) = alloc.reserve_id(msi_id) {
            ltracef!("failed to reserve msi_id {}: {}\n", msi_id, status.into_raw());
            return Err(status);
        }
        let alloc_cleanup = alloc.clone();
        let mut cleanup = zr::defer(move || {
            let _ = alloc_cleanup.release_id(msi_id);
        });

        // To handle MSI masking we need to create a kernel mapping for the VMO handed
        // to us, this will provide access to the register controlling the given MSI.
        // The VMO must be a contiguous VMO with the cache policy already configured.
        // Size checks will come into play when we know what type of capability we're
        // working with.
        let vmar = VmAspace::kernel_aspace()
            .root_vmar()
            .expect("kernel aspace root VMAR must be initialized");
        let vector = base_irq_id + msi_id;
        let mut name = [0u8; ZX_MAX_NAME_LEN];
        let name = kformat!(&mut name, "msi id {:u} (vector {:u})", msi_id, vector);
        let mapping_result = match vmar.create_vm_mapping(
            0,
            vmo.size() as usize,
            0,
            0,
            vmo.clone(),
            0,
            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
            name,
        ) {
            Ok(res) => res,
            Err(status) => {
                ltracef!("Failed to create MSI mapping: {}\n", status.into_raw());
                return Err(status);
            }
        };
        let mapping = mapping_result.mapping;

        if let Err(status) = mapping.map_range(0, vmo.size() as usize, true, false) {
            ltracef!("Falled to MapRange for the mapping: {}\n", status.into_raw());
            return Err(status);
        }

        ltracef!(
            "Mapping mapped at {:#x}, size {:x}, vmo size {:x}, vmo_offset = {:#x}\n",
            mapping_result.base,
            mapping.size(),
            vmo.size(),
            vmo_offset
        );

        // MSI lives inside a device's config space within an MSI Capability. MSI-X has a similar
        // capability, but has another table mapped elsewhere which contains individually maskable
        // bits per vector. The capability itself is managed by the PCI bus driver, and the mask
        // bits are handled by this dispatcher. So in the event of MSI-X there is no capability id
        // to check, since we don't touch the capability at all at this level.
        let disp = if is_msix {
            // Most validation for MSI-X is done in the PCI driver since it can confirm that the
            // Table Structure is appropriately large for the number of interrupts, and the
            // allocation by now has already been made.
            if vmo_offset
                .checked_add(msix_table_offset(msi_id + 1))
                .is_none_or(|add_result| add_result as u64 > vmo.size())
            {
                return Err(Status::INVALID_ARGS);
            }

            InterruptDispatcher::create_raw(FLAGS, 0, |disp| {
                let variant = MsiVariant::Msix(MsixInterruptDispatcherImpl::new(
                    &alloc,
                    msi_id,
                    &mapping,
                    vmo_offset as zx_off_t,
                ));
                Self::new(disp, alloc, mapping, msi_id, register_int_fn, variant);
            })
        } else {
            // MSI capabilities fit within a given device's configuration space which is either 256
            // or 4096 bytes. But in most cases the VMO containing config space is going to be at
            // least the size of a full PCI bus's worth of devices, and physical VMOs cannot be
            // sliced into children. We can validate that the capability fits within the offset
            // given, but otherwise cannot rely on the VMO's size for validation.
            if vmo_offset
                .checked_add(size_of::<MsiCapability>())
                .is_none_or(|add_result| add_result as u64 > vmo.size())
            {
                return Err(Status::INVALID_ARGS);
            }

            let cap =
                with_exposed_provenance_mut::<MsiCapability>(mapping_result.base + vmo_offset);
            // SAFETY: `vmo_offset + size_of::<MsiCapability>() <= vmo.size()` was verified above.
            if unsafe { read_volatile(&raw const (*cap).id) } != MSI_CAPABILITY_ID {
                return Err(Status::INVALID_ARGS);
            }

            // SAFETY: `vmo_offset + size_of::<MsiCapability>() <= vmo.size()` was verified above.
            let ctrl_val: u16 = unsafe { read_volatile(&raw const (*cap).control) };
            let has_pvm = (ctrl_val & MSI_PVM_SUPPORTED) != 0;
            let has_64bit = (ctrl_val & MSI_64BIT_SUPPORTED) != 0;
            InterruptDispatcher::create_raw(FLAGS, 0, |disp| {
                let variant = MsiVariant::Msi(MsiInterruptDispatcherImpl::new(
                    &mapping,
                    vmo_offset as zx_off_t,
                    has_pvm,
                    has_64bit,
                    register_int_fn,
                ));
                Self::new(disp, alloc, mapping, msi_id, register_int_fn, variant);
            })
        };

        let Some(disp) = disp else {
            ltracef!("Failed to allocate MsiInterruptDispatcher\n");
            return Err(Status::NO_MEMORY);
        };
        // If we allocated MsiInterruptDispatcher successfully then its dtor will release
        // the id if necessary.
        cleanup.cancel();

        let InterruptKind::Msi(msi) = disp.state().kind() else {
            unreachable!();
        };
        if let Err(status) = msi.register_interrupt_handler() {
            ltracef!(
                "Failed to register interrupt handler for msi id {} (vector {}): {}\n",
                msi_id,
                vector,
                status.into_raw()
            );
            return Err(status);
        }
        msi.unmask_interrupt();

        let rights = InterruptDispatcher::default_rights();
        let handle = KernelHandle::new(disp);
        ltracef!("MsiInterruptDispatcher successfully created.\n");
        Ok((handle, rights))
    }

    #[allow(clippy::new_ret_no_self)]
    fn new(
        disp: &InterruptDispatcher,
        alloc: RefPtr<MsiAllocation>,
        mapping: RefPtr<VmMapping>,
        msi_id: u32,
        register_int_fn: RegisterIntFn,
        variant: MsiVariant,
    ) {
        // SAFETY: Called once during `InterruptDispatcher::create_raw` construction.
        unsafe {
            disp.init_kind(InterruptKind::Msi(Self {
                disp: NonNull::from(disp),
                variant,
                mapping,
                alloc,
                register_int_fn,
                msi_id,
            }));
        }
        DISPATCHER_MSI_CREATE_COUNT.add(1);
        disp.initialize_wake_event();
    }

    fn vector(&self) -> u32 {
        self.alloc.block().base_irq_id + self.msi_id
    }

    /// Masks the MSI or MSI-X interrupt vector.
    pub fn mask_interrupt(&self) {
        match &self.variant {
            MsiVariant::Msi(m) => m.mask_interrupt(&self.alloc, self.msi_id),
            MsiVariant::Msix(m) => m.mask_interrupt(),
        }
    }

    /// Unmasks the MSI or MSI-X interrupt vector.
    pub fn unmask_interrupt(&self) {
        match &self.variant {
            MsiVariant::Msi(m) => m.unmask_interrupt(&self.alloc, self.msi_id),
            MsiVariant::Msix(m) => m.unmask_interrupt(),
        }
    }

    /// Unregisters the MSI interrupt handler.
    pub fn unregister_interrupt_handler(&self) {
        // SAFETY: `self.alloc.block()` is valid and `InterruptHandler::DEFAULT` unregisters.
        unsafe {
            (self.register_int_fn)(self.alloc.block(), self.msi_id, InterruptHandler::DEFAULT);
        }
    }

    fn register_interrupt_handler(&self) -> Result<(), Status> {
        // SAFETY: `self.alloc.block()` and `self.disp` remain valid until
        // `unregister_interrupt_handler` is called.
        unsafe {
            (self.register_int_fn)(
                self.alloc.block(),
                self.msi_id,
                InterruptHandler {
                    cookie: self.disp.as_ptr().cast(),
                    r#fn: Some({
                        extern "C" fn handler(cookie: *mut c_void) {
                            // SAFETY: `cookie` is a valid `*const InterruptDispatcher`.
                            let this = unsafe { &*(cookie as *const InterruptDispatcher) };
                            this.interrupt_handler();
                            DISPATCHER_MSI_INTERRUPT_COUNT.add(1);
                        }
                        handler
                    }),
                },
            );
        }
        Ok(())
    }

    // This override of WakeVector::GetDiagnostics is marked final to prevent further overrides.
    // Because this method cannot be overridden further, it is safe for this class to initialize /
    // destroy InterruptDispatcher::wake_event in the constructor / destructor. See
    // lib/wake-vector.h for more details.
    /// Populates wake vector diagnostics for this MSI interrupt.
    pub fn get_diagnostics(&self, diagnostics_out: &mut WakeVectorDiagnostics) {
        // SAFETY: `self.disp` points to the valid enclosing `InterruptDispatcher`.
        let disp = unsafe { self.disp.as_ref() };
        diagnostics_out.enabled = disp.is_wake_vector();
        diagnostics_out.koid = disp.get_koid();
        diagnostics_out.print_extra(format_args!("MSI {}", self.vector()));
    }
}

struct MsiInterruptDispatcherImpl {
    // Not all interrupt controllers / configurations support masking at the
    // platform level. This is set accordingly if support is detected.
    has_platform_pvm: bool,
    // Whether or not the given device function supports per vector masking within
    // the PCI MSI capability.
    has_cap_pvm: bool,
    // A pointer to the MSI Mask Register for this specific device function. To
    // synchronize with other MSI interactions in the device it requires the MSI
    // allocation's lock.
    mask_reg: NonNull<u32>,
}

impl MsiInterruptDispatcherImpl {
    fn new(
        mapping: &RefPtr<VmMapping>,
        reg_offset: zx_off_t,
        has_cap_pvm: bool,
        has_64bit: bool,
        register_int_fn: RegisterIntFn,
    ) -> Self {
        let has_platform_pvm = fn_addr_eq(register_int_fn, msi_register_handler as RegisterIntFn)
            && msi_supports_masking();
        let cap =
            with_exposed_provenance_mut::<MsiCapability>(mapping.base().0 + reg_offset as usize);
        // SAFETY: `cap` points to a mapped `MsiCapability` region (validated in `create`).
        let mask_reg_ptr = unsafe {
            if has_64bit { &raw mut (*cap).mask_bits_64 } else { &raw mut (*cap).mask_bits_32 }
        };
        let mask_reg =
            NonNull::new(mask_reg_ptr).expect("kernel VmMapping address must be non-null");
        Self { has_platform_pvm, has_cap_pvm, mask_reg }
    }

    fn mask_interrupt(&self, alloc: &MsiAllocation, msi_id: u32) {
        DISPATCHER_MSI_MASK_COUNT.add(1);

        lock!(let _guard = alloc.lock().lock());
        if self.has_platform_pvm {
            msi_mask_unmask(alloc.block(), msi_id, true);
        }

        if self.has_cap_pvm {
            let mask: u32 = 1 << msi_id;
            let mask_reg = self.mask_reg.as_ptr();
            // SAFETY: `mask_reg` is a valid mapped `MsiCapability` mask register pointer
            // synchronized by `alloc.lock()` held via `_guard` above.
            unsafe {
                write_volatile(mask_reg, read_volatile(mask_reg) | mask);
            }
            device_memory_barrier();
        }
    }

    fn unmask_interrupt(&self, alloc: &MsiAllocation, msi_id: u32) {
        DISPATCHER_MSI_UNMASK_COUNT.add(1);

        if self.has_platform_pvm {
            msi_mask_unmask(alloc.block(), msi_id, false);
        }

        if self.has_cap_pvm {
            lock!(let _guard = alloc.lock().lock());
            let mask: u32 = !(1 << msi_id);
            let mask_reg = self.mask_reg.as_ptr();
            // SAFETY: `mask_reg` is a valid mapped `MsiCapability` mask register pointer
            // synchronized by `alloc.lock()` held via `_guard` above.
            unsafe {
                write_volatile(mask_reg, read_volatile(mask_reg) & mask);
            }
            device_memory_barrier();
        }
    }
}

struct MsixInterruptDispatcherImpl {
    entry: NonNull<MsixTableEntry>,
}

impl MsixInterruptDispatcherImpl {
    fn new(
        alloc: &MsiAllocation,
        msi_id: u32,
        mapping: &RefPtr<VmMapping>,
        table_offset: zx_off_t,
    ) -> Self {
        let table_entries =
            with_exposed_provenance_mut::<MsixTableEntry>(mapping.base().0 + table_offset as usize);
        let entry = NonNull::new(table_entries.wrapping_add(msi_id as usize))
            .expect("kernel VmMapping address must be non-null");
        let this = Self { entry };

        // Disable the vector, set up the address and data registers, then re-enable
        // it for our given msi_id. Per PCI Local Bus Spec v3 section 6.8.2
        // implementation notes, all accesses to these registers must be DWORD or
        // QWORD only. We write upper and lower halves of the address unconditionally
        // because if the address is 32 bits then we want to write zeroes to the upper
        // half regardless. The msg_data field is incremented by msi_id because unlike
        // MSI, MSI-X does not adjust the data payload. This allows us to point
        // multiple table entries at the same vector, but requires us to specify the
        // vector in the data field.
        this.mask_interrupt();
        let tgt_addr = alloc.block().tgt_addr;
        let tgt_data = alloc.block().tgt_data;
        // SAFETY: `this.entry` points to the mapped `msi_id` entry (validated in `create`).
        unsafe {
            let entry = this.entry.as_ptr();
            write_volatile(&raw mut (*entry).msg_addr, (tgt_addr & u32::MAX as u64) as u32);
            write_volatile(&raw mut (*entry).msg_upper_addr, (tgt_addr >> 32) as u32);
            write_volatile(&raw mut (*entry).msg_data, tgt_data + msi_id);
        }
        device_memory_barrier();
        this
    }

    fn mask_interrupt(&self) {
        DISPATCHER_MSI_MASK_COUNT.add(1);
        // SAFETY: `self.entry` points to the mapped `MsixTableEntry` for this vector.
        unsafe {
            let reg_ptr = &raw mut (*self.entry.as_ptr()).vector_control;
            let mut reg = read_volatile(reg_ptr);
            reg |= 1u32 << MSIX_VECTOR_CONTROL_MASK_BIT;
            write_volatile(reg_ptr, reg);
        }
        device_memory_barrier();
    }

    fn unmask_interrupt(&self) {
        DISPATCHER_MSI_UNMASK_COUNT.add(1);
        // SAFETY: `self.entry` points to the mapped `MsixTableEntry` for this vector.
        unsafe {
            let reg_ptr = &raw mut (*self.entry.as_ptr()).vector_control;
            let mut reg = read_volatile(reg_ptr);
            reg &= !(1u32 << MSIX_VECTOR_CONTROL_MASK_BIT);
            write_volatile(reg_ptr, reg);
        }
        device_memory_barrier();
    }

    // Performs the inverse of the configuration set up in the constructor. Mask the
    // interrupt and clear out the table entry.
    fn cleanup(&mut self) {
        self.mask_interrupt();
        // SAFETY: `self.entry` points to the mapped `MsixTableEntry` while
        // `MsiInterruptDispatcher::mapping` is still alive and `msi_id` is still reserved.
        unsafe {
            let entry = self.entry.as_ptr();
            write_volatile(&raw mut (*entry).msg_addr, 0);
            write_volatile(&raw mut (*entry).msg_upper_addr, 0);
            write_volatile(&raw mut (*entry).msg_data, 0);
        }
        device_memory_barrier();
    }
}

/// Tests for MSI objects
#[cfg(ktest)]
#[unittest::suite(name = "msi")]
mod tests {
    use core::mem::size_of;
    use core::ptr::{NonNull, read_volatile, with_exposed_provenance_mut, write_volatile};
    use core::sync::atomic::{AtomicU32, Ordering};
    use fbl::RefPtr;
    use kprint::kprint;
    use ksync::lock;
    use unittest::{assert_eq, assert_err, assert_ok, assert_true, expect_eq, unwrap_ok};
    use zx_status::Status;

    use super::super::msi_allocation::MSI_ALLOCATION_COUNT_MAX;
    use super::{
        InterruptHandler, MSI_64BIT_SUPPORTED, MSI_CAPABILITY_ID, MSI_PVM_SUPPORTED, MsiAllocation,
        MsiBlock, MsiCapability, MsiInterruptDispatcher, VmAspace, VmMapping, VmObject,
    };
    use crate::kernel::types::PAddr;
    use crate::vm::arch_vm_aspace::{
        ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE, ARCH_MMU_FLAG_UNCACHED_DEVICE,
    };
    use crate::vm::pmm;
    use crate::vm::vm_object_paged::VmObjectPaged;
    use crate::vm::vm_object_physical::VmObjectPhysical;
    use boot_options::BootOptions;

    fn msi_is_supported_true() -> bool {
        true
    }

    fn msi_allocate(
        requested_irqs: u32,
        _can_target_64bit: bool,
        _is_msix: bool,
        out_block: &mut MsiBlock,
    ) -> Result<(), Status> {
        out_block.allocated = true;
        out_block.base_irq_id = 128;
        out_block.num_irq = requested_irqs;
        out_block.tgt_addr = 0x4321;
        out_block.is_32bit = false;
        Ok(())
    }

    // These functions are used to verify we properly bail out if MSI isn't supported.
    fn msi_free(block: &mut MsiBlock) {
        block.allocated = false;
    }

    fn msi_is_supported_false() -> bool {
        false
    }

    fn msi_allocate_assert(
        _requested_irqs: u32,
        _can_target_64bit: bool,
        _is_msix: bool,
        _out_block: &mut MsiBlock,
    ) -> Result<(), Status> {
        unreachable!();
    }

    fn msi_free_assert(_block: &mut MsiBlock) {
        unreachable!();
    }

    fn create_allocation(cnt: u32) -> Result<RefPtr<MsiAllocation>, Status> {
        MsiAllocation::create_with_fns(cnt, msi_allocate, msi_free, msi_is_supported_true)
    }

    type ValidMsiVmo = (RefPtr<VmObject>, RefPtr<VmMapping>, NonNull<MsiCapability>);

    // Helper function to create a valid vmo / mapping / capability tuple to cut down
    // on the duplication within tests.
    fn create_valid_msi_vmo() -> Result<ValidMsiVmo, Status> {
        let boot_options::AutoOr::Some(reserve) = &BootOptions::get().test_ram_reserve else {
            return Err(Status::NOT_SUPPORTED);
        };
        let boot_options::AutoOr::Some(paddr) = reserve.paddr else {
            return Err(Status::NOT_SUPPORTED);
        };

        let vmo_size = page::SIZE;
        let vmo = VmObjectPhysical::create(PAddr(paddr as usize), vmo_size)?;
        let vmo = VmObjectPhysical::into_vm_object(vmo);
        // SAFETY: `vmo` has no mappings yet.
        unsafe {
            vmo.set_mapping_cache_policy(ARCH_MMU_FLAG_UNCACHED_DEVICE)?;
        }

        let mapping_result =
            VmAspace::kernel_aspace().root_vmar().ok_or(Status::INTERNAL)?.create_vm_mapping(
                /*mapping_offset=*/ 0,
                /*size=*/ vmo_size,
                /*align_pow2=*/ 0,
                /*vmar_flags=*/ 0,
                vmo.clone(),
                /*vmo_offset=*/ 0,
                ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
                /*name=*/ b"",
            )?;

        // Prepopulate the mapping so no page faults are needed in kernel mode.
        mapping_result
            .mapping
            .map_range(/*offset=*/ 0, vmo_size, /*commit=*/ true, false)?;

        let cap = NonNull::new(with_exposed_provenance_mut::<MsiCapability>(mapping_result.base))
            .ok_or(Status::INTERNAL)?;
        // SAFETY: `cap` points to a mapped `page::SIZE` region that fits `MsiCapability`.
        unsafe {
            write_volatile(&raw mut (*cap.as_ptr()).id, MSI_CAPABILITY_ID);
        }

        Ok((vmo, mapping_result.mapping, cap))
    }

    /// Test that create() and get_info() operate properly
    #[test]
    fn allocation_creation_and_info_test() {
        let test_irq_cnt: u32 = 8;
        let alloc = unwrap_ok!(create_allocation(test_irq_cnt));

        let info = alloc.get_info();

        // Grab the lock and compare the block values and info values to both our test
        // data and info data.
        lock!(let _guard = alloc.lock().lock());
        assert_eq!(test_irq_cnt, alloc.block().num_irq);
        assert_true!(alloc.block().allocated);
        assert_eq!(info.base_irq_id, alloc.block().base_irq_id);
        assert_eq!(info.num_irq, alloc.block().num_irq);
        assert_eq!(info.target_addr, alloc.block().tgt_addr);
        assert_eq!(info.target_data, alloc.block().tgt_data);
    }

    /// Test allocation limits
    #[test]
    fn allocation_irq_count_test() {
        // Check that the valid range of allocation sizes work. Verifies that only
        // powers of two are valid.
        for cnt in 1..MSI_ALLOCATION_COUNT_MAX {
            let res =
                MsiAllocation::create_with_fns(cnt, msi_allocate, msi_free, msi_is_supported_true);
            if cnt.is_power_of_two() {
                assert_ok!(res.map(|_| ()));
            } else {
                assert_err!(res, Status::INVALID_ARGS);
            }
        }

        // And check the failure cases.
        assert_err!(
            MsiAllocation::create_with_fns(0, msi_allocate, msi_free, msi_is_supported_true),
            Status::INVALID_ARGS
        );
        assert_err!(
            MsiAllocation::create_with_fns(
                MSI_ALLOCATION_COUNT_MAX + 1,
                msi_allocate,
                msi_free,
                msi_is_supported_true,
            ),
            Status::INVALID_ARGS
        );
    }

    /// Test reservations for MSI ids
    #[test]
    fn allocation_reservation_test() {
        let alloc = unwrap_ok!(create_allocation(MSI_ALLOCATION_COUNT_MAX));

        // Verify the bounds checking and state of id reservations.
        assert_err!(alloc.release_id(0), Status::BAD_STATE);
        assert_ok!(alloc.reserve_id(0));
        assert_err!(alloc.reserve_id(0), Status::ALREADY_BOUND);
        assert_ok!(alloc.release_id(0));
        assert_err!(alloc.reserve_id(MSI_ALLOCATION_COUNT_MAX), Status::INVALID_ARGS);
        assert_err!(alloc.release_id(MSI_ALLOCATION_COUNT_MAX), Status::INVALID_ARGS);
    }

    /// Test for MSI platform support hooks
    #[test]
    fn allocation_support_test() {
        assert_err!(
            MsiAllocation::create_with_fns(
                1,
                msi_allocate_assert,
                msi_free_assert,
                msi_is_supported_false,
            ),
            Status::NOT_SUPPORTED
        );
    }

    // Use a static var for tracking calls rather than a lambda to avoid storage issues with lambda
    // captures and function pointers without having to increase complexity in the dispatcher.
    static REGISTER_CALL_COUNT: AtomicU32 = AtomicU32::new(0);
    unsafe extern "C" fn register_fn(
        _block: *const MsiBlock,
        _msi_id: u32,
        _handler: InterruptHandler,
    ) {
        REGISTER_CALL_COUNT.fetch_add(1, Ordering::Relaxed);
    }

    /// Test that MsiInterruptDispatchers cannot overlap on an MSI id
    #[test]
    fn interrupt_duplication_test() {
        let alloc = unwrap_ok!(create_allocation(MSI_ALLOCATION_COUNT_MAX));

        let vmo_res = create_valid_msi_vmo();
        if vmo_res.as_ref().is_err_and(|&st| st == Status::NOT_SUPPORTED) {
            kprint!("reserved region not available, skipping test");
            return true;
        }
        let (vmo, _mapping, _cap) = unwrap_ok!(vmo_res);

        // Now to the meat of the test. Ensure that two MsiInterruptDispatchers cannot share
        // the same MSI id, and that when a dispatcher is cleaned up it releases the
        // Id reservation in the allocation.
        let (d1, _rights) = unwrap_ok!(MsiInterruptDispatcher::create(
            alloc.clone(),
            0,
            &vmo,
            /*vmo_offset=*/ 0,
            /*options=*/ 0,
            Some(register_fn),
        ));
        assert_err!(
            MsiInterruptDispatcher::create(
                alloc.clone(),
                0,
                &vmo,
                /*vmo_offset=*/ 0,
                /*options=*/ 0,
                Some(register_fn),
            ),
            Status::ALREADY_BOUND
        );
        drop(d1);
        let (_d2, _rights) = unwrap_ok!(MsiInterruptDispatcher::create(
            alloc,
            0,
            &vmo,
            /*vmo_offset=*/ 0,
            /*options=*/ 0,
            Some(register_fn),
        ));
    }

    /// Test that MsiInterruptDispatcher validates the VMO
    #[test]
    fn interrupt_vmo_test() {
        REGISTER_CALL_COUNT.store(0, Ordering::Relaxed);
        let alloc = unwrap_ok!(create_allocation(MSI_ALLOCATION_COUNT_MAX));

        // This test emulates a block of MSI interrupts each taking up a given bit in a register for
        // their own masking. It validates that the MsiInterruptDispatcher masks / unmasks the
        // correct bit, and that the operation of the inherited InterruptDispatcher side of things
        // behaves correctly when interrupts are triggered.
        {
            let vmo_size = page::round_up(size_of::<MsiCapability>());
            let vmo = unwrap_ok!(VmObjectPaged::create_contiguous(
                pmm::ALLOC_FLAG_ANY,
                vmo_size as u64,
                /*alignment_log2=*/ 0,
            ));
            let vmo = VmObjectPaged::into_vm_object(vmo);
            let vmo_noncontig = unwrap_ok!(VmObjectPaged::create(
                pmm::ALLOC_FLAG_ANY,
                /*options=*/ 0,
                vmo_size as u64,
            ));
            let vmo_noncontig = VmObjectPaged::into_vm_object(vmo_noncontig);

            // This should fail because the VMO is non-contiguous.
            assert_err!(
                MsiInterruptDispatcher::create(
                    alloc.clone(),
                    0,
                    &vmo_noncontig,
                    /*vmo_offset=*/ 0,
                    /*options=*/ 0,
                    Some(register_fn),
                ),
                Status::INVALID_ARGS
            );
            // This should fail because the VMO has not had a cache policy set.
            assert_err!(
                MsiInterruptDispatcher::create(
                    alloc.clone(),
                    0,
                    &vmo,
                    /*vmo_offset=*/ 0,
                    /*options=*/ 0,
                    Some(register_fn),
                ),
                Status::INVALID_ARGS
            );
            // SAFETY: `vmo` has no mappings yet.
            unsafe {
                assert_ok!(vmo.set_mapping_cache_policy(ARCH_MMU_FLAG_UNCACHED_DEVICE));
            }
            // Create will still fail because the VMO doesn't look like an MSI capability.
            assert_err!(
                MsiInterruptDispatcher::create(
                    alloc.clone(),
                    0,
                    &vmo,
                    /*vmo_offset=*/ 0,
                    /*options=*/ 0,
                    Some(register_fn),
                ),
                Status::INVALID_ARGS
            );
        }

        let vmo_res = create_valid_msi_vmo();
        if vmo_res.as_ref().is_err_and(|&st| st == Status::NOT_SUPPORTED) {
            kprint!("reserved region not available, skipping test");
            return true;
        }
        let (vmo, _mapping, _cap) = unwrap_ok!(vmo_res);
        // Now create() should succeed.
        let (_interrupt, _rights) = unwrap_ok!(MsiInterruptDispatcher::create(
            alloc,
            0,
            &vmo,
            /*vmo_offset=*/ 0,
            /*options=*/ 0,
            Some(register_fn),
        ));
        assert_eq!(1u32, REGISTER_CALL_COUNT.load(Ordering::Relaxed));
    }

    /// Test that MsiInterruptDispatcher unmasks on creation
    #[test]
    fn interrupt_creation_mask_test() {
        let alloc = unwrap_ok!(create_allocation(MSI_ALLOCATION_COUNT_MAX));
        let vmo_res = create_valid_msi_vmo();
        if vmo_res.as_ref().is_err_and(|&st| st == Status::NOT_SUPPORTED) {
            kprint!("reserved region not available, skipping test");
            return true;
        }
        let (vmo, _mapping, cap) = unwrap_ok!(vmo_res);
        let cap = cap.as_ptr();

        // Below test all the mask variants around PVM. This also serves to ensure
        // that the dtor properly releases the msi registration in all cases.
        // Test 32 bit / no pvm
        {
            // SAFETY: `cap` points to the mapped `MsiCapability` region.
            unsafe {
                write_volatile(&raw mut (*cap).control, !(MSI_PVM_SUPPORTED | MSI_64BIT_SUPPORTED));
            }
            for msi_id in 0..MSI_ALLOCATION_COUNT_MAX {
                // SAFETY: `cap` points to the mapped `MsiCapability` region.
                unsafe {
                    write_volatile(&raw mut (*cap).mask_bits_32, 0);
                    write_volatile(&raw mut (*cap).mask_bits_64, 0);
                }
                let (_interrupt, _rights) = unwrap_ok!(MsiInterruptDispatcher::create(
                    alloc.clone(),
                    msi_id,
                    &vmo,
                    /*vmo_offset=*/ 0,
                    /*options=*/ 0,
                    Some(register_fn),
                ));
                // SAFETY: `cap` points to the mapped `MsiCapability` region.
                let (val_32, val_64) = unsafe {
                    (
                        read_volatile(&raw const (*cap).mask_bits_32),
                        read_volatile(&raw const (*cap).mask_bits_64),
                    )
                };
                expect_eq!(val_32, 0u32);
                expect_eq!(val_64, 0u32);
            }
        }

        // Test 32 bit / pvm
        {
            // SAFETY: `cap` points to the mapped `MsiCapability` region.
            unsafe {
                write_volatile(&raw mut (*cap).control, !MSI_64BIT_SUPPORTED);
            }
            for msi_id in 0..MSI_ALLOCATION_COUNT_MAX {
                // SAFETY: `cap` points to the mapped `MsiCapability` region.
                unsafe {
                    write_volatile(&raw mut (*cap).mask_bits_32, 0);
                    write_volatile(&raw mut (*cap).mask_bits_64, 0);
                }
                let (_interrupt, _rights) = unwrap_ok!(MsiInterruptDispatcher::create(
                    alloc.clone(),
                    msi_id,
                    &vmo,
                    /*vmo_offset=*/ 0,
                    /*options=*/ 0,
                    Some(register_fn),
                ));
                // SAFETY: `cap` points to the mapped `MsiCapability` region.
                let (val_32, val_64) = unsafe {
                    (
                        read_volatile(&raw const (*cap).mask_bits_32),
                        read_volatile(&raw const (*cap).mask_bits_64),
                    )
                };
                expect_eq!(0u32, val_32 & (1u32 << msi_id));
                expect_eq!(val_64, 0u32);
            }
        }

        // Test 64 bit / no pvm
        {
            // SAFETY: `cap` points to the mapped `MsiCapability` region.
            unsafe {
                write_volatile(&raw mut (*cap).control, MSI_64BIT_SUPPORTED);
            }
            for msi_id in 0..MSI_ALLOCATION_COUNT_MAX {
                // SAFETY: `cap` points to the mapped `MsiCapability` region.
                unsafe {
                    write_volatile(&raw mut (*cap).mask_bits_32, 0);
                    write_volatile(&raw mut (*cap).mask_bits_64, 0);
                }
                let (_interrupt, _rights) = unwrap_ok!(MsiInterruptDispatcher::create(
                    alloc.clone(),
                    msi_id,
                    &vmo,
                    /*vmo_offset=*/ 0,
                    /*options=*/ 0,
                    Some(register_fn),
                ));
                // SAFETY: `cap` points to the mapped `MsiCapability` region.
                let (val_32, val_64) = unsafe {
                    (
                        read_volatile(&raw const (*cap).mask_bits_32),
                        read_volatile(&raw const (*cap).mask_bits_64),
                    )
                };
                expect_eq!(val_32, 0u32);
                expect_eq!(val_64, 0u32);
            }
        }

        // Test 64 bit / pvm
        {
            // SAFETY: `cap` points to the mapped `MsiCapability` region.
            unsafe {
                write_volatile(&raw mut (*cap).control, MSI_PVM_SUPPORTED | MSI_64BIT_SUPPORTED);
            }
            for msi_id in 0..MSI_ALLOCATION_COUNT_MAX {
                // SAFETY: `cap` points to the mapped `MsiCapability` region.
                unsafe {
                    write_volatile(&raw mut (*cap).mask_bits_32, 0);
                    write_volatile(&raw mut (*cap).mask_bits_64, 0);
                }
                let (_interrupt, _rights) = unwrap_ok!(MsiInterruptDispatcher::create(
                    alloc.clone(),
                    msi_id,
                    &vmo,
                    /*vmo_offset=*/ 0,
                    /*options=*/ 0,
                    Some(register_fn),
                ));
                // SAFETY: `cap` points to the mapped `MsiCapability` region.
                let (val_32, val_64) = unsafe {
                    (
                        read_volatile(&raw const (*cap).mask_bits_32),
                        read_volatile(&raw const (*cap).mask_bits_64),
                    )
                };
                expect_eq!(val_32, 0u32);
                expect_eq!(0u32, val_64 & (1u32 << msi_id));
            }
        }
    }

    /// Test that MsiAllocation & MsiInterruptDispatcher ownership is correct
    #[test]
    fn out_of_order_ownership_test() {
        let vmo_res = create_valid_msi_vmo();
        if vmo_res.as_ref().is_err_and(|&st| st == Status::NOT_SUPPORTED) {
            kprint!("reserved region not available, skipping test");
            return true;
        }
        let (vmo, _mapping, _cap) = unwrap_ok!(vmo_res);
        let (interrupt1, interrupt2) = {
            let alloc = unwrap_ok!(create_allocation(MSI_ALLOCATION_COUNT_MAX));
            let (interrupt1, _rights) = unwrap_ok!(MsiInterruptDispatcher::create(
                alloc.clone(),
                /*msi_id=*/ 0,
                &vmo,
                /*vmo_offset=*/ 0,
                /*options=*/ 0,
                Some(register_fn),
            ));
            let (interrupt2, _rights) = unwrap_ok!(MsiInterruptDispatcher::create(
                alloc,
                /*msi_id=*/ 1,
                &vmo,
                /*vmo_offset=*/ 0,
                /*options=*/ 0,
                Some(register_fn),
            ));
            (interrupt1, interrupt2)
        };

        // This test verifies that although our allocation order was Alloc -> 1 -> 2,
        // freeing in the same order behaves properly due to the reference ownership.
        drop(interrupt1.release());
        drop(interrupt2);
    }
}
