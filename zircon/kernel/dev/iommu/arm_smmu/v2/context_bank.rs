// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM SMMUv2 Context Bank (`CBARn`, `CBA2Rn`, `SMMU_CBn_*`) state and hardware programming.
//!
//! -- Notes about TLB invalidation operations --
//!
//! Barriers:
//!
//! TLB operations are generally performed after making changes to the Page Table Entries (PTEs)
//! in RAM, to either define a new mapping in, or to remove an existing mapping from, a device's
//! address space.
//!
//! In either case, it is important to make sure that after we make changes to the PTEs:
//!
//! 1) The changes have been flushed from the CPU's data cache to physical memory, so that the HW
//!    is guaranteed to see the new values the next time it goes to physical memory to perform a
//!    page table walk.
//! 2) TLB entries for the region of the device's address space have been properly invalidated
//!    after changes to the PTEs are made and before other code starts to run.
//!
//! For #1, it is expected that anyone manipulating page table entries in RAM will perform a
//! `clean_dcache_range` on all of the pages which they manipulated as part of their PTE
//! maintenance _before_ TLB invalidation takes place. This is handled by [`DeviceAspace`].
//! Additionally, the ARM64 implementation of `CleanDataCacheRange` ends with a `dsb sy` barrier
//! ensuring that all modified bytes must be flushed all of the way to physical memory before
//! subsequent instructions are allowed to execute.
//!
//! For #2, all of the TLB invalidation operations end by calling
//! [`ContextBank::tlb_sync_invalidate_operation`], which ends by calling
//! [`SmmuEnv::device_memory_barrier`]. On ARM64 Zircon, this issues a `dsb sy` instruction, as is
//! suggested by section 5.4.2 of the ARM SMMUv2 spec. This ensures that all requested TLB entries
//! have been properly invalidated before execution continues. This avoids potential hazards such
//! as returning a page of memory to the PMM which is still referenced by a stale table-entry in
//! the TLBs, despite having been invalidated in the PTEs residing in physical memory.
//!
//! ASIDs:
//!
//! All context banks managed by the SMMU driver are assigned a unique ASID during initialization.
//! This is for a couple of reasons, but the most important is to avoid any potential of TLB
//! aliasing, as alluded to by the ARM SMMUv2 spec. TLB behavior requirements are discussed in
//! sections 2.5 and 2.6, however this discussion is far from clear, precise, and obvious. What
//! determines if a TLB cache hit happens? It depends on the precise definition of a Translation
//! Context, vs. a Translation Context Bank, vs a Translation Regime, vs a set of "attributes" vs
//! a "TLB tag" and so on.
//!
//! There is one statement made (immediately before section 2.5.1) which does seem unusually
//! clear. It reads:
//!
//! > If multiple context banks have the same attributes but describe different translations, the
//! > results of a TLB lookup are UNPREDICTABLE. For example, where two context banks use the same
//! > ASID and VMID, each context bank might use TLB entries that are intended to be used by the
//! > other context bank.
//!
//! To avoid any chance of two context banks accidentally colliding over TLB entries for each
//! other, we ensure that all context bank hardware has a unique ASID assigned to it during
//! startup, and we make sure that all TLB entries have been invalidated (globally) at the end of
//! initialization. Following this, we can target all of the TLB entries for a given context bank
//! for invalidation using the unique ASID we have assigned to that context bank.

use debug::dprintf;
use fbl::{DoublyLinkedListContainable, DoublyLinkedListNode, Recyclable, UniquePtr};
use kalloc::AllocError;
use zx_status::Status;

use crate::arm_smmu::common::{
    ArmSmmuMode, DeviceAspace, PAGE_MASK, PAGE_SHIFT, PAGE_SIZE, SmmuEnv, TlbInvalidator,
};
pub use crate::arm_smmu::common::{DEFAULT_TCR_T0SZ, DEFAULT_TTBR0_ADDR_SPACE_SZ};
use crate::arm_smmu::v2::registers::gr1::{Cba2r, Cba2rVa64, Cbar, CbarS1TS2Bypass, CbarType};
use crate::arm_smmu::v2::registers::s1cbr::{
    Sctlr, Tcr, Tcr32Bit, Tcr64Bit, TcrExt32Bit, TcrTg0, TcrTg1, Tlbiasid, TlbivaAArch64,
    Tlbstatus, Tlbsync, Ttbr0_32Bit, Ttbr0_64Bit, Ttbr0Ext32Bit, Ttbr1_32Bit, Ttbr1_64Bit,
    Ttbr1Ext32Bit,
};
use crate::arm_smmu::v2::registers::{SmmuMmio, s2cbr};
use crate::arm_smmu::v2::utils::{
    AddrMode, BtiMode, SmmuController, arm_cbar_type_to_string, arm_smmu_mode_to_string,
    bti_mode_to_string,
};

// We need to allocate a unique ASID for every context bank in the system that we control. For now,
// we just use a context bank's index with some upper bits set in it.
//
// TODO(johngro): We need to make sure that when we come out of the bootloaders and claim the
// hardware, that we do not end up adopting any hardware which is using one of the ASIDs we plan to
// use. Currently, we have no reference hardware which enters the kernel with the hardware
// configured to perform address translation at all, so it isn't an issue yet. Some day, however,
// we should take the time to provide a 100% guarantee that there will be no collisions.

/// Reserved ASID value (`0xFFFF`) assigned to disabled `TTBR` registers so no transaction can hit
/// the TLB cache.
pub const UNUSED_ASID: u16 = 0xFFFF;

/// Fixed upper byte (`0x5A00`) ORed with `cb_ndx` to form a unique 16-bit ASID per Context Bank.
pub const UPPER_ASID_BITS: u16 = 0x5A00;

/// Mask (`0xFF00`) covering the fixed upper bits of a Context Bank ASID.
pub const UPPER_ASID_MASK: u16 = 0xFF00;

/// Page-count threshold above which [`ContextBank::tlb_invalidate_region`] invalidates the entire
/// ASID instead of issuing per-page `TLBIVA` operations.
pub const INVALIDATE_ALL_PAGE_COUNT_THRESHOLD: u64 = 256;

/// Info fetched from the `TCR` and `TTBRn` registers describing how a translation table base
/// register is configured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TtbrInfo {
    /// Whether translation table walking via this TTBR is enabled.
    pub enabled: bool,
    /// Log2 of the translation granule size in bytes (`12` for 4 KiB, `14` for 16 KiB, `16` for
    /// 64 KiB).
    pub granule_size_bits: u32,
    /// Lowest valid virtual address covered by this TTBR region.
    pub first_valid_addr: u64,
    /// Highest valid virtual address covered by this TTBR region.
    pub last_valid_addr: u64,
    /// Physical base address of the root translation table.
    pub ttbr_paddr: u64,
}

/// [`TlbInvalidator`] adapter that forwards page/region invalidations from [`DeviceAspace`] to a
/// [`ContextBank`].
pub struct ContextBankInvalidator<'a>(pub &'a ContextBank);

