// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM SMMUv2 Bus Transaction Initiator (`SmmuBti`) implementation.

use core::pin::Pin;
use debug::dprintf;
use fbl::{
    DefaultObjectTag, DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode,
    Recyclable, RefPtr, TrackingSize, UniquePtr, pin_make_ref_counted,
};
use ksync::{KCell, KMutex};
use pin_init::{pin_data, pinned_drop};
use zx_status::Status;

use crate::arm_smmu::common::{
    ALL_PERM_FLAGS, Allocation, ArmSmmuMode, BtiPageLeakReason, DEFAULT_MAX_PAGE_CACHE_ENTRIES,
    DeviceAspace, EscapedName, PAGE_SHIFT, PAGE_SIZE, PinnedVmoSource, RequireContiguousMapping,
    SmmuEnv, SmmuPinnedVmo, SmmuRawSpinLock, TlbInvalidator, ZX_MAX_NAME_LEN, is_page_rounded,
};
use crate::arm_smmu::v2::context_bank::{ContextBank, DEFAULT_TTBR0_ADDR_SPACE_SZ};
use crate::arm_smmu::v2::registers::gr0::{
    S2cr, S2crBypass, S2crFault, S2crTranslation, S2crType, Smr,
};
use crate::arm_smmu::v2::registers::gr1::{
    Cba2r, Cbar, CbarS1TS2Bypass, CbarS1TS2Fault, CbarS1TS2Translate, CbarS2Translation, CbarType,
};
use crate::arm_smmu::v2::registers::s1cbr::{
    Sctlr, Tcr, Tcr2, Tcr32Bit, Tcr64Bit, TcrExt32Bit, Ttbr0_32Bit, Ttbr0_64Bit, Ttbr0Ext32Bit,
    Ttbr1_32Bit, Ttbr1_64Bit, Ttbr1Ext32Bit,
};
use crate::arm_smmu::v2::smmu::{Smmu, SmmuLockGuard, dump_reg};
use crate::arm_smmu::v2::smmu_pmt::{SmmuPmt, SmmuPmtState};
use crate::arm_smmu::v2::stream_match_reg::StreamMatchRegGroup;
use crate::arm_smmu::v2::utils::{
    AddrMode, BtiMode, SmrValue, addr_mode_to_string, arm_cbar_type_to_string,
    arm_s2cr_type_to_string, bti_mode_to_string,
};

/// Bus Transaction Initiator (BTI) backed by an ARM SMMUv2 instance.
///
/// ## Notes on life-cycle and locking
///
/// ### BTI and PMT Reference Cycles and Lifecycles
///
/// An `SmmuBti` is initially held by:
/// 1) Its parent `Smmu`'s `bti_list` (`RefPtr<SmmuBti>`),
/// 2) The user-facing `BusTransactionInitiatorDispatcher` (`RefPtr<SmmuBti>`), and
/// 3) Each child `SmmuPmt` (`SmmuPmt::owner: RefPtr<SmmuBti>`), plus any context-bank IRQ vector
///    associated with the BTI while active (`ContextIrqVector::bti`).
///
/// Each active `SmmuPmt` is held by:
/// 1) `SmmuBti::active_pmt_list` (`RefPtr<SmmuPmt>`), and
/// 2) The user-facing `PinnedMemoryTokenDispatcher` (`RefPtr<SmmuPmt>`).
///
/// Unlike `StubBti`, an `SmmuBti` does not retain `RefPtr<SmmuPmt>` references to quarantined
/// PMTs: when a PMT's final dispatcher handle is closed without first calling `zx_pmt_unpin`
/// (`on_pmt_zero_handles`), the BTI revokes hardware access (either unmapping the PMT's PTEs in
/// `Enforced` mode or placing the context bank into `Fault` mode in `Passthru` mode), removes the
/// PMT from `active_pmt_list`, increments `quarantined_pmt_count` / `quarantined_page_count`, and
/// immediately releases the `SmmuPinnedVmo` pages back to the PMM.
///
/// A BTI transitions through the following lifecycle states:
/// - **Operational (`Bypass` / `Translation` / `Fault`)**: User handles to the BTI dispatcher
///   exist (`orphaned == false`).
/// - **Orphaned (`orphaned == true`)**: The final user handle to the BTI dispatcher was closed
///   (`on_dispatcher_zero_handles`) while `active_pmt_list` was non-empty, or while
///   `quarantined_page_count != 0` in `Passthru` mode. (In `Passthru` mode, an orphaned BTI with
///   quarantined PMTs remains in `Fault` mode permanently so the device cannot access physical
///   memory.)
/// - **End of Life (`Shutdown`)**: Reached either in `on_dispatcher_zero_handles` when not
///   orphaned, or in `on_pmt_zero_handles` when an orphaned BTI's last active PMT is released (and
///   either `quarantined_pmt_count == 0` or the SMMU is in `Enforced` mode). `on_end_of_life`
///   calls `Smmu::shutdown_bti` (which synchronizes and disables the context-bank IRQ, shuts down
///   the BTI's SMRGs, context bank, and `DeviceAspace`, and unlinks the BTI from `Smmu::bti_list`)
///   and clears `self.smmu` so the `SmmuBti` destructs once the last `RefPtr` drops.
///
/// ### Lock Hierarchy
///
/// Locks must be acquired from outermost to innermost in the following order:
/// 1. `Smmu::lock` (`KMutex`)
/// 2. `SmmuBti::pmt_lock` (`KMutex`, shared with child `SmmuPmt` instances via
///    `SmmuBtiPmtLockClass`)
/// 3. `SmmuBti::lock` (`KMutex<SmmuRawSpinLock>`, IRQ-saving spinlock)
/// 4. `Smmu::irq_lock` (`KMutex<SmmuRawSpinLock>`, IRQ-saving spinlock)
#[ksync::guarded]
#[fbl::ref_counted]
#[pin_data(PinnedDrop)]
#[derive(Recyclable)]
#[repr(C)]
pub struct SmmuBti {
    #[pin]
    pub(crate) list_node: DoublyLinkedListNode<SmmuBti>,
    bti_id: u64,

    /// Separate PMT lock class to prevent lock ordering inversions between PMT and BTI locks.
    /// Specifically, BTI needs to iterate over PMTs while holding its lock, and PMT needs to call
    /// back into BTI when unpinned/destroyed. By having a dedicated PMT lock that nests outside
    /// `lock`, we can synchronize PMT list manipulation and quarantine counting without deadlocks.
    #[mutex]
    pub(crate) pmt_lock: KMutex,
    #[pin]
    #[guarded_by(pmt_lock)]
    pub(crate) active_pmt_list: DoublyLinkedList<RefPtr<SmmuPmt>, DefaultObjectTag, TrackingSize>,
    /// Note: We don't need to actually hold references to "quarantined" PMTs. If a user leaks a PMT
    /// without unpinning it, we simply enter the fault state and lock down this BTI so that the
    /// actual initiator hardware cannot access any memory anymore, after which we can return all
    /// pinned physical memory to the global pool. We do need to maintain a count of the number of
    /// virtually quarantined PMTs, however, in order to comply with the `get_info` reporting rules.
    #[guarded_by(pmt_lock)]
    pub(crate) quarantined_pmt_count: u64,
    #[guarded_by(pmt_lock)]
    pub(crate) quarantined_page_count: u64,
    /// A flag indicating that the final user-mode handle to this BTI was closed, but it still had
    /// PMTs (either active or quarantined) under its control.
    #[guarded_by(pmt_lock)]
    pub(crate) orphaned: bool,
    /// Protected by `pmt_lock` and its own internal page-table/page-cache state.
    #[guarded_by(pmt_lock)]
    pub(crate) aspace: Option<kalloc::Box<DeviceAspace>>,

    #[mutex]
    pub(crate) lock: KMutex<SmmuRawSpinLock>,
    /// Reference to the parent `Smmu`. Guarded by `lock` as an `Option<RefPtr<Smmu>>` so
    /// `on_end_of_life` can soundly clear the parent reference without `const_cast` or interior
    /// mutability outside the lock.
    #[guarded_by(lock)]
    pub(crate) smmu: Option<RefPtr<Smmu>>,
    #[guarded_by(lock)]
    pub(crate) mode: BtiMode,
    #[pin]
    #[guarded_by(lock)]
    pub(crate) smrg_list:
        DoublyLinkedList<UniquePtr<StreamMatchRegGroup>, DefaultObjectTag, TrackingSize>,
    #[guarded_by(lock)]
    pub(crate) context_bank: Option<UniquePtr<ContextBank>>,
    #[guarded_by(lock)]
    pub(crate) name: [u8; ZX_MAX_NAME_LEN],
}

// Manual implementation because `#[pin_data]` copies field attributes onto its internal
// projection struct literal, where derive helper attributes like `#[dll_node]` are rejected.
impl DoublyLinkedListContainable<Self> for SmmuBti {
    #[inline]
    fn get_node(&self) -> &DoublyLinkedListNode<Self> {
        &self.list_node
    }
}

// SAFETY: All mutable fields of `SmmuBti` are synchronized by `pmt_lock`, `lock`, or the enclosing
// `Smmu::lock` (for `list_node`).
unsafe impl Send for SmmuBti {}
// SAFETY: Shared references to `SmmuBti` only mutate state under `pmt_lock` or `lock`.
unsafe impl Sync for SmmuBti {}

struct BtiTlbInvalidator<'a> {
    bti: &'a SmmuBti,
}

impl TlbInvalidator for BtiTlbInvalidator<'_> {
    #[inline]
    fn invalidate(&mut self, base: u64, size: u64) {
        self.bti.tlb_inval_thunk(base, size);
    }
}

struct NoOpTlbInvalidator;

impl TlbInvalidator for NoOpTlbInvalidator {
    #[inline]
    fn invalidate(&mut self, _base: u64, _size: u64) {}
}

