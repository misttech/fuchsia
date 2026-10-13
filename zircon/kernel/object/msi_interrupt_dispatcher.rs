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
    zx_rights_t, zx_status_t,
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
const K_FLAGS: u32 = INTERRUPT_UNMASK_PREWAIT | INTERRUPT_MASK_POSTWAIT;

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
const K_MSI_CAPABILITY_ID: u8 = 0x5;
const K_MSI_64BIT_SUPPORTED: u16 = 1 << 7;
const K_MSI_PVM_SUPPORTED: u16 = 1 << 8;

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

const K_MSIX_VECTOR_CONTROL_MASK_BIT: u32 = 0;

const fn msix_table_offset(id: u32) -> usize {
    id as usize * size_of::<MsixTableEntry>()
}

#[repr(C)]
enum MsiVariant {
    Msi(MsiInterruptDispatcherImpl),
    Msix(MsixInterruptDispatcherImpl),
}

/// The common interface for all MSI related interrupt handling. This encompasses MSI and MSI-X.
#[repr(C)]
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
    // Cache the base irq id of the block so we can use it without locking.
    base_irq_id: u32,
    // The specific MSI id within the block that this dispatcher services.
    msi_id: u32,
}

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
                self.base_irq_id + self.msi_id,
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

            InterruptDispatcher::create_raw(K_FLAGS, 0, |disp| {
                let variant = MsiVariant::Msix(MsixInterruptDispatcherImpl::new(
                    alloc.clone(),
                    msi_id,
                    &mapping,
                    vmo_offset as zx_off_t,
                ));
                Self::new(disp, alloc, mapping, base_irq_id, msi_id, register_int_fn, variant);
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
            if unsafe { read_volatile(&raw const (*cap).id) } != K_MSI_CAPABILITY_ID {
                return Err(Status::INVALID_ARGS);
            }

            // SAFETY: `vmo_offset + size_of::<MsiCapability>() <= vmo.size()` was verified above.
            let ctrl_val: u16 = unsafe { read_volatile(&raw const (*cap).control) };
            let has_pvm = (ctrl_val & K_MSI_PVM_SUPPORTED) != 0;
            let has_64bit = (ctrl_val & K_MSI_64BIT_SUPPORTED) != 0;
            InterruptDispatcher::create_raw(K_FLAGS, 0, |disp| {
                let variant = MsiVariant::Msi(MsiInterruptDispatcherImpl::new(
                    alloc.clone(),
                    msi_id,
                    &mapping,
                    vmo_offset as zx_off_t,
                    has_pvm,
                    has_64bit,
                    register_int_fn,
                ));
                Self::new(disp, alloc, mapping, base_irq_id, msi_id, register_int_fn, variant);
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
        base_irq_id: u32,
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
                base_irq_id,
                msi_id,
            }));
        }
        DISPATCHER_MSI_CREATE_COUNT.add(1);
        disp.initialize_wake_event();
    }

    const fn vector(&self) -> u32 {
        self.base_irq_id + self.msi_id
    }

    /// Masks the MSI or MSI-X interrupt vector.
    pub fn mask_interrupt(&self) {
        match &self.variant {
            MsiVariant::Msi(m) => m.mask_interrupt(),
            MsiVariant::Msix(m) => m.mask_interrupt(),
        }
    }

    /// Unmasks the MSI or MSI-X interrupt vector.
    pub fn unmask_interrupt(&self) {
        match &self.variant {
            MsiVariant::Msi(m) => m.unmask_interrupt(),
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

#[repr(C)]
struct MsiInterruptDispatcherImpl {
    alloc: RefPtr<MsiAllocation>,
    msi_id: u32,
    // Not all interrupt controllers / configurations support masking at the
    // platform level. This is set accordingly if support is detected.
    has_platform_pvm: bool,
    // Whether or not the given device function supports per vector masking within
    // the PCI MSI capability.
    has_cap_pvm: bool,
    has_64bit: bool,
    // A pointer to the MSI Mask Register for this specific device function. To
    // synchronize with other MSI interactions in the device it requires the MSI
    // allocation's lock.
    capability: NonNull<MsiCapability>,
}

impl MsiInterruptDispatcherImpl {
    fn new(
        alloc: RefPtr<MsiAllocation>,
        msi_id: u32,
        mapping: &RefPtr<VmMapping>,
        reg_offset: zx_off_t,
        has_cap_pvm: bool,
        has_64bit: bool,
        register_int_fn: RegisterIntFn,
    ) -> Self {
        let has_platform_pvm = fn_addr_eq(register_int_fn, msi_register_handler as RegisterIntFn)
            && msi_supports_masking();
        let capability = NonNull::new(with_exposed_provenance_mut::<MsiCapability>(
            mapping.base().0 + reg_offset as usize,
        ))
        .expect("kernel VmMapping address must be non-null");
        Self { alloc, msi_id, has_platform_pvm, has_cap_pvm, has_64bit, capability }
    }

    fn mask_interrupt(&self) {
        DISPATCHER_MSI_MASK_COUNT.add(1);

        lock!(let _guard = self.alloc.lock().lock());
        if self.has_platform_pvm {
            msi_mask_unmask(self.alloc.block(), self.msi_id, true);
        }

        if self.has_cap_pvm {
            let mask: u32 = 1 << self.msi_id;
            let cap = self.capability.as_ptr();
            // SAFETY: `cap` is a valid mapped `MsiCapability` pointer synchronized
            // by `self.alloc.lock()` held via `_guard` above.
            unsafe {
                let mask_reg = if self.has_64bit {
                    &raw mut (*cap).mask_bits_64
                } else {
                    &raw mut (*cap).mask_bits_32
                };
                write_volatile(mask_reg, read_volatile(mask_reg) | mask);
            }
            device_memory_barrier();
        }
    }

    fn unmask_interrupt(&self) {
        DISPATCHER_MSI_UNMASK_COUNT.add(1);

        if self.has_platform_pvm {
            msi_mask_unmask(self.alloc.block(), self.msi_id, false);
        }

        if self.has_cap_pvm {
            lock!(let _guard = self.alloc.lock().lock());
            let mask: u32 = !(1 << self.msi_id);
            let cap = self.capability.as_ptr();
            // SAFETY: `cap` is a valid mapped `MsiCapability` pointer synchronized
            // by `self.alloc.lock()` held via `_guard` above.
            unsafe {
                let mask_reg = if self.has_64bit {
                    &raw mut (*cap).mask_bits_64
                } else {
                    &raw mut (*cap).mask_bits_32
                };
                write_volatile(mask_reg, read_volatile(mask_reg) & mask);
            }
            device_memory_barrier();
        }
    }
}

#[repr(C)]
struct MsixInterruptDispatcherImpl {
    alloc: RefPtr<MsiAllocation>,
    msi_id: u32,
    table_entries: NonNull<MsixTableEntry>,
}

impl MsixInterruptDispatcherImpl {
    fn new(
        alloc: RefPtr<MsiAllocation>,
        msi_id: u32,
        mapping: &RefPtr<VmMapping>,
        table_offset: zx_off_t,
    ) -> Self {
        let table_entries = NonNull::new(with_exposed_provenance_mut::<MsixTableEntry>(
            mapping.base().0 + table_offset as usize,
        ))
        .expect("kernel VmMapping address must be non-null");
        let this = Self { alloc, msi_id, table_entries };

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
        let tgt_addr = this.alloc.block().tgt_addr;
        let tgt_data = this.alloc.block().tgt_data;
        // SAFETY: `table_entries` is mapped for `msi_id + 1` entries (validated in `create`).
        unsafe {
            let entry = this.table_entries.as_ptr().wrapping_add(msi_id as usize);
            write_volatile(&raw mut (*entry).msg_addr, (tgt_addr & u32::MAX as u64) as u32);
            write_volatile(&raw mut (*entry).msg_upper_addr, (tgt_addr >> 32) as u32);
            write_volatile(&raw mut (*entry).msg_data, tgt_data + msi_id);
        }
        device_memory_barrier();
        this
    }

    fn mask_interrupt(&self) {
        DISPATCHER_MSI_MASK_COUNT.add(1);
        // SAFETY: `self.table_entries` is mapped for `self.msi_id + 1` entries.
        unsafe {
            let reg_ptr =
                &raw mut (*self.table_entries.as_ptr().wrapping_add(self.msi_id as usize))
                    .vector_control;
            let mut reg = read_volatile(reg_ptr);
            reg |= 1u32 << K_MSIX_VECTOR_CONTROL_MASK_BIT;
            write_volatile(reg_ptr, reg);
        }
        device_memory_barrier();
    }

    fn unmask_interrupt(&self) {
        DISPATCHER_MSI_UNMASK_COUNT.add(1);
        // SAFETY: `self.table_entries` is mapped for `self.msi_id + 1` entries.
        unsafe {
            let reg_ptr =
                &raw mut (*self.table_entries.as_ptr().wrapping_add(self.msi_id as usize))
                    .vector_control;
            let mut reg = read_volatile(reg_ptr);
            reg &= !(1u32 << K_MSIX_VECTOR_CONTROL_MASK_BIT);
            write_volatile(reg_ptr, reg);
        }
        device_memory_barrier();
    }

    // Performs the inverse of the configuration set up in the constructor. Mask the
    // interrupt and clear out the table entry.
    fn cleanup(&mut self) {
        self.mask_interrupt();
        // SAFETY: `self.table_entries` is mapped for `self.msi_id + 1` entries while
        // `MsiInterruptDispatcher::mapping` is still alive and `self.msi_id` is still reserved.
        unsafe {
            let entry = self.table_entries.as_ptr().wrapping_add(self.msi_id as usize);
            write_volatile(&raw mut (*entry).msg_addr, 0);
            write_volatile(&raw mut (*entry).msg_upper_addr, 0);
            write_volatile(&raw mut (*entry).msg_data, 0);
        }
        device_memory_barrier();
    }
}

/// # Safety
///
/// `alloc` and `vmo` must be valid `RefPtr` references, and `out_rights` and `out_interrupt` must
/// be valid out-pointers when non-null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_msi_interrupt_dispatcher_create(
    alloc: &RefPtr<MsiAllocation>,
    msi_id: u32,
    vmo: &RefPtr<VmObject>,
    cap_offset: zx_off_t,
    options: u32,
    out_rights: *mut zx_rights_t,
    out_interrupt: *mut KernelHandle<InterruptDispatcher>,
    register_int_fn: Option<RegisterIntFn>,
) -> zx_status_t {
    // SAFETY: Caller guarantees `out_rights` and `out_interrupt` are valid if non-null.
    let out = unsafe { out_rights.as_mut().zip(out_interrupt.as_mut()) };
    Status::result_into_raw(out.ok_or(Status::INVALID_ARGS).and_then(
        |(out_rights, out_interrupt)| {
            let (interrupt, rights) = MsiInterruptDispatcher::create(
                alloc.clone(),
                msi_id,
                vmo,
                cap_offset as zx_paddr_t,
                options,
                register_int_fn,
            )?;
            *out_rights = rights;
            *out_interrupt = interrupt;
            Ok(())
        },
    ))
}