impl TlbInvalidator for ContextBankInvalidator<'_> {
    #[inline]
    fn invalidate(&mut self, base: u64, size: u64) {
        // Zero size means flush everything for this ASID; otherwise flush the requested range.
        if size == 0 {
            self.0.tlb_invalidate_by_asid();
        } else {
            self.0.tlb_invalidate_region(base, size);
        }
    }
}

/// Representation of a single SMMUv2 Translation Context Bank (`CBARn`, `CBA2Rn`, and Context Bank
/// register page `cb_ndx`).
#[derive(DoublyLinkedListContainable, Recyclable)]
pub struct ContextBank {
    #[dll_node]
    list_node: DoublyLinkedListNode<ContextBank>,
    cb_ndx: u32,
    mode: BtiMode,
    gr1_base: SmmuMmio,
    cb_base: SmmuMmio,
    addr_mode: AddrMode,
    ttbrs: [TtbrInfo; 2],
}

// SAFETY: The only `!Send` field of `ContextBank` is `list_node: DoublyLinkedListNode<ContextBank>`
// (which contains `UnsafeCell<NodeLinks<Self>>`). `ContextBank` and any intrusive list containing
// it are only accessed or transferred across threads while holding the owning `Smmu` or `SmmuBti`
// synchronization lock.
unsafe impl Send for ContextBank {}

impl Drop for ContextBank {
    #[inline]
    fn drop(&mut self) {
        debug_assert!(!self.list_node.in_container());
        debug_assert!(self.gr1_base.is_null());
        debug_assert!(self.cb_base.is_null());
        debug_assert!(self.mode == BtiMode::Shutdown);
    }
}

impl ContextBank {
    /// Default `TCR.T0SZ` value (`16`) configuring a 48-bit device address space.
    pub const DEFAULT_TCR_T0SZ: u32 = DEFAULT_TCR_T0SZ;

    /// Default `TTBR0` stage-1 device address space size in bytes (`1 << 48`).
    pub const DEFAULT_TTBR0_ADDR_SPACE_SZ: u64 = DEFAULT_TTBR0_ADDR_SPACE_SZ;

    /// Reserved ASID value (`0xFFFF`) assigned to disabled `TTBR` registers.
    pub const UNUSED_ASID: u16 = UNUSED_ASID;

    /// Fixed upper byte (`0x5A00`) ORed with `cb_ndx` to form a unique 16-bit ASID.
    pub const UPPER_ASID_BITS: u16 = UPPER_ASID_BITS;

    /// Mask (`0xFF00`) covering the fixed upper bits of a Context Bank ASID.
    pub const UPPER_ASID_MASK: u16 = UPPER_ASID_MASK;

    /// Constructs a new, uninitialized [`ContextBank`] for hardware context bank index `cb_ndx`.
    #[inline]
    const fn new(cb_ndx: u32) -> Self {
        Self {
            list_node: DoublyLinkedListNode::new(),
            cb_ndx,
            mode: BtiMode::Invalid,
            gr1_base: SmmuMmio::null(),
            cb_base: SmmuMmio::null(),
            addr_mode: AddrMode::Invalid,
            ttbrs: [
                TtbrInfo {
                    enabled: false,
                    granule_size_bits: 0,
                    first_valid_addr: 0,
                    last_valid_addr: 0,
                    ttbr_paddr: 0,
                },
                TtbrInfo {
                    enabled: false,
                    granule_size_bits: 0,
                    first_valid_addr: 0,
                    last_valid_addr: 0,
                    ttbr_paddr: 0,
                },
            ],
        }
    }

    /// Returns the hardware index (`0..num_cbs`) of this Context Bank.
    #[inline]
    pub const fn cb_ndx(&self) -> u32 {
        self.cb_ndx
    }

    /// Returns the current [`BtiMode`] of this Context Bank.
    #[inline]
    pub const fn mode(&self) -> BtiMode {
        self.mode
    }

    /// Returns the current [`AddrMode`] of this Context Bank.
    #[inline]
    pub const fn addr_mode(&self) -> AddrMode {
        self.addr_mode
    }

    /// Returns the `GR1` MMIO bank handle associated with this Context Bank.
    #[inline]
    pub const fn gr1_base(&self) -> SmmuMmio {
        self.gr1_base
    }

    /// Returns the per-Context-Bank (`SMMU_CBn_BASE`) MMIO bank handle.
    #[inline]
    pub const fn cb_base(&self) -> SmmuMmio {
        self.cb_base
    }

    /// Returns the two [`TtbrInfo`] descriptors (`TTBR0` and `TTBR1`) for this Context Bank.
    #[inline]
    pub const fn ttbrs(&self) -> &[TtbrInfo; 2] {
        &self.ttbrs
    }

    /// Returns `true` if this Context Bank is currently linked into an intrusive doubly-linked
    /// list.
    #[inline]
    pub fn in_container(&self) -> bool {
        self.list_node.in_container()
    }

    /// Computes the unique 16-bit ASID assigned to hardware context bank `cb_ndx`.
    #[inline]
    pub fn asid_for_cb_ndx(cb_ndx: u32) -> u16 {
        debug_assert!(cb_ndx <= 0xFF);
        (cb_ndx as u16) | Self::UPPER_ASID_BITS
    }

    /// Returns the unique 16-bit ASID assigned to this Context Bank.
    #[inline]
    pub fn asid(&self) -> u16 {
        Self::asid_for_cb_ndx(self.cb_ndx)
    }

    /// Records the `TTBR0` bookkeeping fields from an attached [`DeviceAspace`].
    ///
    /// Called by `SmmuBti` during BTI construction when attaching a newly created stage-1 address
    /// space to its Context Bank.
    #[inline]
    pub(crate) fn attach_aspace(&mut self, aspace: &DeviceAspace) {
        debug_assert_eq!(self.ttbrs[0].ttbr_paddr, 0);
        self.ttbrs[0].granule_size_bits = aspace.granule_size_bits();
        self.ttbrs[0].first_valid_addr = aspace.first_valid_address();
        self.ttbrs[0].last_valid_addr = aspace.last_valid_address();
        self.ttbrs[0].ttbr_paddr = aspace.root_tt_paddr();
    }

    /// Enables context fault interrupts (`SCTLR.CFIE = 1`) for this Context Bank.
    #[inline]
    pub(crate) fn enable_fault_interrupt(&mut self) {
        Sctlr::read_from(&self.cb_base).set_cfie(1).write_to(&self.cb_base);
    }

    /// Disables context fault interrupts (`SCTLR.CFIE = 0`) for this Context Bank.
    #[inline]
    pub(crate) fn disable_fault_interrupt(&mut self) {
        Sctlr::read_from(&self.cb_base).set_cfie(0).write_to(&self.cb_base);
    }