#[pinned_drop]
impl PinnedDrop for SmmuBti {
    fn drop(self: Pin<&mut Self>) {
        // By the time we destruct, we should have been shut down already and all of
        // our resources explicitly released.
        //
        // No locks are taken here. `drop` may run wherever the last reference happens to be
        // released, and it has exclusive access to `self`, so the guarded fields are read
        // directly.
        //
        // SAFETY: We are in `PinnedDrop::drop`, so nothing else can reference `self`. None of the
        // fields are moved; the lists are only inspected and cleared in place.
        let this = unsafe { self.get_unchecked_mut() };
        debug_assert!(this.smmu.get_inner_mut().is_none());
        debug_assert!(this.context_bank.get_inner_mut().is_none());
        debug_assert!(this.smrg_list.get_inner_mut().is_empty());
        debug_assert!(this.active_pmt_list.get_inner_mut().is_empty());
        debug_assert!(this.aspace.get_inner_mut().is_none());
        // SAFETY: `this` came from the pinned `self`, so neither list has moved or will.
        unsafe {
            Pin::new_unchecked(this.active_pmt_list.get_inner_mut()).clear();
            Pin::new_unchecked(this.smrg_list.get_inner_mut()).clear();
        }
    }
}

impl SmmuBti {
    /// Returns the bus transaction ID for this BTI.
    #[inline]
    pub fn bti_id(&self) -> u64 {
        self.bti_id
    }

    /// Returns `true` if this BTI is currently linked in `smmu.bti_list`.
    #[inline]
    pub fn in_container(&self) -> bool {
        self.list_node.in_container()
    }

    /// Returns a displayable form of a BTI `name` buffer (up to its first NUL byte).
    ///
    /// BTI names are arbitrary user-controlled bytes, so they are printed via [`EscapedName`],
    /// which escapes anything that is not valid UTF-8.
    pub(crate) fn name_display(name: &[u8; ZX_MAX_NAME_LEN]) -> EscapedName<'_> {
        EscapedName::new(name)
    }

    /// Creates a `DeviceAspace` for a translation-mode BTI.
    pub(crate) fn create_aspace() -> Result<kalloc::Box<DeviceAspace>, Status> {
        // Allocations start at the second L3 page (index == 1 at L2) in the page
        // tables. This is 2MB when page size is 4KB.
        //
        // All of our mappings are going to be done using the page tables specified by
        // TTBR0, we are not going to be using TTBR1. By default, given the way that
        // we configure the hardware, this means that TTBR0 will control the mappings
        // in the device address space on the range from `[0, DEFAULT_TTBR0_ADDR_SPACE_SZ)`.
        //
        // The actual range controlled by the `DeviceAspace` instance, however, will be
        // `[2MB, DEFAULT_TTBR0_ADDR_SPACE_SZ)` ==
        // `[LOWER_ASPACE_BOUND, DEFAULT_TTBR0_ADDR_SPACE_SZ)`.
        //
        // To construct the `DeviceAspace` object, we need to pass the base address and
        // the size of the region to manage. The base address is `LOWER_ASPACE_BOUND`,
        // but the size of the managed space ends up being
        // `DEFAULT_TTBR0_ADDR_SPACE_SZ - LOWER_ASPACE_BOUND`. Statically assert that
        // this is computable before passing the value on to `DeviceAspace::create`.
        const _: () =
            assert!(PAGE_SIZE == 4096, "SMMU Device Address Space manager assumes 4k pages");
        const LOWER_ASPACE_BOUND: u64 = PAGE_SIZE * 512;
        const _: () = assert!(LOWER_ASPACE_BOUND < DEFAULT_TTBR0_ADDR_SPACE_SZ);
        const DEV_ASPACE_SIZE: u64 = DEFAULT_TTBR0_ADDR_SPACE_SZ - LOWER_ASPACE_BOUND;

        DeviceAspace::create(LOWER_ASPACE_BOUND, DEV_ASPACE_SIZE, DEFAULT_MAX_PAGE_CACHE_ENTRIES)
    }

    /// Creates and initializes a new `SmmuBti` instance, taking control of `smrg` and (optional)
    /// `context_bank`.
    ///
    /// On success, the supplied resources (`smrg` and `context_bank`) are marked as unavailable in
    /// the owning `Smmu`'s bookkeeping.
    ///
    /// Temporary porting note: the C++ `SmmuBti::Create` marked the SMRG and context bank as in
    /// use in the SMMU's bookkeeping *before* its fallible steps. If creating the address space
    /// then failed, it returned without undoing that, permanently leaking one SMRG and one context
    /// bank, and destroyed objects which had not been shut down (tripping their destructors' debug
    /// asserts). The ordering below avoids both.
    ///
    /// On success the register accesses, and their order, are the same as the C++. On the two
    /// failure paths (no memory for the address space or for the BTI) the C++ wrote no registers
    /// at all, and this does:
    ///
    /// 1) The SMRG is disabled: SMR read and write with VALID, MASK and ID zero, S2CR read and
    ///    write with TYPE = Fault, then a barrier. From `Smmu::create_bti` this repeats what
    ///    `create_and_lockdown` just wrote, so no register changes value. From
    ///    `Smmu::adopt_smrg` (`initial_mode` is `Adopted`, and only the BTI allocation can fail)
    ///    it turns off a stream match which the bootloader had enabled; the consequences are the
    ///    three listed in the porting note in `adopt_smrg`, and the same evaluation applies.
    /// 2) A context bank which is not adopted goes through `set_mode(Shutdown)`: a second
    ///    `disable_regs` (TCR EPD0/EPD1 set, barrier, SCTLR, CBAR, TTBR0, TTBR1, TLBIASID and
    ///    sync), then a read and write each of CBA2R and TCR, which the C++ never touched on this
    ///    path. This is exactly the sequence both implementations run whenever a BTI is shut
    ///    down, so the context bank ends in the state every released context bank is in:
    ///    faulting, with table walks disabled. The SMRG is disabled throughout, so no stream can
    ///    reach the context bank while it is rewritten. An adopted context bank is not touched
    ///    here, as with the C++; `Smmu::lockdown` disables it afterwards in both, as it is still
    ///    marked available.
    ///
    /// This is at least as good as the C++: every register ends in a faulting state the C++ also
    /// produces, the only cost is some redundant accesses on a path which needs an allocation to
    /// fail, and the SMRG and context bank remain available for a later BTI instead of being lost
    /// until reboot. The one exception is consequence 3) in `adopt_smrg`.
    ///
    /// Note: Fallible allocations (`Self::create_aspace()` and `pin_make_ref_counted!`) are
    /// performed upfront before clearing `available_cbs` / `available_smrgs` or linking hardware
    /// resources into `bti`. This avoids dropping `bti.lock` mid-initialization while interacting
    /// with the heap/PMM and guarantees that if allocation fails, `available_cbs` and
    /// `available_smrgs` remain untouched while `smrg` and `context_bank` are cleanly disabled and
    /// dropped.
    pub(crate) fn create(
        smmu: &Smmu,
        smmu_guard: &mut Pin<&mut SmmuLockGuard<'_>>,
        mut smrg: UniquePtr<StreamMatchRegGroup>,
        mut context_bank: Option<UniquePtr<ContextBank>>,
        initial_mode: BtiMode,
    ) -> Result<RefPtr<Self>, Status> {
        // We always use the raw value of our first SMRG's match register as our
        // opaque "bti id".
        let bti_id = smrg.stream_ids().value() as u64;

        // If we are supposed to be operating in translation mode, we need to allocate
        // and initialize a device address space in order to implement the bookkeeping
        // we need for mapping memory.
        //
        // Note that we cannot hold our spinlock during aspace initialization as we will need to
        // interact with the heap and the PMM in the process of doing so. Because we are in the
        // process of constructing this BTI instance, no other threads (nor any interrupt handlers)
        // can know about it yet; the first time another concurrent execution context could become
        // aware of us is when we enable context bank interrupts at the end of initialization below.
        let maybe_aspace = if initial_mode == BtiMode::Translation {
            match Self::create_aspace() {
                Ok(a) => Some(a),
                Err(status) => {
                    dprintf!(
                        INFO,
                        "ERROR - failed to construct aspace for BTI ({})\n",
                        status.into_raw()
                    );
                    smrg.disable();
                    if let Some(ref mut cb) = context_bank
                        && cb.mode() != BtiMode::Adopted
                    {
                        cb.set_mode(smmu.name(), BtiMode::Shutdown);
                    }
                    return Err(status);
                }
            }
        } else {
            None
        };

        let bti = match pin_make_ref_counted!(SmmuBti {
            list_node: DoublyLinkedListNode::new(),
            bti_id,
            pmt_lock <- KMutex::init(),
            active_pmt_list <- KCell::pin_init(DoublyLinkedList::new()),
            quarantined_pmt_count: KCell::new(0),
            quarantined_page_count: KCell::new(0),
            orphaned: KCell::new(false),
            aspace: KCell::new(None),
            lock <- KMutex::init(),
            smmu: KCell::new(Some(RefPtr::from_ref(smmu))),
            mode: KCell::new(BtiMode::Shutdown),
            smrg_list <- KCell::pin_init(DoublyLinkedList::new()),
            context_bank: KCell::new(None),
            name: KCell::new([0u8; ZX_MAX_NAME_LEN]),
        }) {
            Ok(bti) => bti,
            Err(_) => {
                if let Some(mut aspace) = maybe_aspace {
                    aspace.free_translation_tables(&mut NoOpTlbInvalidator);
                }
                smrg.disable();
                if let Some(ref mut cb) = context_bank
                    && cb.mode() != BtiMode::Adopted
                {
                    cb.set_mode(smmu.name(), BtiMode::Shutdown);
                }
                return Err(Status::NO_MEMORY);
            }
        };

        {
            ksync::lock!(let mut pmt_guard = bti.lock_pmt_lock());
            ksync::lock!(let mut guard = bti.lock_lock());

            // If this hardware is being adopted, set our initial mode to adopted before
            // proceeding. This will prevent `set_mode_locked` from attempting to
            // reconfigure the hardware. Adopted hardware is supposed to stay in the
            // state it was in when handed to us by the bootloader, and should not even
            // actually change state.
            if initial_mode == BtiMode::Adopted {
                debug_assert!(context_bank.as_ref().is_none_or(|cb| cb.mode() == BtiMode::Adopted));
                *guard.as_mut().mode_mut() = BtiMode::Adopted;
            } else {
                // Temporarily set mode to Invalid so `set_mode_locked` can transition to
                // `initial_mode` (since `Shutdown` is not allowed to transition out).
                *guard.as_mut().mode_mut() = BtiMode::Invalid;
            }

            if let Some(mut cb) = context_bank {
                let cb_ndx = cb.cb_ndx();
                debug_assert!(smmu_guard.available_cbs().test_bit(cb_ndx));
                smmu_guard.as_mut().available_cbs_mut().clear_bit(cb_ndx);
                if let Some(aspace) = maybe_aspace {
                    cb.attach_aspace(&aspace);
                    *pmt_guard.as_mut().aspace_mut() = Some(aspace);
                }
                *guard.as_mut().context_bank_mut() = Some(cb);
            }

            bti.add_smrg_locked(smmu_guard, &mut guard, smrg);

            // In theory, setting the initial mode can never fail. All of the resources
            // which might be needed (the SMRG and Context Bank) have already been
            // allocated, we just need to set up their registers. No PTE pages ever
            // have to be allocated at this point in time as we will (initially) have no
            // pinned memory.
            let res = bti.set_mode_locked(&mut guard, initial_mode);
            debug_assert!(res.is_ok());

            // If we have a context bank, associate ourselves with our context bank's
            // interrupt, then enable the interrupt at the context bank level.
            if let Some(ref mut cb) = *guard.as_mut().context_bank_mut() {
                let cb_ndx = cb.cb_ndx();
                smmu.associate_bti_irq(&bti, cb_ndx);
                cb.enable_fault_interrupt();
            }
        }

        Ok(bti)
    }

    /// Maps a pinned VMO into this BTI and returns the resulting `SmmuPmt`.
    pub fn map(
        &self,
        pinned_vmo: SmmuPinnedVmo,
        perms: u32,
        req_contig: RequireContiguousMapping,
    ) -> Result<RefPtr<SmmuPmt>, Status> {
        let vmo_offset = pinned_vmo.offset();
        let vmo_size = pinned_vmo.size();

        // Start by checking the basics of our request. IOW - the stuff we can
        // validate without needing to lock anything.
        if !is_page_rounded(vmo_offset) || vmo_size == 0 {
            return Err(Status::INVALID_ARGS);
        }

        if (perms & !ALL_PERM_FLAGS) != 0 || perms == 0 {
            return Err(Status::INVALID_ARGS);
        }

        // TODO(https://fxbug.dev/520619827): Resolve this. Right now, DWC3 tries to
        // make write-only mappings. If we tell them no, things spiral out of control
        // rapidly:
        //
        // 1) The driver does not come up. This is obvious and expected.
        // 2) Someone ends up closing their BTI. This is not right. DF is supposed
        //    to be escrowing these things, no matter what happens with the driver who
        //    uses the BTI.
        // 3) Then, something goes wrong in the driver system such that an attempt to
        //    `dm reboot` just hangs the shell.
        //
        // We need to figure out the proper policy here. If someone wants a
        // particular level of enforcement, and we cannot provide it, what should we
        // be doing? Deny the request and allow them to retry? Silently accept the
        // request and give them a less strict level of access? Allow them to pass a
        // flag which determines if they want strict or lax enforcement?
        //
        // Note: It is not possible for an SMMU using VMSAv8-64 translation tables to
        // create permissions which allow write, but do not allow read. When resolved,
        // write-without-read requests (`(perms & PERM_READ) == 0 && (perms & PERM_WRITE) != 0`)
        // would return `Err(Status::INVALID_ARGS)`.

        // If there has been a request for a contiguous mapping, then check to see if
        // our pinned VMO is (in fact) contiguous before attempting to obtain our
        // lock. Once we are holding our lock, we can safely check to see if we are
        // operating in Bypass mode, where we will need to fail the operation if the
        // user desires a contiguous mapping, but the pinned VMO is not contiguous.
        let contiguous_vmo_status = if req_contig == RequireContiguousMapping::No {
            Ok(())
        } else {
            pinned_vmo.lookup_contiguous(vmo_offset, vmo_size).map(|_| ())
        };

        // Hold our spinlock while we check to make sure we are in a state where
        // mapping is possible. IOW - we should be in either Bypass or Translation
        // mode. If we are in something like fault mode, adopted mode, or shutdown mode,
        // we are likely to stay there for a long time (if not forever), and it is
        // worth it to make a quick check to be sure that there is a chance that this
        // request can succeed before going further.
        //
        // After that, speculatively allocate an `SmmuPmt` object with our spinlock
        // dropped (we can't hold a spinlock and touch the heap).
        //
        // After this, however, we need to re-enter our lock and re-validate our mode.
        // It is possible that while we were off fetching memory from the heap, we
        // took a context interrupt and entered the fault state. In a situation like
        // this, we need to deny the map request as we are required to do when a BTI
        // is faulted.
        let observed_mode = {
            ksync::lock!(let guard = self.lock_lock());
            let mode = *guard.mode();
            if mode != BtiMode::Bypass && mode != BtiMode::Translation {
                return Err(Status::BAD_STATE);
            }

            // If we are not in translation mode, and the user both wants a contiguous
            // mapping but the underlying pinned VMO is not contiguous (computed in
            // `contiguous_vmo_status`, above) then we need to bail out here.
            if mode != BtiMode::Translation {
                contiguous_vmo_status?;
            }

            mode
        };

        // Perform our mapping if we are in translation mode. Then, create our
        // PMT, giving it ownership of any new mapping we just created.
        let (pmt, mode_changed) = {
            ksync::lock!(let mut pmt_guard = self.lock_pmt_lock());
            let mapped_location = if observed_mode == BtiMode::Translation {
                let mut inv = BtiTlbInvalidator { bti: self };
                let aspace = pmt_guard
                    .as_mut()
                    .aspace_mut()
                    .as_mut()
                    .expect("translation BTI must have aspace");
                // Our `DeviceAspace` updates our page tables, flushes the updates out of the CPU
                // data cache, and invalidates the TLBs. Take ownership of our new mapping and pass
                // it along to the PMT we are about to create.
                Some(aspace.map(&pinned_vmo, perms, &mut inv, None)?)
            } else {
                None
            };

            // Create our PMT and give it our mapped location (if any). If something goes wrong,
            // the factory method will take care of removing the mapping via our
            // `release_mapping_locked` method.
            // SAFETY: `pmt_guard` holds `self.pmt_lock`.
            let pmt = unsafe {
                SmmuPmt::create(self, &mut pmt_guard, pinned_vmo, observed_mode, mapped_location)
            }
            .ok_or(Status::NO_MEMORY)?;

            // Now enter our lock and attempt to finish the mapping operation. We need to
            // hold the state of the BTI constant during the rest of the process.
            let mode_changed = {
                ksync::lock!(let guard = self.lock_lock());
                // We made it this far because we observed our BTI as being in either the
                // Bypass or Translation state. There is no legal way for a BTI to change from
                // Bypass to Translation (or vice-versa) after being created, so if the
                // current state does not match our observed state, then we must have either
                // faulted or been shut down and will need to bail out. The VMO which had
                // been pinned has been transferred to our PMT and will be unpinned
                // automatically as the PMT drops.
                //
                // Note that this assumption is predicated on the notion that the Dispatcher
                // level is holding locks which prevent us from:
                //
                // 1) Faulting at an IRQ level and having the driver-level BTI enter the fault
                //    state, then
                // 2) Recovering from the fault with a syscall call to `release_quarantine` while
                //    we were creating the PMT object.
                if *guard.mode() != observed_mode {
                    true
                } else {
                    // Add the PMT to the list of active PMTs, and mark it as being in the `Active`
                    // state.
                    // SAFETY: `pmt` was created above with `self` as its owner, and `pmt_guard`
                    // holds `self.pmt_lock`.
                    *unsafe { pmt.guard_pmt_lock_mut(pmt_guard.as_mut().token_mut()) }
                        .state_mut() = SmmuPmtState::Active;
                    pmt_guard.as_mut().active_pmt_list_mut().push_back(pmt.clone());
                    false
                }
            };
            if mode_changed {
                // The PMT never became active, so it still owns the mapping we just made. Give
                // the mapping back now, while we hold `pmt_lock` (and after the spinlock above
                // has been dropped, as TLB invalidation takes it). `SmmuPmt::drop` takes no
                // locks, so it cannot do this for us.
                // SAFETY: `pmt` was created above with `self` as its owner, and `pmt_guard` holds
                // `self.pmt_lock`.
                let mapping = unsafe { pmt.guard_pmt_lock_mut(pmt_guard.as_mut().token_mut()) }
                    .map_location_mut()
                    .take();
                self.release_mapping_locked(&mut pmt_guard, mapping);
            }
            (pmt, mode_changed)
        };
        if mode_changed {
            // Dropping the PMT unpins the VMO, which we do outside of `pmt_lock`.
            drop(pmt);
            return Err(Status::BAD_STATE);
        }

        Ok(pmt)
    }

    /// Unmaps a previously mapped `Allocation` under `pmt_lock`.
    pub(crate) fn release_mapping_locked(
        &self,
        pmt_guard: &mut Pin<&mut SmmuBtiPmtLockGuard<'_>>,
        mapping: Option<Allocation>,
    ) {
        if let Some(alloc) = mapping {
            // Give the mapping back to our `DeviceAspace`, which will manage the process
            // of updating the translation table PTEs and flushing the CPU caches as
            // well as invalidating the TLBs (using the callback we provide).
            let mut inv = BtiTlbInvalidator { bti: self };
            let aspace =
                pmt_guard.as_mut().aspace_mut().as_mut().expect("translation BTI must have aspace");
            aspace.unmap(alloc, &mut inv);
        }
    }

    fn tlb_inval_thunk(&self, base: u64, size: u64) {
        // This callback is invoked from `DeviceAspace` code after it has made some number of
        // modifications to the page tables as they currently exist in RAM. Before it calls into
        // our thunk to invalidate TLBs, it must flush the entries it modified from the CPU cache
        // out to physical memory via `SmmuEnv::clean_dcache_range`.
        //
        // On ARM, `clean_dcache_range` ends with a `dsb sy` instruction, which ensures that the
        // cache flushes have all finished before proceeding, instead of potentially lingering in
        // the execution pipeline waiting to finish. With all of the changes propagated to physical
        // memory, we are safe to begin invalidating the TLB entries for the updated region of the
        // device's address space.
        //
        // We should always have a context bank whenever we are being asked to
        // invalidate our TLBs by our `DeviceAspace`.
        ksync::lock!(let guard = self.lock_lock());
        let cb = guard
            .context_bank()
            .as_ref()
            .expect("context_bank must be present during TLB invalidation");

        // No size means flush everything. Otherwise, just flush the requested range.
        if size == 0 {
            cb.tlb_invalidate_by_asid();
        } else {
            cb.tlb_invalidate_region(base, size);
        }
    }

    /// Releases all quarantined PMTs on this BTI and restores operational mode if faulted.
    pub fn release_quarantine(&self) {
        let smmu = {
            ksync::lock!(let mut pmt_guard = self.lock_pmt_lock());
            ksync::lock!(let mut guard = self.lock_lock());

            let smmu = guard.smmu().as_ref().expect("active BTI must have SMMU").clone();
            let op_mode = smmu.op_mode();
            // The mode we will want to return to depends on the mode our SMMU is operating in.
            let target_mode = match op_mode {
                ArmSmmuMode::Passthru => BtiMode::Bypass,
                ArmSmmuMode::Enforced => BtiMode::Translation,
                ArmSmmuMode::Disabled => {
                    panic!("release_quarantine called when SMMU is Disabled");
                }
            };

            // No one should be calling `release_quarantine` on us unless we are in one of
            // the 3 operational states: Fault, Bypass, or Translation.
            let mode = *guard.mode();
            debug_assert!(
                mode == BtiMode::Fault || mode == BtiMode::Bypass || mode == BtiMode::Translation
            );

            // `release_quarantine` should be idempotent. If we are already operating in
            // Bypass or Translation, there is nothing to do.
            if mode == BtiMode::Fault {
                // Reset our "quarantine" bookkeeping.
                *pmt_guard.as_mut().quarantined_pmt_count_mut() = 0;
                *pmt_guard.as_mut().quarantined_page_count_mut() = 0;

                let res = self.set_mode_locked(&mut guard, target_mode);
                debug_assert!(res.is_ok());
            }

            smmu
        };

        // Re-enable our context bank interrupt if needed.
        smmu.reenable_context_bank_irq(self);
    }

    /// Handles the owning `BusTransactionInitiatorDispatcher` reaching zero handles.
    pub fn on_dispatcher_zero_handles(&self) {
        // Start by checking to see if we still have any PMTs, either active or
        // quarantined. If we do, we cannot actually fully shut down yet. Instead,
        // we have become "orphaned".
        //
        // If we still have active PMTs (nothing quarantined) we have to stick around,
        // allowing our HW to access the memory it has pinned until it formally unpins
        // the last PMT it had pinned. Only then can we fully shut down.
        //
        // If we have quarantined memory, the situation is slightly worse, at least if
        // we are in passthru mode. If a PMT has been leaked, and its BTI is
        // orphaned, there is no way for user-mode to tell the kernel that it has
        // taken control of its hardware and that it is now safe to un-quarantine its
        // quarantined PMTs. Typically that would happen with a call to
        // `zx_bti_release_quarantine`, but the last handle to the BTI has been closed
        // preventing user-mode from properly following the protocol.
        //
        // In passthru mode, we can only grant or deny a device access to *all* of
        // physical memory, not only portions of it. This is good in-so-much as the
        // hardware can be prevented from accessing any memory (allowing us to return
        // quarantined memory to the physical pool) however we need to stay in
        // deny/fault mode pretty much forever as there is no way for user-mode to
        // give us an all-clear sign given that we are orphaned.
        //
        // We should be able to do better in enforced mode. Once all of the active
        // PMTs have either been formally unpinned, or even leaked and quarantined, we
        // should be able to destroy the BTI instance even if it has been orphaned.
        // The PTE entries will already have been removed and the HW will not be able
        // to access any memory. If a new instance happens to be re-created, newly
        // pinned memory will only be addressable on a per-physical-page basis, and
        // the newly created BTI will not have any valid PTEs, so the hardware will
        // not be able to access any memory, at least not until it successfully pins
        // new pages. It is very important for a driver to make certain that it has
        // stopped all DMA before pinning any new memory. Failure to do so risks the
        // driver either leaking or corrupting its newly pinned memory because of its
        // out of control DMA.
        let (was_orphaned, quarantined_page_count, quarantined_pmt_count) = {
            ksync::lock!(let mut pmt_guard = self.lock_pmt_lock());
            ksync::lock!(let guard = self.lock_lock());
            debug_assert!(!*pmt_guard.orphaned());

            let q_pmt = *pmt_guard.quarantined_pmt_count();
            let q_page = *pmt_guard.quarantined_page_count();

            // These must either both be zero, or both be non-zero.
            debug_assert_eq!(q_pmt == 0, q_page == 0);

            let smmu = guard.smmu().as_ref().expect("active BTI must have SMMU");
            let orphaned = if q_page != 0 && smmu.op_mode() != ArmSmmuMode::Enforced {
                // If we have quarantined PMTs, we should already be in the Fault state.
                debug_assert_eq!(*guard.mode(), BtiMode::Fault);
                true
            } else {
                // If we have no (logically) quarantined PMTs, or we are operating in
                // Enforced mode, then we are orphaned iff we have active PMTs which are
                // still alive.
                !pmt_guard.active_pmt_list().is_empty()
            };

            *pmt_guard.as_mut().orphaned_mut() = orphaned;
            (orphaned, q_page, q_pmt)
        };

        if !was_orphaned {
            // If we were not orphaned, then we have no active PMTs and no quarantined
            // PMTs, and user-mode no longer has any handles to us. We can now safely
            // shut down and remove ourselves from our parent SMMU instance.
            self.on_end_of_life();
        } else {
            // If we are now orphaned because we have quarantined PMTs, we need to log a
            // DRIVER OOPS warning.
            self.print_quarantine_warning(
                BtiPageLeakReason::BtiOrphanedWithQuarantinedPmts,
                quarantined_page_count,
                quarantined_pmt_count as usize,
            );
        }
    }

    /// Conducts final cleanup when this BTI reaches the end of its life.
    ///
    /// There are two possible ways to get here:
    /// 1) The final user-mode handle to the BTI is closed, and the BTI does not have any active or
    ///    quarantined PMTs (`on_dispatcher_zero_handles`).
    /// 2) The BTI is orphaned (its final handle was closed while it still held PMTs), however the
    ///    final PMT has now been unpinned and its final handle closed, and there are no
    ///    quarantined PMTs (`on_pmt_zero_handles`).
    pub(crate) fn on_end_of_life(&self) {
        // We should never be calling `on_end_of_life` twice.
        let smmu = {
            ksync::lock!(let guard = self.lock_lock());
            guard.smmu().as_ref().expect("on_end_of_life called twice").clone()
        };
        smmu.shutdown_bti(self);
        {
            ksync::lock!(let mut guard = self.lock_lock());
            *guard.as_mut().smmu_mut() = None;
        }
    }

    fn print_quarantine_warning(
        &self,
        reason: BtiPageLeakReason,
        total_leaked_pages: u64,
        total_leaked_vmos: usize,
    ) {
        let name_buf = {
            ksync::lock!(let guard = self.lock_lock());
            *guard.name()
        };
        SmmuEnv::print_quarantine_warning(
            self.bti_id,
            &name_buf,
            reason,
            total_leaked_pages,
            total_leaked_vmos,
        );
    }

    /// Returns the minimum contiguity guaranteed by this BTI.
    pub fn minimum_contiguity(&self) -> u64 {
        let op_mode = {
            ksync::lock!(let guard = self.lock_lock());
            guard.smmu().as_ref().expect("BTI must have SMMU").op_mode()
        };
        match op_mode {
            // If we are operating in passthru mode, then we are not actually doing any
            // translation. We cannot guarantee any contiguity beyond the CPUs' page
            // size.
            ArmSmmuMode::Passthru => PAGE_SIZE,

            // If we are operating in fully-enforced mode, then we currently guarantee
            // that all of our mappings will be made contiguous from the perspective of
            // the initiator HW.
            ArmSmmuMode::Enforced => u64::MAX,

            // It should be impossible for our SMMU to be in Disabled mode. If the
            // bootloaders configured the SMMUs to operate in Disabled mode, then we
            // should never have created an `Smmu` instance, and therefore should not
            // have been able to create any `SmmuBti` instances either (we should have
            // created a `StubIommu` instance instead).
            ArmSmmuMode::Disabled => {
                debug_assert!(false);
                0
            }
        }
    }

    /// Returns the virtual address space size managed by this BTI.
    pub fn aspace_size(&self) -> u64 {
        ksync::lock!(let guard = self.lock_lock());
        let Some(ref smmu) = *guard.smmu() else {
            debug_assert!(false);
            return 0;
        };
        match smmu.op_mode() {
            // If we are operating in passthru mode, then the address space that
            // initiator HW sees is going to be the physical address space of the
            // system, which is functionally unlimited in size.
            ArmSmmuMode::Passthru => u64::MAX,

            // If we are operating in fully-enforced mode, then the address space that
            // initiator HW sees depends on this BTI's context bank's configuration.
            ArmSmmuMode::Enforced => {
                guard.context_bank().as_ref().expect("Enforced BTI must have CB").aspace_size()
            }

            // See above, it should be impossible to get here.
            ArmSmmuMode::Disabled => {
                debug_assert!(false);
                0
            }
        }
    }

    /// Returns the total number of active and quarantined PMTs on this BTI.
    pub fn pmo_count(&self) -> u64 {
        ksync::lock!(let pmt_guard = self.lock_pmt_lock());
        (pmt_guard.active_pmt_list().len() as u64) + *pmt_guard.quarantined_pmt_count()
    }

    /// Returns the number of quarantined PMTs on this BTI.
    pub fn quarantine_count(&self) -> usize {
        ksync::lock!(let pmt_guard = self.lock_pmt_lock());
        *pmt_guard.quarantined_pmt_count() as usize
    }

    /// Returns whether this BTI is currently in `BtiMode::Fault`.
    pub fn in_fault_state(&self) -> bool {
        ksync::lock!(let guard = self.lock_lock());
        // It should not be possible to get here unless we are in an operational state.
        let mode = *guard.mode();
        debug_assert!(
            mode == BtiMode::Fault || mode == BtiMode::Bypass || mode == BtiMode::Translation
        );
        mode == BtiMode::Fault
    }

    /// Copies the BTI's debug name into `out_name`.
    pub fn get_name(&self, out_name: &mut [u8; ZX_MAX_NAME_LEN]) {
        ksync::lock!(let guard = self.lock_lock());
        *out_name = *guard.name();
    }

    /// Sets the BTI's debug name from `name`.
    pub fn set_name(&self, name: &[u8]) {
        ksync::lock!(let mut guard = self.lock_lock());
        let mut buf = [0u8; ZX_MAX_NAME_LEN];
        let nul_len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        let copy_len = nul_len.min(ZX_MAX_NAME_LEN - 1);
        buf[..copy_len].copy_from_slice(&name[..copy_len]);
        *guard.as_mut().name_mut() = buf;
    }

    /// Shuts down this BTI's hardware resources and returns its SMRGs and ContextBank to the SMMU.
    pub(crate) fn shutdown(&self, smmu_guard: &mut Pin<&mut SmmuLockGuard<'_>>) {
        pin_init::stack_pin_init!(
            let local_smrgs = DoublyLinkedList::<
                UniquePtr<StreamMatchRegGroup>,
                DefaultObjectTag,
                TrackingSize,
            >::new()
        );
        let local_cb: Option<UniquePtr<ContextBank>>;

        {
            ksync::lock!(let mut pmt_guard = self.lock_pmt_lock());
            {
                ksync::lock!(let mut guard = self.lock_lock());
                debug_assert_ne!(*guard.mode(), BtiMode::Shutdown);

                // Lock down all of the hardware, and place ourselves in the shutdown
                // state first. This should never fail as we should never be shutting down
                // any adopted configuration.
                let mode_res = self.set_mode_locked(&mut guard, BtiMode::Shutdown);
                assert!(mode_res.is_ok());

                // Mark our resources in the SMMU bookkeeping as being available for
                // allocation once again. We are finished messing with the registers; they
                // belong to our SMMU instance once again.
                for smrg in guard.smrg_list().iter() {
                    debug_assert!(!smmu_guard.available_smrgs().test_bit(smrg.smrg_ndx()));
                    smmu_guard.as_mut().available_smrgs_mut().set_bit(smrg.smrg_ndx());
                }

                if let Some(ref cb) = *guard.context_bank() {
                    // At this point in time, we should be certain that we no longer have any
                    // registered context bank IRQs, and that there are not any context bank
                    // interrupts in flight targeting this BTI. This was taken care of for us by
                    // our SMMU instance during `Smmu::shutdown_bti`.
                    debug_assert!(!smmu_guard.available_cbs().test_bit(cb.cb_ndx()));
                    smmu_guard.as_mut().available_cbs_mut().set_bit(cb.cb_ndx());
                }

                // If we are shutting down, we should not have any active or quarantined PMTs left.
                debug_assert!(pmt_guard.active_pmt_list().is_empty());
                debug_assert_eq!(*pmt_guard.quarantined_pmt_count(), 0);
                debug_assert_eq!(*pmt_guard.quarantined_page_count(), 0);

                // Move our resources outside of our spinlock's scope so that they can
                // return to the heap after the lock has been dropped.
                while let Some(smrg) = guard.as_mut().smrg_list_mut().pop_front() {
                    local_smrgs.as_mut().push_back(smrg);
                }
                local_cb = guard.as_mut().context_bank_mut().take();
            }

            // If we have a device address space, clear mappings it may still have.
            //
            // Note: We do not need to actually perform any TLB invalidating during this
            // process. When we shut down all of our hardware (above) we cleared and
            // disabled the TTBRs in our context bank, and then invalidated all of our
            // TLB entries using our CB's ASID. The TLB cache cannot have any valid
            // entries in it referring to any part of our PTEs, and there is no way for
            // hardware to establish any new TLB entries since both of our TTBRs are
            // disabled.
            //
            // TODO(johngro): Consider (as an optimization pass) changing the way that
            // `free_translation_tables` works. Right now it is doing stuff like flushing
            // dirty PTEs to physical memory, and requiring a TLB invalidate callback.
            // Provided we make the invariant for calling `free_translation_tables` "You
            // must have disabled your HW and invalidated your TLBs before calling
            // this", we don't need to do this stuff anymore. We can optimize the
            // method, remove the need to pass a callback, and even auto-invoke it from
            // `Drop` instead of needing to manually call it. Then, all we need to do
            // here is drop our `Box<DeviceAspace>`.
            if let Some(mut aspace) = pmt_guard.as_mut().aspace_mut().take() {
                debug_assert!(local_cb.is_some());
                aspace.free_translation_tables(&mut NoOpTlbInvalidator);
            }
        }

        // Now go ahead and destroy the resource bookkeeping.
        local_smrgs.as_mut().clear();
        drop(local_cb);
    }

    /// Returns `true` if any SMRG owned by this BTI intersects `stream_ids`.
    pub(crate) fn smr_intersects(&self, stream_ids: SmrValue) -> bool {
        ksync::lock!(let guard = self.lock_lock());
        for smrg in guard.smrg_list().iter() {
            if smrg.stream_ids().intersects(stream_ids) {
                return true;
            }
        }
        false
    }

    /// Adds a `StreamMatchRegGroup` to this BTI under `smmu_guard`.
    pub(crate) fn add_smrg(
        &self,
        smmu_guard: &mut Pin<&mut SmmuLockGuard<'_>>,
        smrg: UniquePtr<StreamMatchRegGroup>,
    ) {
        ksync::lock!(let mut guard = self.lock_lock());
        self.add_smrg_locked(smmu_guard, &mut guard, smrg);
    }

    /// Adds a `StreamMatchRegGroup` to this BTI under both `smmu_guard` and `guard`.
    pub(crate) fn add_smrg_locked(
        &self,
        smmu_guard: &mut Pin<&mut SmmuLockGuard<'_>>,
        guard: &mut Pin<&mut SmmuBtiLockGuard<'_>>,
        smrg: UniquePtr<StreamMatchRegGroup>,
    ) {
        // Currently, there are only three places that we ever add an SMRG to a BTI:
        //
        // 1) As we create the BTI for a user's new `BtiDispatcher`.
        // 2) As we adopt the SMRG from initial hardware state and create a new BTI to
        //    contain it.
        // 3) During adoption when we find another SMRG to adopt which is in
        //    translation mode and whose `cb_ndx` matches this BTI's currently configured
        //    context bank.
        //
        // In case #1, the SMRG we just created should have come from the pool of free
        // SMRGs, and should be in the `Invalid` state with an invalid context bank
        // index.
        //
        // For case #2, the SMRG may or may not have an associated context bank, but
        // if it does, the SMRG should be in translation mode, and the BTI we created
        // for the adopted SMRG should already have adopted the `ContextBank` as
        // specified by the SMRG's register state.
        //
        // Finally, for case #3, the only reason the adoption code should be adding
        // another SMRG to this BTI would be because it discovered one in `Translation`
        // mode whose `cb_ndx` matches the index of this BTI's context bank.
        //
        // So, if this SMRG is in translation mode, assert that we have a context
        // bank, and the index of the context bank matches that of the adopted SMRG.
        //
        // TODO(johngro): When we get around to allowing user-mode to add a new SMRG to
        // an existing BTI, we need to make sure that the SMRG (which will be in a
        // locked-down state) is reconfigured to match the BTI's current state. IOW -
        // If the BTI is operating, the SMRG will need to be configured for
        // translation, pointed at this BTI's CB, and enabled.
        if smrg.mode() == S2crType::Translation {
            debug_assert!(guard.context_bank().is_some());
            debug_assert_eq!(guard.context_bank().as_ref().unwrap().cb_ndx(), smrg.cb_ndx());
        }

        // The SMRG we are adding should not be marked as in-use at the SMMU level (yet).
        debug_assert!(smmu_guard.available_smrgs().test_bit(smrg.smrg_ndx()));

        // Everything checks out. Add the SMRG to our list, and mark it as in use in
        // our parent SMMU.
        smmu_guard.as_mut().available_smrgs_mut().clear_bit(smrg.smrg_ndx());
        guard.as_mut().smrg_list_mut().push_back(smrg);
    }

    /// Sets the operating mode of this BTI and its owned hardware blocks.
    pub(crate) fn set_mode_locked(
        &self,
        guard: &mut Pin<&mut SmmuBtiLockGuard<'_>>,
        target_mode: BtiMode,
    ) -> Result<(), Status> {
        // Are we already there?
        if target_mode == *guard.mode() {
            return Ok(());
        }

        // Adopted and Shutdown BTIs can never change mode.
        if *guard.mode() == BtiMode::Adopted || *guard.mode() == BtiMode::Shutdown {
            return Err(Status::BAD_STATE);
        }

        let op_mode = guard.smmu().as_ref().expect("active BTI must have SMMU").op_mode();

        // Record our new mode.
        *guard.as_mut().mode_mut() = target_mode;

        // If we are changing to Fault mode while our SMMU is operating in Enforced
        // mode, we are basically finished. Access control is handled at the
        // per-address level in our address space, we don't need to do anything
        // special to protect ourselves against rogue hardware.
        if target_mode == BtiMode::Fault && op_mode == ArmSmmuMode::Enforced {
            return Ok(());
        }

        let mut smmu_name_buf = [0u8; 32];
        let smmu_name = {
            let s = guard.smmu().as_ref().expect("active BTI must have SMMU").name().as_bytes();
            let len = s.len().min(smmu_name_buf.len());
            smmu_name_buf[..len].copy_from_slice(&s[..len]);
            core::str::from_utf8(&smmu_name_buf[..len]).unwrap_or("")
        };

        // We were not adopted, so we should be able to assert that we have a
        // context bank. Do so, then configure our context bank for its new mode.
        let cb_ndx = {
            let cb = guard
                .as_mut()
                .context_bank_mut()
                .as_mut()
                .expect("non-adopted BTI must have a context bank");
            cb.set_mode(smmu_name, target_mode);
            cb.cb_ndx()
        };

        for smrg in guard.as_mut().smrg_list_mut().iter_mut() {
            // Except for when we are shutting down (and we disable access at all
            // levels), we control all of our enforcement policy through only the
            // context bank. So, if we are shutting down, disable all of the match
            // registers, otherwise enable them and point them at our context bank.
            if target_mode == BtiMode::Shutdown {
                smrg.disable_pinned();
            } else {
                smrg.enable_for_context_bank_pinned(cb_ndx);
            }
        }

        Ok(())
    }

    /// Handles a context bank fault interrupt under `guard`.
    pub(crate) fn handle_fault_locked(&self, guard: &mut Pin<&mut SmmuBtiLockGuard<'_>>) {
        // TODO(johngro): Currently, the only thing for us to do here is to enter
        // "Fault" mode. Some day, we'd like to define a signal that we can raise on
        // our BTI object so that user-mode is aware that this has happened. See the
        // section "BTI Dispatchers should have a 'faulted' signal" in the top-level
        // README.md for more details.
        //
        // There should be nothing for us to do if we are already shut down.
        if *guard.mode() != BtiMode::Shutdown {
            if guard.smmu().is_none() {
                return;
            };
            // The only way this should ever be the case is if we have adopted hardware
            // whose SMRG registers didn't specify a valid context bank, but secretly
            // actually had one which was configured using virtualized register
            // techniques by the system at either EL2/sEL3. In theory, we can promote
            // this to an ASSERT, but for now it is just a warning.
            if guard.context_bank().is_none() {
                let smmu_name = guard.smmu().as_ref().expect("checked above").name();
                dprintf!(
                    INFO,
                    "{}: WARNING - Context fault received, but BTI has no associated context bank.\n",
                    smmu_name
                );
            }

            let res = self.set_mode_locked(guard, BtiMode::Fault);
            let smmu_name = guard.smmu().as_ref().expect("checked above").name();
            let mut sid_buf = [0u8; 128];
            let sid_str = Self::render_sid_list_locked(guard, &mut sid_buf);
            if let Err(err) = res {
                dprintf!(
                    INFO,
                    "{}: WARNING - Failed to place BTI controlling StreamID(s) [{}] into fault mode (err {}).\n",
                    smmu_name,
                    sid_str,
                    err.into_raw()
                );
            } else {
                dprintf!(
                    INFO,
                    "{}: BTI controlling StreamID(s) [{}] has entered fault mode.\n",
                    smmu_name,
                    sid_str
                );
            }
        }
    }

    /// Returns the context bank index under `guard` (or `u32::MAX` if none).
    #[inline]
    pub(crate) fn cb_ndx_locked(guard: &SmmuBtiLockGuard<'_>) -> u32 {
        guard.context_bank().as_ref().map_or(u32::MAX, |cb| cb.cb_ndx())
    }

    /// Returns the context bank index (or `u32::MAX` if none).
    #[inline]
    pub(crate) fn cb_ndx(&self) -> u32 {
        ksync::lock!(let guard = self.lock_lock());
        Self::cb_ndx_locked(&guard)
    }

    /// Invalidates all SMRGs on this BTI (used by debug console `smmu isids`).
    pub(crate) fn invalidate_sids(&self) -> Result<(), Status> {
        ksync::lock!(let guard = self.lock_lock());
        // Adopted and Shutdown BTIs can never invalidate their Stream IDs.
        if *guard.mode() == BtiMode::Adopted || *guard.mode() == BtiMode::Shutdown {
            return Err(Status::BAD_STATE);
        }
        for smrg in guard.smrg_list().iter() {
            smrg.invalidate();
        }
        Ok(())
    }

    /// Renders the space-separated list of 16-bit Stream IDs matched by this BTI into `buffer`.
    pub(crate) fn render_sid_list_locked<'a>(
        guard: &SmmuBtiLockGuard<'_>,
        buffer: &'a mut [u8],
    ) -> &'a str {
        // Stop accumulating characters and automatically truncate the string if we run out of room
        // in `buffer`.
        //
        // Temporary porting note: the C++ `RenderSidList` printed into a `StringFile`, which cut
        // the text off mid-entry once the buffer was full, and it kept iterating over every
        // remaining Stream ID (up to 32768 of them for a fully wildcarded mask, with the BTI
        // spinlock held, at interrupt time when called from the fault handler). Here the list
        // ends with the last whole entry which fits, and the loops stop there.
        let mut pos = 0usize;
        let mut first = true;
        const HEX: &[u8; 16] = b"0123456789abcdef";

        'outer: for smrg in guard.smrg_list().iter() {
            for id in smrg.stream_ids() {
                let needed = if first { 6 } else { 7 };
                if pos + needed > buffer.len() {
                    break 'outer;
                }
                if !first {
                    buffer[pos] = b' ';
                    pos += 1;
                }
                first = false;
                buffer[pos] = b'0';
                buffer[pos + 1] = b'x';
                buffer[pos + 2] = HEX[((id >> 12) & 0xf) as usize];
                buffer[pos + 3] = HEX[((id >> 8) & 0xf) as usize];
                buffer[pos + 4] = HEX[((id >> 4) & 0xf) as usize];
                buffer[pos + 5] = HEX[(id & 0xf) as usize];
                pos += 6;
            }
        }
        core::str::from_utf8(&buffer[..pos]).unwrap_or("")
    }

    /// Handles explicit `SmmuPmt::unpin` (`ReleasePinnedMemory`) of a child PMT.
    ///
    /// # Safety
    ///
    /// `pmt` must be owned by `self`.
    pub(crate) unsafe fn on_pmt_unpin(&self, pmt: &SmmuPmt) {
        let mut removed_pmt: Option<RefPtr<SmmuPmt>> = None;

        // If there is still a PMO to unpin, move it outside of the scope of the lock
        // before doing so.
        let mut released_vmo = {
            // Release the mapping for this PMT, if any. If the PMT has no mapping in
            // the device's address space (as would be the case if this BTI is operating
            // in passthru mode), then this will be a no-op.
            ksync::lock!(let mut pmt_guard = self.lock_pmt_lock());
            let mapping =
                // SAFETY: The caller promises that `self` owns `pmt`, and `pmt_guard` holds
                // `self.pmt_lock`.
                unsafe { pmt.guard_pmt_lock_mut(pmt_guard.as_mut().token_mut()) }.map_location_mut().take();
            self.release_mapping_locked(&mut pmt_guard, mapping);

            ksync::lock!(let _guard = self.lock_lock());
            // SAFETY: The caller promises that `self` owns `pmt`, and `pmt_guard` holds
            // `self.pmt_lock`.
            let state = *unsafe { pmt.guard_pmt_lock(pmt_guard.token()) }.state();
            if state == SmmuPmtState::Active {
                debug_assert!(pmt.in_container());
                // SAFETY: `pmt` was verified to be in `active_pmt_list` above.
                removed_pmt = unsafe { pmt_guard.as_mut().active_pmt_list_mut().erase(pmt) };
                debug_assert!(removed_pmt.is_some());
                // SAFETY: The caller promises that `self` owns `pmt`, and `pmt_guard` holds
                // `self.pmt_lock`.
                *unsafe { pmt.guard_pmt_lock_mut(pmt_guard.as_mut().token_mut()) }.state_mut() =
                    SmmuPmtState::Released;
            } else {
                debug_assert!(!pmt.in_container());
                // SAFETY: The caller promises that `self` owns `pmt`, and `pmt_guard` holds
                // `self.pmt_lock`.
                debug_assert!(
                    !unsafe { pmt.guard_pmt_lock(pmt_guard.token()) }.pinned_vmo().has_vmo()
                );
            }

            // SAFETY: The caller promises that `self` owns `pmt`, and `pmt_guard` holds
            // `self.pmt_lock`.
            unsafe { pmt.guard_pmt_lock_mut(pmt_guard.as_mut().token_mut()) }
                .pinned_vmo_mut()
                .take()
        };

        // Drop the list's `RefPtr<SmmuPmt>` outside `pmt_lock` and `lock` so that if it was the
        // last reference to `pmt`, we do not interact with the heap while a spinlock is held.
        drop(removed_pmt);

        // Now that the lock is dropped, if we took ownership of the PMO, we can let
        // it go out of scope now, releasing the pages back to the PMM as we do.
        released_vmo.reset();
    }

    /// Handles `SmmuPmt::on_zero_handles` (`OnDispatcherZeroHandles`) when a PMT's dispatcher
    /// reaches zero user handles.
    ///
    /// # Safety
    ///
    /// `pmt` must be owned by `self`.
    pub(crate) unsafe fn on_pmt_zero_handles(&self, pmt: &SmmuPmt) {
        let mut quarantined_page_count: u64 = 0;
        let mut quarantined_pmt_count: u64 = 0;
        let has_active_pmts: bool;
        let was_orphaned: bool;
        // `None` once this BTI has reached end of life (see below).
        let op_mode: Option<ArmSmmuMode>;
        let mut removed_pmt: Option<RefPtr<SmmuPmt>> = None;

        // If there is still a PMO to unpin, move it outside of the scope of the lock
        // before doing so.
        let mut quarantined_vmo = {
            // Release the mapping for this PMT, if any.
            ksync::lock!(let mut pmt_guard = self.lock_pmt_lock());
            let mapping =
                // SAFETY: The caller promises that `self` owns `pmt`, and `pmt_guard` holds
                // `self.pmt_lock`.
                unsafe { pmt.guard_pmt_lock_mut(pmt_guard.as_mut().token_mut()) }.map_location_mut().take();
            self.release_mapping_locked(&mut pmt_guard, mapping);

            ksync::lock!(let mut guard = self.lock_lock());
            // Our SMMU reference is cleared by `on_end_of_life`. A PMT which was explicitly
            // unpinned (`Released`) may legitimately have its final handle closed after that
            // point: `zx_pmt_unpin` unpins first and only then drops the handle, and the last BTI
            // handle can be closed in between. So do not demand an SMMU here; the operating mode
            // is only consulted below when deciding whether an orphaned BTI can shut down.
            op_mode = guard.smmu().as_ref().map(|smmu| smmu.op_mode());

            // SAFETY: The caller promises that `self` owns `pmt`, and `pmt_guard` holds
            // `self.pmt_lock`.
            let state = *unsafe { pmt.guard_pmt_lock(pmt_guard.token()) }.state();
            debug_assert_ne!(state, SmmuPmtState::Initial);
            if state == SmmuPmtState::Active {
                // If a PMT has its last handle closed before being formally unpinned,
                // then we have leaked the PMT and will need to quarantine it.
                //
                // Start by entering fault mode.
                let set_fault_res = self.set_mode_locked(&mut guard, BtiMode::Fault);
                assert!(set_fault_res.is_ok());

                // Remove the PMT from the active set of PMTs and flag it as quarantined.
                debug_assert!(pmt.in_container());
                // SAFETY: `pmt` was verified to be in `active_pmt_list` above.
                removed_pmt = unsafe { pmt_guard.as_mut().active_pmt_list_mut().erase(pmt) };
                debug_assert!(removed_pmt.is_some());
                // SAFETY: The caller promises that `self` owns `pmt`, and `pmt_guard` holds
                // `self.pmt_lock`.
                *unsafe { pmt.guard_pmt_lock_mut(pmt_guard.as_mut().token_mut()) }.state_mut() =
                    SmmuPmtState::Quarantined;

                // Update our stats and take note of what they are so we can log an
                // appropriate warning.
                let pmt_pages = pmt.pages(pmt_guard.token());
                *pmt_guard.as_mut().quarantined_page_count_mut() += pmt_pages;
                *pmt_guard.as_mut().quarantined_pmt_count_mut() += 1;
                quarantined_page_count = *pmt_guard.quarantined_page_count();
                quarantined_pmt_count = *pmt_guard.quarantined_pmt_count();
            } else {
                // If this PMT was not active, then it should be in the Released state and
                // not on the active list.
                debug_assert_eq!(state, SmmuPmtState::Released);
                debug_assert!(!pmt.in_container());
                // SAFETY: The caller promises that `self` owns `pmt`, and `pmt_guard` holds
                // `self.pmt_lock`.
                debug_assert!(
                    !unsafe { pmt.guard_pmt_lock(pmt_guard.token()) }.pinned_vmo().has_vmo()
                );
            }

            has_active_pmts = !pmt_guard.active_pmt_list().is_empty();
            was_orphaned = *pmt_guard.orphaned();
            // SAFETY: The caller promises that `self` owns `pmt`, and `pmt_guard` holds
            // `self.pmt_lock`.
            unsafe { pmt.guard_pmt_lock_mut(pmt_guard.as_mut().token_mut()) }
                .pinned_vmo_mut()
                .take()
        };

        drop(removed_pmt);

        // Now that we are outside of the locks, we can handle any final cleanup
        // tasks:
        //
        // 1) If this PMT was leaked, we need to log a warning.
        // 2) If we were orphaned, and this was the last active PMT, and we either
        //    have no quarantined PMTs, or are operating in Enforced mode, then we
        //    have reached end of life and can clean up now.
        // 3) If we quarantined a pinned memory object, we can now return its pages to
        //    the PMM. We don't need to actually keep it around, since we have either
        //    revoked access to that specific region of memory (if we are in enforced
        //    mode), or revoked the device's access to all memory (because we are in
        //    passthru mode).

        // #1 : Log a warning if needed.
        if quarantined_vmo.has_vmo() {
            let reason = if was_orphaned {
                BtiPageLeakReason::PmtQuarantinedWhenBtiOrphaned
            } else {
                BtiPageLeakReason::PmtQuarantined
            };
            self.print_quarantine_warning(
                reason,
                quarantined_page_count,
                quarantined_pmt_count as usize,
            );
        }

        // #2 : Shutdown if it is time.
        debug_assert_eq!(quarantined_page_count == 0, quarantined_pmt_count == 0);
        if was_orphaned
            && !has_active_pmts
            && (quarantined_pmt_count == 0 || op_mode == Some(ArmSmmuMode::Enforced))
        {
            self.on_end_of_life();
        }

        // #3 : Release our VMO (if any).
        quarantined_vmo.reset();
    }
}