    /// Returns a [`ContextBankInvalidator`] implementing [`TlbInvalidator`] for [`DeviceAspace`]
    /// map/unmap operations.
    #[inline]
    pub(crate) const fn make_inval_op(&self) -> ContextBankInvalidator<'_> {
        ContextBankInvalidator(self)
    }

    /// Issues a `TLBSYNC` operation on `cb_base`, spins until `TLBSTATUS.SACTIVE == 0`, and
    /// executes a `DeviceMemoryBarrier` (`dsb sy`).
    fn tlb_sync_invalidate_operation(cb_base: &SmmuMmio) {
        Tlbsync::from_value(0).write_to(cb_base);
        while Tlbstatus::read_from(cb_base).sactive() != 0 {
            SmmuEnv::yield_cpu();
        }

        // See Section 5.4.2 "TLB maintenance operation processing"'s SYNC assembly example. We
        // need a `dsb sy` instruction following this operation.
        SmmuEnv::device_memory_barrier();
    }

    /// Invalidates all TLB entries tagged with `asid` via `cb_base` and synchronizes completion.
    fn tlb_invalidate_by_asid_at(asid: u16, cb_base: &SmmuMmio) {
        // Note that the `asid` passed here does not necessarily have to be the same as the ASID
        // assigned to this context bank in the registers pointed to by `cb_base`, however they
        // currently always should be.
        Tlbiasid::from_value(0).set_asid(asid as u32).write_to(cb_base);
        Self::tlb_sync_invalidate_operation(cb_base);
    }

    /// Invalidates all TLB entries tagged with this Context Bank's ASID and synchronizes
    /// completion.
    #[inline]
    pub(crate) fn tlb_invalidate_by_asid(&self) {
        Self::tlb_invalidate_by_asid_at(self.asid(), &self.cb_base);
    }

    /// Invalidates TLB entries covering `[base_va, base_va + size)` for `asid` via `cb_base` and
    /// synchronizes completion.
    fn tlb_invalidate_region_at(base_va: u64, size: u64, asid: u16, cb_base: &SmmuMmio) {
        debug_assert_eq!(base_va & PAGE_MASK, 0);
        debug_assert_eq!(size & PAGE_MASK, 0);

        // If the number of page entries to invalidate is below our (arbitrary) acceptable
        // threshold, invalidate TLB entries based on device virtual addresses. Otherwise, simply
        // invalidate the entire TLB for this context bank.
        let page_count = size >> PAGE_SHIFT;
        if page_count > INVALIDATE_ALL_PAGE_COUNT_THRESHOLD {
            Self::tlb_invalidate_by_asid_at(asid, cb_base);
        } else {
            // TODO(johngro): Figure out exactly what rules our behavior has to conform to here.
            // Specifically, how many individual virtual address invalidate operations are we
            // allowed to queue before we must throttle ourselves, either by explicitly sync'ing
            // queued operations, or via some other mechanism.
            //
            // Section 5.4.2 "TLB maintenance operation processing" of the SMMUv2 docs says a
            // couple of different things on this topic. Specifically, they say:
            //
            // > An SMMU must accept an unbounded number of memory-mapped TLB maintenance
            // > operations without relying on the forward progress of client transactions.
            //
            // Which seems to imply that there is no limit. We are free to queue VA invalidate
            // operations as much as we want. But, just after this they say:
            //
            // > Note: Software must ensure that it limits the number of TLB Invalidate and SYNC
            // > operations issued to the same TLB invalidation resource. Failure to adhere to this
            // > can result in a situation where new operations are continuously added at a rate
            // > that prevents all operations being completed, preventing the TLB status from
            // > reporting that the context bank is inactive.
            //
            // But they provide no guidance as to what this limit should be. Are we limited in the
            // number of VA invalidate operations we are allowed to post before syncing, or are
            // they just saying "hey, be careful: if you are constantly invalidating your TLBs, you
            // can run into a situation where no one ever sees the SYNC state of their invalidation
            // operation complete"?
            //
            // We assume the latter, and that we can queue as many VA invalidate operations as we
            // want before sync, but it would be good to confirm this.
            let final_va = base_va + size;
            let mut va = base_va;
            while va < final_va {
                TlbivaAArch64::from_value(0)
                    .set_asid(asid as u64)
                    .set_address(va)
                    .write_to(cb_base);
                va += PAGE_SIZE;
            }
            Self::tlb_sync_invalidate_operation(cb_base);
        }
    }

    /// Invalidates TLB entries covering `[base_va, base_va + size)` for this Context Bank's ASID
    /// and synchronizes completion.
    #[inline]
    pub(crate) fn tlb_invalidate_region(&self, base_va: u64, size: u64) {
        Self::tlb_invalidate_region_at(base_va, size, self.asid(), &self.cb_base);
    }

    /// Allocates a [`UniquePtr<ContextBank>`] and caches `smmu.gr1_base()` and
    /// `smmu.get_cb_base(cb_ndx)`.
    fn create<S: SmmuController>(smmu: &S, cb_ndx: u32) -> Result<UniquePtr<Self>, Status> {
        let mut cb =
            UniquePtr::try_new(Self::new(cb_ndx)).map_err(|AllocError| Status::NO_MEMORY)?;

        // Cache references to the base addresses for our registers. While we exist, we are the
        // only thing allowed to write to them. Specifically, we own all of the registers in
        // context bank register page #cb_ndx, and the following registers in global register
        // space 1:
        //
        // + CBAR(cb_ndx)
        // + CBA2R(cb_ndx)
        // + CBFRSYNRA(cb_ndx)
        cb.gr1_base = smmu.gr1_base();
        cb.cb_base = smmu.get_cb_base(cb_ndx);
        Ok(cb)
    }

    /// Validates `cb_ndx`, allocates a [`UniquePtr<ContextBank>`], and disables its hardware
    /// registers.
    pub fn create_and_lockdown<S: SmmuController>(
        smmu: &S,
        cb_ndx: u32,
    ) -> Result<UniquePtr<Self>, Status> {
        Self::validate_ndx(smmu, cb_ndx)?;
        let cb = Self::create(smmu, cb_ndx)?;
        Self::disable_regs(cb.gr1_base, cb.cb_base, cb.cb_ndx);
        Ok(cb)
    }

    /// Validates `cb_ndx`, allocates a [`UniquePtr<ContextBank>`], and adopts its pre-existing
    /// hardware register state.
    pub fn create_and_adopt<S: SmmuController>(
        smmu: &S,
        cb_ndx: u32,
    ) -> Result<UniquePtr<Self>, Status> {
        Self::validate_ndx(smmu, cb_ndx)?;
        let mut cb = Self::create(smmu, cb_ndx)?;
        if let Err(status) = cb.adopt_register_state(smmu) {
            // `create` initialized `cb.gr1_base` and `cb.cb_base`. Reset these fields and set
            // `cb.mode` to `BtiMode::Shutdown` before `cb` is dropped so `Drop`'s debug assertions
            // do not trip on the adoption error path.
            cb.gr1_base = SmmuMmio::null();
            cb.cb_base = SmmuMmio::null();
            cb.mode = BtiMode::Shutdown;
            return Err(status);
        }
        Ok(cb)
    }

    /// Computes the effective address space size in bytes for `mode`.
    ///
    /// Accesses registers when `mode == BtiMode::Translation` and should only be called from a
    /// `ContextBank`'s owning BTI with the BTI's lock held.
    fn aspace_size_for_mode(&self, mode: BtiMode) -> u64 {
        if mode == BtiMode::Translation {
            // We currently only support AArch64 addressing, and only ever use the bottom half of
            // our address space (via TTBR0). Computing the effective size of our address space can
            // be done using TCR.T0SZ with the formula:
            //
            // size = 2**(64 - TCR.T0SZ)
            //
            // See Section 1.5.1 "Defining the VA subranges for stage 1 translations" for more
            // details.
            let tcr = Tcr64Bit::read_from(&self.cb_base);
            let mut t0sz = tcr.t0sz();

            // We should never configure for a full 64-bit address space as doing so would make it
            // impossible to report our actual `aspace_size` in byte units, which is what the
            // current ABI demands.
            if t0sz == 0 {
                t0sz = 16;
            }

            1u64 << (64 - t0sz)
        } else if mode == BtiMode::Bypass {
            // When we are operating in passthru mode, the addresses we will be returning for
            // pinned memory will be PA/IPAs of the underlying VMO. The max physical address we can
            // encounter should be determined by our page size and translation table format. For
            // VMSAv8-64, we get 36 bits of address, plus the number of bits in our page size. This
            // is the maximum size of a physical address an MMU can output, so it should be safe to
            // report it as the "size of the device's address space". Any address it gets back from
            // a pin operation should be somewhere in this range (48 bits, when we are using 4k
            // pages).
            const ADDR_SPACE_BITS: u32 = 36 + PAGE_SHIFT;
            const _: () = assert!(
                ADDR_SPACE_BITS < 64,
                "Page size too large to compute maximum physical address size for this system"
            );
            1u64 << ADDR_SPACE_BITS
        } else {
            0
        }
    }

    /// Returns the effective address space size in bytes for this Context Bank's current mode.
    #[inline]
    pub fn aspace_size(&self) -> u64 {
        self.aspace_size_for_mode(self.mode)
    }

    /// Decodes a `TCR.TG0` field value (`reg_bits`) into log2 granule size bits.
    fn decode_granule_size_bits(&self, reg_bits: TcrTg0) -> u32 {
        match reg_bits {
            TcrTg0::Granule4KB => 12,  // 4KB
            TcrTg0::Granule64KB => 16, // 64KB
            TcrTg0::Granule16KB => 14, // 16KB
            TcrTg0::Reserved => {
                dprintf!(
                    INFO,
                    "WARNING - unrecognized encoded context bank granule size ({}) in CB {}\n",
                    reg_bits as u32,
                    self.cb_ndx
                );
                0
            }
        }
    }

    /// Decodes a `TCR.TG1` field value (`reg_bits`) into log2 granule size bits.
    ///
    /// Temporary porting note: the C++ `AdoptRegisterState` passed `tcr.tg1()` (cast to `TcrTg0`)
    /// into `DecodeGranuleSizeBits(TcrTg0)`, even though `TCR.TG0` and `TCR.TG1` use different
    /// 2-bit hardware encodings (`TG0`: `00` = 4KB, `01` = 64KB, `10` = 16KB; `TG1`: `01` = 16KB,
    /// `10` = 4KB, `11` = 64KB). Here `TG1` is decoded using [`TcrTg1`] when `TTBR1` is enabled.
    fn decode_tg1_granule_size_bits(&self, reg_bits: TcrTg1) -> u32 {
        match reg_bits {
            TcrTg1::Granule4KB => 12,  // 4KB
            TcrTg1::Granule64KB => 16, // 64KB
            TcrTg1::Granule16KB => 14, // 16KB
            TcrTg1::Reserved0 => {
                dprintf!(
                    INFO,
                    "WARNING - unrecognized encoded context bank TG1 granule size ({}) in CB {}\n",
                    reg_bits as u32,
                    self.cb_ndx
                );
                0
            }
        }
    }

    /// Decodes the valid virtual address ranges of `TTBR0` and `TTBR1` from `t0sz`, `t1sz`, and
    /// `self.addr_mode`.
    fn decode_ttbr_regions(&mut self, t0sz: u32, t1sz: u32) {
        // First valid address for TTBR0 is always 0x0.
        self.ttbrs[0].first_valid_addr = 0x0;

        match self.addr_mode {
            AddrMode::Mode64Bit => {
                // Section 1.5.1, Figure 1-7
                //
                // Notes about the valid range of t[01]sz. The register field width of the size
                // fields in the TCR is 6 bits (when using VA64 addressing), so we should be able
                // to assert that these values are < 64.
                //
                // Values on the range [0, 12) don't really make any sense. The maximum size of an
                // address space when using VMSAv8-64 page tables is 52 bits, 36 bits from the page
                // tables and a 16 bit (max) page size. That said, while they don't make sense,
                // they can be programmed. When filling out bookkeeping while adopting register
                // state, compute the range's size in a naive fashion, simply applying the
                // `2**(64 - t[01]sz)` formula while ignoring the fact that values less than 12
                // might be nonsensical. Be sure to special case sz == 0. It is technically illegal
                // to shift a 64-bit value to the left by >= 64 bits.
                debug_assert!(t0sz < 64);
                debug_assert!(t1sz < 64);
                self.ttbrs[0].last_valid_addr =
                    if t0sz != 0 { (1u64 << (64 - t0sz)) - 1 } else { 0xFFFF_FFFF_FFFF_FFFF };
                self.ttbrs[1].first_valid_addr =
                    if t1sz != 0 { !((1u64 << (64 - t1sz)) - 1) } else { 0x0 };
                self.ttbrs[1].last_valid_addr = 0xFFFF_FFFF_FFFF_FFFF;
            }
            AddrMode::Ext32Bit => {
                // Section 1.5.1, Table 1-7
                debug_assert!(t0sz < 8);
                debug_assert!(t1sz < 8);

                // TODO(johngro): this is true for S1 translation, figure out what it is for S2
                // translation.
                const MAX_ADDR_BITS: u64 = 32;
                const MAX_ADDR: u64 = (1u64 << MAX_ADDR_BITS) - 1;

                if t1sz == 0 {
                    // Section 1.5.1, Figure 1-5
                    // "Control of TTBR0 and TTBR1 regions, when SMMU_CBn_TCR.T1SZ is zero"
                    self.ttbrs[0].last_valid_addr = (1u64 << (32 - t0sz)) - 1;

                    if t0sz == 0 {
                        if self.ttbrs[1].enabled {
                            dprintf!(
                                INFO,
                                "WARNING - TTBR1 is unused but enabled in context bank {}\n",
                                self.cb_ndx
                            );
                        }
                    } else {
                        self.ttbrs[1].first_valid_addr = self.ttbrs[0].last_valid_addr + 1;
                        self.ttbrs[1].last_valid_addr = MAX_ADDR;
                    }
                } else {
                    // Section 1.5.1, Figure 1-6
                    // "Control of TTBR0 and TTBR1 regions when SMMU_CBn_TCR.T1SZ is nonzero"
                    self.ttbrs[1].first_valid_addr = (1u64 << 32) - (1u64 << (32 - t1sz));
                    self.ttbrs[1].last_valid_addr = MAX_ADDR;

                    if t0sz == 0 {
                        self.ttbrs[0].last_valid_addr = self.ttbrs[1].first_valid_addr - 1;
                    } else {
                        self.ttbrs[0].last_valid_addr = (1u64 << (32 - t0sz)) - 1;
                    }
                }
            }
            AddrMode::Mode32Bit => {
                // Section 1.5.1, Table 1-6
                debug_assert!(t0sz < 8);
                self.ttbrs[0].last_valid_addr = (1u64 << (32 - t0sz)) - 1;

                if t0sz != 0 {
                    self.ttbrs[1].first_valid_addr = self.ttbrs[0].last_valid_addr + 1;
                    self.ttbrs[1].last_valid_addr = 0xFFFF_FFFF;
                } else {
                    // TTBR1 is not used and should be disabled, but go ahead and write something
                    // to these values so that they have deterministic values, even if they are
                    // technically undefined.
                    self.ttbrs[1].first_valid_addr = 0;
                    self.ttbrs[1].last_valid_addr = 0;
                    if self.ttbrs[1].enabled {
                        dprintf!(
                            INFO,
                            "WARNING - TTBR1 is unused but enabled in context bank {}\n",
                            self.cb_ndx
                        );
                    }
                }
            }
            AddrMode::Invalid => {
                // The addressing mode is invalid. If either TTBR is enabled, print a warning,
                // otherwise don't bother. If neither TTBR is enabled, we are not going to be
                // performing any translations anyway.
                if self.ttbrs[0].enabled || self.ttbrs[1].enabled {
                    dprintf!(
                        INFO,
                        "WARNING - Bad addressing mode ({}) when decoding TTBR regions for context bank {}\n",
                        self.addr_mode as u32,
                        self.cb_ndx
                    );
                }
            }
        }
    }

    /// Validates that `cb_ndx` is within `[0, smmu.num_cbs())` and marked available in
    /// `smmu.available_cbs()`.
    fn validate_ndx<S: SmmuController>(smmu: &S, cb_ndx: u32) -> Result<(), Status> {
        if cb_ndx >= smmu.num_cbs() {
            return Err(Status::INVALID_ARGS);
        }

        if !smmu.available_cbs().test_bit(cb_ndx) {
            return Err(Status::ALREADY_BOUND);
        }

        Ok(())
    }

    /// Transitions this Context Bank's hardware configuration and bookkeeping to `target_mode`.
    ///
    /// Note: Exclusive access is statically enforced because every live [`ContextBank`] is owned
    /// by `SmmuBti::context_bank` (`#[guarded_by(lock)]`) or exclusively held during `Smmu::init`
    /// before BTI publication, so obtaining `&mut self` requires holding the owning lock (passing
    /// only `smmu_name` for diagnostic logging).
    pub fn set_mode(&mut self, smmu_name: &str, target_mode: BtiMode) {
        // Upper levels should ensure that we never call this method for:
        //
        // 1) Adopted context banks.
        // 2) Context banks which have already shut down.
        // 3) To attempt to move from non-faulting mode -> a different non-faulting mode.
        debug_assert!(self.mode != BtiMode::Adopted);
        debug_assert!(self.mode != BtiMode::Shutdown);
        debug_assert!(
            !((self.mode == BtiMode::Translation && target_mode == BtiMode::Bypass)
                || (self.mode == BtiMode::Bypass && target_mode == BtiMode::Translation))
        );

        // If we are already in the proper mode, no action is needed.
        if self.mode == target_mode {
            return;
        }

        // Start by disabling ourselves at the register level. During operation, we expect to only
        // ever see the following mode transitions:
        //
        // 1) Initial register state (unknown) -> Initial desired mode (fault/bypass/translate)
        // 2) Current mode -> Shutdown
        // 3) Non-faulting mode (bypass/translate) -> Fault mode
        // 4) Fault mode -> Non-faulting mode (bypass/translate)
        //
        // #1 and #2 cover the create/shutdown cases. #3 and #4 cover entering into and exiting
        // from quarantine states respectively.
        //
        // In no situation should we ever go from a non-faulting mode to another non-faulting mode.
        // Because of this, it should always be safe to first unconditionally disable all access at
        // the register level. This is either already the case (because we are in a faulting mode),
        // or it needs to become the case (because we are about to switch to a faulting mode).
        Self::disable_regs(self.gr1_base, self.cb_base, self.cb_ndx);

        // Our TTBRs are now all disabled.
        for ttbr in &mut self.ttbrs {
            if ttbr.enabled {
                ttbr.enabled = false;
            }
        }

        // Make sure that we are configured for 64-bit addressing mode, whether or not we are going
        // to actually perform translation.
        Cba2r::read_at(&self.gr1_base, self.cb_ndx)
            .set_va64(Cba2rVa64::Va64)
            .set_monc(0)
            .set_vmid16(0)
            .write_at(&self.gr1_base, self.cb_ndx);
        self.addr_mode = AddrMode::Mode64Bit;

        // Set up a 48-bit address space with 4k pages, once again whether or not we are going to
        // actually perform translation. Note that we can ask for this configuration all that we
        // want, but both the HW and the Hypervisor/Secure Monitor might choose to override our
        // decision. When attempting to determine the actual address space configuration, be sure
        // to read back the register values instead of assuming that they stuck.
        Tcr64Bit::read_from(&self.cb_base)
            .set_tg0(TcrTg0::Granule4KB) // TTBR0 uses 4k pages
            .set_t0sz(DEFAULT_TCR_T0SZ) // TTBR0 has a 48-bit address space.
            .set_tg1(TcrTg1::Granule4KB) // _If_ we used TTBR1, we'd use 4k pages.
            .set_t1sz(0x3f) // Smallest we can make the TTBR1 aspace size is 2 bytes.
            .write_to(&self.cb_base); // We don't enable TTBR1, so T1SZ is unused anyway.

        // Finally, set up for our new mode of operation.
        match target_mode {
            // In theory, we _should_ already be configured for faulting since `disable_regs` tried
            // to put us into S1TS2Fault mode. That said, systems sometimes do not allow us to
            // configure for this mode. Either the HW or the Hypervisor can block such requests.
            //
            // So, if we are not in S2Fault mode, do this instead. Enable the MMU, but leave page
            // table walking for both of the TTBRs disabled, and configure for S1TS2Bypass.
            // Basically, we are in translate mode, but page table walking is disabled, so all
            // accesses _should_ end up faulting, even though the HW is in translation mode. Said
            // another way, Fault mode is Translate mode but with no valid PTEs.
            BtiMode::Fault => {
                // Temporary porting note: the C++ `SetMode` guarded this block with
                // `#ifdef ZX_DEBUG_ASSERT_IMPLEMENTED`, and that macro is always defined (as 0
                // when debug asserts are off), so the three register reads below happened in
                // every build even though the asserts using them compiled away. Here they
                // happen only in builds with debug assertions (`ZX_ASSERT_LEVEL > 1`), which is
                // what the C++ evidently meant. In other builds the hardware therefore sees
                // three fewer reads (TCR, SCTLR and CBAR) at the end of each transition to
                // fault mode. None of the three has read side effects, and no register is
                // written differently, so nothing about the context bank's behavior changes.
                if cfg!(debug_assertions) {
                    let tcr = Tcr64Bit::read_from(&self.cb_base);
                    debug_assert!(
                        tcr.epd0() == 1 && tcr.epd1() == 1,
                        "Failed to disable page table walking in TCR (0x{:08x}) for Context Bank #{}",
                        tcr.reg_value(),
                        self.cb_ndx
                    );

                    let sctlr = Sctlr::read_from(&self.cb_base);
                    debug_assert!(
                        sctlr.m() == 1,
                        "Failed to enable MMU in SCTLR (0x{:08x}) for Context Bank #{}",
                        sctlr.reg_value(),
                        self.cb_ndx
                    );

                    let cbar = Cbar::read_at(&self.gr1_base, self.cb_ndx);
                    debug_assert!(
                        cbar.cbar_type() == CbarType::S1TS2Bypass,
                        "Failed to configure for S1TS2Bypass CBAR (0x{:08x}) for Context Bank #{}",
                        cbar.reg_value(),
                        self.cb_ndx
                    );
                }
            }

            // We are shutting down, and will not become enabled again. Our registers now belong to
            // our top-level SMMU instance, so go ahead and zero out the base addresses here so
            // that if anything in the ContextBank code attempts to touch the registers, it will
            // cause a fault. Our `Drop` impl checks to confirm that our register addresses have
            // been zeroed as an indication that we have been shut down on the way out.
            BtiMode::Shutdown => {
                self.gr1_base = SmmuMmio::null();
                self.cb_base = SmmuMmio::null();
                self.ttbrs[0].ttbr_paddr = 0;
            }

            // From a disabled state, entering either bypass or translate mode involves setting our
            // CBAR type to S1TS2Bypass. The only difference is that translation mode requires that
            // the MMU be enabled first. There is no need (just now) to allocate any PTEs or enable
            // any TTBRs. That will happen when we pin our first block of memory.
            BtiMode::Translation => {
                CbarS1TS2Bypass::from_value(0)
                    .set_cbar_type(CbarType::S1TS2Bypass) // Bypass mode
                    .set_bpshcfg(3) // Non-shareable
                    //.set_irptndx(0) // We only support SMMUv2, which has an interrupt
                    .write_at(&self.gr1_base, self.cb_ndx); // per context bank, and IRPTNDX is ignored.

                debug_assert!(!self.ttbrs[0].enabled);
                debug_assert_ne!(self.ttbrs[0].ttbr_paddr, 0);

                // Configure the TTBR0 base address and ASID, enable page table walking from TTBR0,
                // but not TTBR1, and finally enable the MMU. Note that we should have already
                // invalidated any TLB entries which were using our ASID during `disable_regs`.
                Ttbr0_64Bit::from_value(0)
                    .set_asid(self.asid() as u64)
                    .set_base_addr_from_value(self.ttbrs[0].ttbr_paddr)
                    .write_to(&self.cb_base);
                Tcr64Bit::read_from(&self.cb_base).set_epd0(0).set_epd1(1).write_to(&self.cb_base);
                Sctlr::from_value(0)
                    .set_m(1) // - MMU Enabled
                    //.set_e(0)     // - Translation tables are expected to use little endian.
                    //.set_cfcfg(0) // - Terminate transactions when a fault occurs.
                    //.set_cfre(0)  // - Do not return an ABORT during a fault. Just RAZ/WI instead.
                    //.set_wxn(0)   // - Disabled Writeable Execute Never
                    //.set_uwxn(0)  // - Disabled Unprivileged Writeable Execute Never
                    .write_to(&self.cb_base);

                self.ttbrs[0].enabled = true;
            }

            BtiMode::Bypass => {
                CbarS1TS2Bypass::from_value(0)
                    .set_cbar_type(CbarType::S1TS2Bypass) // Bypass mode
                    .set_bpshcfg(3) // Non-shareable
                    //.set_irptndx(0) // We only support SMMUv2, which has an interrupt
                    .write_at(&self.gr1_base, self.cb_ndx); // per context bank, and IRPTNDX is ignored.

                Sctlr::from_value(0)
                    //.set_m(0)     // - MMU Disabled
                    //.set_e(0)     // - Translation tables are expected to use little endian.
                    //.set_cfcfg(0) // - Terminate transactions when a fault occurs.
                    //.set_cfre(0)  // - Do not return an ABORT during a fault. Just RAZ/WI instead.
                    //.set_wxn(0)   // - Disabled Writeable Execute Never
                    //.set_uwxn(0)  // - Disabled Unprivileged Writeable Execute Never
                    .write_to(&self.cb_base);
            }

            // We should never be in any of these cases.
            BtiMode::Adopted | BtiMode::Invalid => {
                debug_assert!(
                    false,
                    "{} : Invalid target mode ({}) for context bank {}.\n",
                    smmu_name,
                    bti_mode_to_string(target_mode),
                    self.cb_ndx
                );
                return;
            }
        }

        self.mode = target_mode;
    }

    /// Disables the hardware registers for context bank `cb_ndx` in `gr1_base` and `cb_base`, and
    /// purges any TLB entries tagged with its assigned ASID.
    fn disable_regs(gr1_base: SmmuMmio, cb_base: SmmuMmio, cb_ndx: u32) {
        // Make sure we are configured to use a 64-bit translation table. This will determine the
        // format of the TCR, which we are about to configure.
        //
        // Why is it OK to potentially change the TCR format when it might be in use? Generally
        // speaking, it probably isn't. That said, there are two cases to consider here. This code
        // is running after initialization, or during initialization.
        //
        // If this is happening after initialization, then we are already in VA64 mode as this is
        // what we always configure. So, after-init, this is a no-op.
        //
        // During initialization, we follow a specific sequence for initializing the hardware. The
        // first thing we will do is disable all of the SMRGs, meaning that none of them refer to
        // any context banks. With no context banks in use, we should be free to go through
        // transient configuration states which would otherwise be invalid.
        Cba2r::read_at(&gr1_base, cb_ndx).set_va64(Cba2rVa64::Va64).write_at(&gr1_base, cb_ndx);

        // Unconditionally disable page table walking for both TTBRs. If any transaction makes it
        // to this context bank, we want it to fail, and we definitely do not want it touching
        // stale TTBRs.
        Tcr64Bit::read_from(&cb_base).set_epd0(1).set_epd1(1).write_to(&cb_base);
        SmmuEnv::device_memory_barrier();

        // And explicitly _enable_ the MMU. When the MMU is enabled, but both TTBRs are disabled,
        // translation is being demanded, but no translation table walks can take place,
        // guaranteeing a fault even if we failed to configure this CB for S1TS2Fault.
        //
        // TODO(johngro): Research what the UCI bit _specifically_ controls, and decide if we
        // should be enabling it here, enabling it later, or leaving it disabled at all times.
        Sctlr::from_value(0)
            .set_m(1) // - MMU Enabled
            //.set_e(0)     // - Translation tables are expected to use little endian.
            //.set_uci(0)   // - Cache maintenance operations from EL0 are disabled.
            .set_hupcf(1) // - Process all transactions independently, even with a pending fault IRQ.
            //.set_cfie(0)  // - Disabled context fault interrupts by default.
            //.set_cfcfg(0) // - Terminate transactions when a fault occurs.
            //.set_cfre(0)  // - Do not return an ABORT during a fault. Just RAZ/WI instead.
            //.set_wxn(0)   // - Disabled Writeable Execute Never
            //.set_uwxn(0)  // - Disabled Unprivileged Writeable Execute Never
            .write_to(&cb_base);

        // Configure for S1TS2Bypass. Since we have disabled the TTBRs but enabled the MMU, this
        // should result in a Stage 1 fault for every transaction.
        Cbar::from_value(0).set_cbar_type(CbarType::S1TS2Bypass).write_at(&gr1_base, cb_ndx);

        // Zero out the TTBR address fields and set their ASIDs to a value we will never use
        // (0xFFFF). We've already disabled the TTBRs at the TCR level of things, but this will
        // ensure that no matter what, we never manage to have a transaction hit the TLB cache
        // because the ASIDs will not match.
        Ttbr0_64Bit::read_from(&cb_base)
            .set_base_address(0)
            .set_asid(UNUSED_ASID as u64)
            .write_to(&cb_base);
        Ttbr1_64Bit::read_from(&cb_base)
            .set_base_address(0)
            .set_asid(UNUSED_ASID as u64)
            .write_to(&cb_base);

        // Purge any TLB entries which _might_ still be around using our assigned ASID. This will
        // call `SmmuEnv::device_memory_barrier` (which issues a `dsb sy` instruction on ARM) as a
        // side effect.
        Self::tlb_invalidate_by_asid_at(Self::asid_for_cb_ndx(cb_ndx), &cb_base);
    }

    /// Static helper that validates `cb_ndx` is available in `smmu` and disables its hardware
    /// registers.
    pub fn disable_at<S: SmmuController>(smmu: &S, cb_ndx: u32) {
        debug_assert!(cb_ndx < smmu.num_cbs());
        debug_assert!(smmu.available_cbs().test_bit(cb_ndx));
        Self::disable_regs(smmu.gr1_base(), smmu.get_cb_base(cb_ndx), cb_ndx);
    }

    /// Adopts the pre-existing hardware state of this Context Bank from `smmu`.
    fn adopt_register_state<S: SmmuController>(&mut self, smmu: &S) -> Result<(), Status> {
        // We should only be adopting registers during initialization.
        debug_assert_eq!(self.mode, BtiMode::Invalid);

        // Figure out whether or not this context bank is configured in a way which matches one of
        // our standard modes.
        //
        // Note that we are using the stage-1 translation definition of SCTLR here, even though we
        // have not determined yet if the CBAR has us configured for stage-2-only translation. This
        // should be fine; S2.SCTLR is basically the same as S1, just with a few bits removed. We
        // only really care about SCTLR.M right now, which is the same between the two.
        let cbar = Cbar::read_at(&self.gr1_base, self.cb_ndx);
        let sctlr = Sctlr::read_from(&self.cb_base);

        // We don't understand either S2-only or S1->S2 translation. Basically, we don't understand
        // much of anything about systems which support stage 2 translation, or operating as a
        // virtual guest in such systems. If we are adopting register state which indicates either
        // of these two modes, print a warning.
        if cbar.cbar_type() == CbarType::S1TS2Translate
            || cbar.cbar_type() == CbarType::S2Translation
        {
            dprintf!(
                INFO,
                "{} : WARNING - stage 2 translation enabled in adopted context bank {} (mode {})\n",
                smmu.name(),
                self.cb_ndx,
                arm_cbar_type_to_string(cbar.cbar_type())
            );
        }

        // Regardless of whether or not the MMU is enabled, determine what our addressing mode
        // _would_ be if enabled in order to keep our internal bookkeeping consistent. While the
        // TCR format is technically different between S1 and S2 context banks when in 32-bit
        // addressing mode, the differences don't actually matter to us here as the register is in
        // the same location, and the EAE bit (which is all we care about) is in the same bit
        // position.
        if Cba2r::read_at(&self.gr1_base, self.cb_ndx).va64() == Cba2rVa64::Va64 {
            self.addr_mode = AddrMode::Mode64Bit;
        } else if Tcr::read_from(&self.cb_base).eae() != 0 {
            self.addr_mode = AddrMode::Ext32Bit;
        } else {
            self.addr_mode = AddrMode::Mode32Bit;
        }

        // We currently do not support adopting context banks which have active translation tables.
        // Disabling the MMU for passthru mode is fine, enabling the MMU and disabling both TTBRs
        // for fault mode is also fine. Having actively configured TTBRs, however, implies that the
        // bootloader chain must have set aside _some_ memory for the translation tables.
        //
        // It is not impossible for us to verify that this memory is part of a properly reserved
        // region and will not accidentally be used by the kernel or user-mode, and to arrange a
        // proper handoff as the drivers start up for the first time, but we don't currently have
        // any use cases which require this.
        //
        // For now, we simply ASSERT that this is not the case, unless we are in disabled mode,
        // which should only be used during initial bringup. There are a few different places
        // where we might need to assert this, so we create this small closure to help us out a
        // bit.
        let assert_ttbrs_disabled = |cb_ndx: u32, ttbr0_disabled: bool, ttbr1_disabled: bool| {
            assert!(
                smmu.op_mode() == ArmSmmuMode::Disabled || (ttbr0_disabled && ttbr1_disabled),
                "{} ERROR: SMMU driver is not disabled (mode {}), but adopted Context Bank #{} has {}{}{} enabled!",
                smmu.name(),
                arm_smmu_mode_to_string(smmu.op_mode()),
                cb_ndx,
                if ttbr0_disabled { "" } else { "TRBR0" },
                if ttbr0_disabled || ttbr1_disabled { "" } else { " and " },
                if ttbr1_disabled { "" } else { "TRBR1" }
            );
        };

        // If translation is enabled at all, we may need to print some warnings depending on
        // whether or not either of the TTBRs are enabled for translation. If they are, it implies
        // that we have actual translation tables somewhere in physical memory (at the location
        // specified by the TTBR), and those translation tables had better already be in memory
        // which was reserved, and both unavailable to the physical page allocator, as well as to
        // user mode (from the perspective of creating physical VMOs).
        if sctlr.m() != 0 {
            // Now figure out which of the two TTBRs (if any) are enabled.
            if cbar.cbar_type() == CbarType::S2Translation {
                // Stage 2 translation has only one TTBR, and from what we can tell, translation
                // table walking cannot be turned off. TTBR0 is always valid for S2-only context
                // banks.
                //
                // Fill out the cached TTBR info for the benefit of someone bringing up a new
                // platform who is interested in dumping the configuration using the kernel
                // console.
                self.ttbrs[0].enabled = true;
                match self.addr_mode {
                    AddrMode::Mode64Bit => {
                        let tcr = s2cbr::Tcr64Bit::read_from(&self.cb_base);
                        self.ttbrs[0].granule_size_bits = self.decode_granule_size_bits(tcr.tg0());
                        self.decode_ttbr_regions(tcr.t0sz(), 0);
                    }
                    AddrMode::Ext32Bit => {
                        let tcr = s2cbr::TcrExt32Bit::read_from(&self.cb_base);
                        self.ttbrs[0].granule_size_bits = 12; // Extended 32-bit always uses 4KB.
                        self.decode_ttbr_regions(tcr.t0sz(), 0);
                    }
                    AddrMode::Mode32Bit | AddrMode::Invalid => {
                        dprintf!(
                            INFO,
                            "{} : WARNING - bad addressing mode ({}) in adopted stage-2 only context bank ({})\n",
                            smmu.name(),
                            self.addr_mode as u32,
                            self.cb_ndx
                        );
                    }
                }

                self.ttbrs[0].ttbr_paddr = s2cbr::Ttbr0::read_from(&self.cb_base).base_addr_value();
            } else {
                // All other CBAR types may perform stage 1 translation, depending on the SCTLR::M
                // and TCR::EPD[01] bits.
                match self.addr_mode {
                    AddrMode::Mode64Bit => {
                        let tcr = Tcr64Bit::read_from(&self.cb_base);
                        assert_ttbrs_disabled(self.cb_ndx, tcr.epd0() != 0, tcr.epd1() != 0);

                        self.ttbrs[0].enabled = tcr.epd0() == 0;
                        self.ttbrs[1].enabled = tcr.epd1() == 0;

                        self.ttbrs[0].granule_size_bits = if self.ttbrs[0].enabled {
                            self.decode_granule_size_bits(tcr.tg0())
                        } else {
                            0
                        };
                        self.ttbrs[1].granule_size_bits = if self.ttbrs[1].enabled {
                            self.decode_tg1_granule_size_bits(tcr.tg1())
                        } else {
                            0
                        };

                        self.decode_ttbr_regions(tcr.t0sz(), tcr.t1sz());

                        self.ttbrs[0].ttbr_paddr =
                            Ttbr0_64Bit::read_from(&self.cb_base).base_addr_value();
                        self.ttbrs[1].ttbr_paddr =
                            Ttbr1_64Bit::read_from(&self.cb_base).base_addr_value();
                    }
                    AddrMode::Ext32Bit => {
                        let tcr = TcrExt32Bit::read_from(&self.cb_base);
                        assert_ttbrs_disabled(self.cb_ndx, tcr.epd0() != 0, tcr.epd1() != 0);

                        self.ttbrs[0].enabled = tcr.epd0() == 0;
                        self.ttbrs[1].enabled = tcr.epd1() == 0;

                        self.ttbrs[0].granule_size_bits = 12; // Extended 32-bit always uses 4KB.
                        self.ttbrs[1].granule_size_bits = 12;

                        self.decode_ttbr_regions(tcr.t0sz(), tcr.t1sz());

                        self.ttbrs[0].ttbr_paddr =
                            Ttbr0Ext32Bit::read_from(&self.cb_base).base_addr_value();
                        self.ttbrs[1].ttbr_paddr =
                            Ttbr1Ext32Bit::read_from(&self.cb_base).base_addr_value();
                    }
                    AddrMode::Mode32Bit => {
                        let tcr = Tcr32Bit::read_from(&self.cb_base);
                        assert_ttbrs_disabled(self.cb_ndx, tcr.pd0() != 0, tcr.pd1() != 0);

                        self.ttbrs[0].enabled = tcr.pd0() == 0;
                        self.ttbrs[1].enabled = tcr.pd1() == 0;

                        self.ttbrs[0].granule_size_bits = 12; // 32-bit always uses 4KB.
                        self.ttbrs[1].granule_size_bits = 12;

                        self.decode_ttbr_regions(tcr.t0sz(), 0);

                        self.ttbrs[0].ttbr_paddr =
                            Ttbr0_32Bit::read_from(&self.cb_base).base_addr_value() as u64;
                        self.ttbrs[1].ttbr_paddr =
                            Ttbr1_32Bit::read_from(&self.cb_base).base_addr_value() as u64;
                    }
                    AddrMode::Invalid => {
                        dprintf!(
                            INFO,
                            "{} : WARNING - bad addressing mode ({}) in adopted stage-1 context bank ({})\n",
                            smmu.name(),
                            self.addr_mode as u32,
                            self.cb_ndx
                        );
                    }
                }
            }

            for (i, ttbr) in self.ttbrs.iter().enumerate() {
                if ttbr.enabled {
                    dprintf!(
                        INFO,
                        "{} : WARNING - Translation enabled and TTBR{} valid in adopted context bank ({}).\n",
                        smmu.name(),
                        i,
                        self.cb_ndx
                    );
                    dprintf!(
                        INFO,
                        "{} : WARNING - Make sure that PTE pages starting at TTBR{} paddr 0x{:x} are reserved.\n",
                        smmu.name(),
                        i,
                        ttbr.ttbr_paddr
                    );
                }
            }
        }

        self.mode = BtiMode::Adopted;
        Ok(())
    }

    /// Forces the internal mode of this context bank to `mode` during test teardown (e.g., to
    /// transition an `Adopted` context bank to `Fault` before shutting it down).
    ///
    /// Used by `Smmu::test_shutdown_and_destroy` (`#[cfg(test)]` / host tests) because `set_mode`
    /// asserts that an `Adopted` context bank is never transitioned during normal operation.
    #[cfg(any(ktest, not(is_kernel)))]
    pub(crate) fn force_mode_for_test(&mut self, mode: BtiMode) {
        self.mode = mode;
    }
}