impl SmmuBti {
    /// Prints diagnostic details of this BTI for `smmu show`.
    pub(crate) fn cmd_show(&self, ilvl: usize, show_pmts: bool, verbose: bool) {
        ksync::lock!(let pmt_guard = self.lock_pmt_lock());
        ksync::lock!(let guard = self.lock_lock());

        let indent0 = ilvl * 2;
        let indent1 = (ilvl + 1) * 2;
        let indent2 = (ilvl + 2) * 2;

        dprintf!(
            INFO,
            "{:indent0$}Mode              : {}{}\n",
            "",
            bti_mode_to_string(*guard.mode()),
            if *pmt_guard.orphaned() { " (orphaned)" } else { "" },
            indent0 = indent0
        );
        let bti_name = Self::name_display(guard.name());
        dprintf!(INFO, "{:indent0$}Name              : {}\n", "", bti_name, indent0 = indent0);

        let mut sid_buf = [0u8; 128];
        let sid_str = Self::render_sid_list_locked(&guard, &mut sid_buf);
        dprintf!(INFO, "{:indent0$}StreamIDs         : {}\n", "", sid_str, indent0 = indent0);
        dprintf!(
            INFO,
            "{:indent0$}Active PMTs       : {}\n",
            "",
            pmt_guard.active_pmt_list().len(),
            indent0 = indent0
        );
        dprintf!(
            INFO,
            "{:indent0$}Quarantined PMTs  : {}\n",
            "",
            *pmt_guard.quarantined_pmt_count(),
            indent0 = indent0
        );
        dprintf!(
            INFO,
            "{:indent0$}Quarantined Pages : {}\n",
            "",
            *pmt_guard.quarantined_page_count(),
            indent0 = indent0
        );
        if let Some(ref aspace) = *pmt_guard.aspace() {
            dprintf!(
                INFO,
                "{:indent0$}TLB Pages         : {}\n",
                "",
                aspace.page_cache().in_flight_pages(),
                indent0 = indent0
            );
            dprintf!(
                INFO,
                "{:indent0$}Cached TLB Pages  : {}\n",
                "",
                aspace.page_cache().cache_entries(),
                indent0 = indent0
            );
        }

        if show_pmts && *guard.mode() == BtiMode::Translation {
            let mut page_total = 0u64;
            let token = pmt_guard.token();

            for (ndx, pmt) in pmt_guard.active_pmt_list().iter().enumerate() {
                // SAFETY: `pmt` is on this BTI's active list, so `self` owns it, and `pmt_guard`
                // holds `self.pmt_lock`.
                if let Some(ref loc) = *unsafe { pmt.guard_pmt_lock(token) }.map_location() {
                    let base = loc.base();
                    let size = loc.size();
                    let page_size = size >> PAGE_SHIFT;
                    dprintf!(
                        INFO,
                        "{:indent1$}PMT[{:3}] : [{:#x}, {:#x}] ({} Page{})\n",
                        "",
                        ndx,
                        base,
                        base + size - 1,
                        page_size,
                        if page_size == 1 { "" } else { "s" },
                        indent1 = indent1
                    );
                    page_total += page_size;
                } else {
                    // If we have an active PMT in a BTI operating in translation mode, we
                    // may not have our `map_location` any more because someone used the debug
                    // console to force-drop our mapping with the `lock` command.
                    dprintf!(INFO, "{:indent1$}PMT[{:3}] : ???\n", "", ndx, indent1 = indent1);
                }
            }
            dprintf!(
                INFO,
                "{:indent1$}Counted {} mapped page{}.\n",
                "",
                page_total,
                if page_total == 1 { "" } else { "s" },
                indent1 = indent1
            );
        }

        let smrg_count = guard.smrg_list().len();
        let mut smrg_num = 0u32;
        for smrg in guard.smrg_list().iter() {
            smrg_num += 1;
            dprintf!(
                INFO,
                "\n{:indent0$}SMRG {}/{} (index {})\n",
                "",
                smrg_num,
                smrg_count,
                smrg.smrg_ndx(),
                indent0 = indent0
            );
            dprintf!(
                INFO,
                "{:indent1$}Mode  : {}\n",
                "",
                arm_s2cr_type_to_string(smrg.mode()),
                indent1 = indent1
            );
            dprintf!(
                INFO,
                "{:indent1$}Mask  : {:#06x}\n",
                "",
                smrg.stream_ids().mask(),
                indent1 = indent1
            );
            dprintf!(
                INFO,
                "{:indent1$}ID    : {:#06x}\n",
                "",
                smrg.stream_ids().id(),
                indent1 = indent1
            );
            dprintf!(INFO, "{:indent1$}CBNdx : {:#06x}\n", "", smrg.cb_ndx(), indent1 = indent1);

            if verbose {
                let gr0_base = smrg.gr0_base();
                let ndx = smrg.smrg_ndx();
                let smr_reg = Smr::read_at(&gr0_base, ndx);
                dump_reg(
                    "SMR",
                    smr_reg.reg_value() as u64,
                    4,
                    Smr::offset(ndx),
                    Smr::FIELDS,
                    &gr0_base,
                    ilvl + 2,
                );
                let s2cr_reg = S2cr::read_at(&gr0_base, ndx);
                // Format `"S2CR (<mode>)"` using `arm_s2cr_type_to_string(smrg.mode())`
                // (`"Translation"`, `"Bypass"`, `"Fault"`, `"Invalid"`).
                let (s2cr_tag, fields) = match smrg.mode() {
                    S2crType::Translation => ("S2CR (Translation)", S2crTranslation::FIELDS),
                    S2crType::Bypass => ("S2CR (Bypass)", S2crBypass::FIELDS),
                    S2crType::Fault => ("S2CR (Fault)", S2crFault::FIELDS),
                    S2crType::Invalid => ("S2CR (Invalid)", S2cr::FIELDS),
                };
                dump_reg(
                    s2cr_tag,
                    s2cr_reg.reg_value() as u64,
                    4,
                    S2cr::offset(ndx),
                    fields,
                    &gr0_base,
                    ilvl + 2,
                );
            }
        }

        if let Some(ref cb) = *guard.context_bank() {
            let gr1_base = cb.gr1_base();
            let cb_base = cb.cb_base();
            let cb_ndx = cb.cb_ndx();
            let cbar = Cbar::read_at(&gr1_base, cb_ndx);
            let sctlr = Sctlr::read_from(&cb_base);
            let irq_num =
                guard.smmu().as_ref().and_then(|s| s.get_context_irq(cb_ndx)).map_or(0, |d| d.num);

            dprintf!(INFO, "\n{:indent0$}Context Bank (index {})\n", "", cb_ndx, indent0 = indent0);
            dprintf!(
                INFO,
                "{:indent1$}Mode      : {}\n",
                "",
                bti_mode_to_string(cb.mode()),
                indent1 = indent1
            );
            dprintf!(INFO, "{:indent1$}IRQ       : {}\n", "", irq_num, indent1 = indent1);
            dprintf!(
                INFO,
                "{:indent1$}AddrMode  : {}\n",
                "",
                addr_mode_to_string(cb.addr_mode()),
                indent1 = indent1
            );
            dprintf!(
                INFO,
                "{:indent1$}CBAR Type : {}\n",
                "",
                arm_cbar_type_to_string(cbar.cbar_type()),
                indent1 = indent1
            );
            dprintf!(
                INFO,
                "{:indent1$}MMU State : {}\n",
                "",
                if sctlr.m() != 0 { "Enabled" } else { "Disabled" },
                indent1 = indent1
            );

            for (i, ttbr) in cb.ttbrs().iter().enumerate() {
                dprintf!(
                    INFO,
                    "{:indent1$}TTBR[{}]   : {}\n",
                    "",
                    i,
                    if ttbr.enabled { "Enabled" } else { "Disabled" },
                    indent1 = indent1
                );
                if ttbr.enabled {
                    dprintf!(
                        INFO,
                        "{:indent2$}Granule Size : {}\n",
                        "",
                        ttbr.granule_size_bits,
                        indent2 = indent2
                    );
                    dprintf!(
                        INFO,
                        "{:indent2$}First Valid  : {:#018x}\n",
                        "",
                        ttbr.first_valid_addr,
                        indent2 = indent2
                    );
                    dprintf!(
                        INFO,
                        "{:indent2$}Last Valid   : {:#018x}\n",
                        "",
                        ttbr.last_valid_addr,
                        indent2 = indent2
                    );
                    dprintf!(
                        INFO,
                        "{:indent2$}Phys Addr    : {:#018x}\n",
                        "",
                        ttbr.ttbr_paddr,
                        indent2 = indent2
                    );
                }

                // TODO(johngro): If we are in translation mode, walk the translation table
                // and print the active mappings.
            }

            if verbose {
                let (cbar_tag, cbar_fields) = match cbar.cbar_type() {
                    CbarType::S2Translation => ("CBAR (S2Translate)", CbarS2Translation::FIELDS),
                    CbarType::S1TS2Bypass => ("CBAR (S1TS2Bypass)", CbarS1TS2Bypass::FIELDS),
                    CbarType::S1TS2Fault => ("CBAR (S1TS2Fault)", CbarS1TS2Fault::FIELDS),
                    CbarType::S1TS2Translate => {
                        ("CBAR (S1TS2Translate)", CbarS1TS2Translate::FIELDS)
                    }
                };
                dump_reg(
                    cbar_tag,
                    cbar.reg_value() as u64,
                    4,
                    Cbar::offset(cb_ndx),
                    cbar_fields,
                    &gr1_base,
                    ilvl + 2,
                );
                let cba2r = Cba2r::read_at(&gr1_base, cb_ndx);
                dump_reg(
                    "CBA2R",
                    cba2r.reg_value() as u64,
                    4,
                    Cba2r::offset(cb_ndx),
                    Cba2r::FIELDS,
                    &gr1_base,
                    ilvl + 2,
                );
                dump_reg(
                    "SCTLR",
                    sctlr.reg_value() as u64,
                    4,
                    Sctlr::OFFSET,
                    Sctlr::FIELDS,
                    &cb_base,
                    ilvl + 2,
                );
                match cb.addr_mode() {
                    AddrMode::Mode32Bit => {
                        dump_reg(
                            "TTBR0 (32)",
                            Ttbr0_32Bit::read_from(&cb_base).reg_value() as u64,
                            4,
                            Ttbr0_32Bit::OFFSET,
                            Ttbr0_32Bit::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                        dump_reg(
                            "TTBR1 (32)",
                            Ttbr1_32Bit::read_from(&cb_base).reg_value() as u64,
                            4,
                            Ttbr1_32Bit::OFFSET,
                            Ttbr1_32Bit::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                        dump_reg(
                            "TCR (32)",
                            Tcr32Bit::read_from(&cb_base).reg_value() as u64,
                            4,
                            Tcr32Bit::OFFSET,
                            Tcr32Bit::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                    }
                    AddrMode::Ext32Bit => {
                        dump_reg(
                            "TTBR0 (E32)",
                            Ttbr0Ext32Bit::read_from(&cb_base).reg_value(),
                            8,
                            Ttbr0Ext32Bit::OFFSET,
                            Ttbr0Ext32Bit::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                        dump_reg(
                            "TTBR1 (E32)",
                            Ttbr1Ext32Bit::read_from(&cb_base).reg_value(),
                            8,
                            Ttbr1Ext32Bit::OFFSET,
                            Ttbr1Ext32Bit::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                        dump_reg(
                            "TCR (E32)",
                            TcrExt32Bit::read_from(&cb_base).reg_value() as u64,
                            4,
                            TcrExt32Bit::OFFSET,
                            TcrExt32Bit::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                    }
                    AddrMode::Mode64Bit => {
                        dump_reg(
                            "TTBR0 (64)",
                            Ttbr0_64Bit::read_from(&cb_base).reg_value(),
                            8,
                            Ttbr0_64Bit::OFFSET,
                            Ttbr0_64Bit::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                        dump_reg(
                            "TTBR1 (64)",
                            Ttbr1_64Bit::read_from(&cb_base).reg_value(),
                            8,
                            Ttbr1_64Bit::OFFSET,
                            Ttbr1_64Bit::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                        dump_reg(
                            "TCR (64)",
                            Tcr64Bit::read_from(&cb_base).reg_value() as u64,
                            4,
                            Tcr64Bit::OFFSET,
                            Tcr64Bit::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                    }
                    AddrMode::Invalid => {
                        dump_reg(
                            "TCR",
                            Tcr::read_from(&cb_base).reg_value() as u64,
                            4,
                            Tcr::OFFSET,
                            Tcr::FIELDS,
                            &cb_base,
                            ilvl + 2,
                        );
                    }
                }
                dump_reg(
                    "TCR2",
                    Tcr2::read_from(&cb_base).reg_value() as u64,
                    4,
                    Tcr2::OFFSET,
                    Tcr2::FIELDS,
                    &cb_base,
                    ilvl + 2,
                );
            }
        }
    }

    /// Locks down this BTI for the debug console `smmu lock` command.
    pub(crate) fn cmd_lock(&self, ndx: u32) {
        let (bti_name_buf, smmu) = {
            ksync::lock!(let guard = self.lock_lock());
            (*guard.name(), guard.smmu().as_ref().expect("BTI must have SMMU").clone())
        };
        let bti_name = Self::name_display(&bti_name_buf);

        // We are going to do one of two things, depending on whether we are in
        // Enforced mode, or in Passthru mode. If we are in enforced mode, we are
        // going to simply go through our list of active PMTs, and remove all of
        // their mappings from the page tables. This does not actually "lock" the
        // BTI, it just makes existing pinned memory inaccessible (assuming that
        // there is any).
        //
        // When we are in passthru mode, we just force the BTI (and its associated
        // context bank) into Fault mode, meaning that the MMU will be enabled, but
        // the TTBRs will be disabled, causing every translation request to fail
        // instead of passing through.
        if smmu.op_mode() == ArmSmmuMode::Enforced {
            ksync::lock!(let mut pmt_guard = self.lock_pmt_lock());
            // Go through this BTI's PMTs and release any mappings back to the BTI's
            // address space. Don't actually release the pinned memory yet; we are
            // only trying to force some faults.
            let mut released = 0usize;
            let mut maybe_pmt_ptr: Option<*const SmmuPmt> =
                pmt_guard.active_pmt_list().front().map(|p| p as *const SmmuPmt);
            while let Some(pmt_ptr) = maybe_pmt_ptr {
                // In general, we should be able to assert that all active PMTs have a
                // mapping during normal operation. However, it is possible that someone
                // has run this command twice and the mappings were already released. To
                // keep our reporting consistent, make sure to only count mappings we
                // actually released.
                let (maybe_mapping, next_ptr) = {
                    // SAFETY: `pmt_ptr` points to an active PMT in `active_pmt_list`, and
                    // `pmt_lock` is held across the dereference (and `release_mapping_locked`
                    // does not unlink nodes from `active_pmt_list`).
                    let pmt = unsafe { &*pmt_ptr };
                    let next_ptr = {
                        // SAFETY: `active_pmt_list` is not moved, and `pmt` is an element of
                        // `active_pmt_list`.
                        let mut cursor =
                            unsafe { pmt_guard.as_mut().active_pmt_list_mut().cursor_at(pmt) };
                        cursor.move_next();
                        cursor.get().map(|next| next as *const SmmuPmt)
                    };
                    let token = pmt_guard.as_mut().token_mut();
                    // SAFETY: `pmt` is on this BTI's active list, so `self` owns it, and
                    // `pmt_guard` holds `self.pmt_lock`.
                    let maybe_mapping =
                        unsafe { pmt.guard_pmt_lock_mut(token) }.map_location_mut().take();
                    (maybe_mapping, next_ptr)
                };
                if let Some(mapping) = maybe_mapping {
                    self.release_mapping_locked(&mut pmt_guard, Some(mapping));
                    released += 1;
                }
                maybe_pmt_ptr = next_ptr;
            }
            dprintf!(
                INFO,
                "{}: BTI index #{} (\"{}\") released {} active mapping{}.\n",
                smmu.name(),
                ndx,
                bti_name,
                released,
                if released == 1 { "" } else { "s" }
            );
        } else {
            ksync::lock!(let mut guard = self.lock_lock());
            // If the SMMU is not operating in Enforced mode, it must be in passthru
            // mode (if it was in disabled mode, there would be no constructed `SmmuBti`
            // objects).
            debug_assert_eq!(smmu.op_mode(), ArmSmmuMode::Passthru);

            // Force the BTI into fault mode. This will lock things down if we
            // are in passthru mode, and prevent new PMTs from being created moving
            // forward.
            let res = self.set_mode_locked(&mut guard, BtiMode::Fault);
            match res {
                Ok(()) => {
                    // We are forcing our BTI into fault mode from the debug console as opposed
                    // to in reaction to an interrupt. Re-enable the context fault interrupt,
                    // so that the console user can see a fault exception during the next access
                    // in response to the lockdown command issued from the console.
                    if let Some(ref mut cb) = *guard.as_mut().context_bank_mut() {
                        cb.enable_fault_interrupt();
                    }
                    dprintf!(
                        INFO,
                        "{}: BTI index #{} (\"{}\") set to mode FAULT.\n",
                        smmu.name(),
                        ndx,
                        bti_name
                    );
                }
                Err(status) => {
                    dprintf!(
                        INFO,
                        "{}: Failed to set BTI index #{} (\"{}\") to mode FAULT (err {}).\n",
                        smmu.name(),
                        ndx,
                        bti_name,
                        status.into_raw()
                    );
                }
            }
        }
    }
}
