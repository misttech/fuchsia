// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM SMMUv2 Stream Match Register Group (`SMRn` + `S2CRn`) state and hardware programming.
//!
//! Each [`StreamMatchRegGroup`] manages one `(SMRn, S2CRn)` register pair in Global Register
//! Space 0 (`GR0`) and can be linked into an `SmmuBti`'s intrusive doubly-linked list of stream
//! match groups.

use core::pin::Pin;
use debug::dprintf;
use fbl::{DoublyLinkedListContainable, DoublyLinkedListNode, Recyclable, UniquePtr};
use kalloc::AllocError;
use zx_status::Status;

use crate::arm_smmu::common::SmmuEnv;
use crate::arm_smmu::v2::registers::SmmuMmio;
use crate::arm_smmu::v2::registers::gr0::{S2cr, S2crPrivCfg, S2crType, S2crType0, Smr};
use crate::arm_smmu::v2::utils::{SmmuController, SmrValue};

/// Representation of a single SMMUv2 Stream Match Register Group (`SMRn` + `S2CRn`).
#[derive(DoublyLinkedListContainable, Recyclable)]
pub struct StreamMatchRegGroup {
    #[dll_node]
    list_node: DoublyLinkedListNode<StreamMatchRegGroup>,
    smrg_ndx: u32,
    stream_ids: SmrValue,
    gr0_base: SmmuMmio,
    mode: S2crType,
    cb_ndx: u32,
}

// SAFETY: The only `!Send` field of `StreamMatchRegGroup` is
// `list_node: DoublyLinkedListNode<StreamMatchRegGroup>` (which contains
// `UnsafeCell<NodeLinks<Self>>`). `StreamMatchRegGroup` and any intrusive list containing it are
// only accessed or transferred across threads while holding the owning `Smmu` or `SmmuBti`
// synchronization lock.
unsafe impl Send for StreamMatchRegGroup {}

impl Drop for StreamMatchRegGroup {
    #[inline]
    fn drop(&mut self) {
        debug_assert!(!self.list_node.in_container());
        debug_assert!(self.gr0_base.is_null());
        debug_assert!(self.mode == S2crType::Invalid);
        debug_assert!(self.cb_ndx == Self::INVALID_CB_NDX);
    }
}

impl StreamMatchRegGroup {
    /// Sentinel value indicating that no Context Bank index is associated with this SMRG.
    pub const INVALID_CB_NDX: u32 = 0xffff_ffff;

    /// Constructs a new, uninitialized [`StreamMatchRegGroup`] for `smrg_ndx` and `stream_ids`.
    #[inline]
    const fn new(smrg_ndx: u32, stream_ids: SmrValue) -> Self {
        Self {
            list_node: DoublyLinkedListNode::new(),
            smrg_ndx,
            stream_ids,
            gr0_base: SmmuMmio::null(),
            mode: S2crType::Invalid,
            cb_ndx: Self::INVALID_CB_NDX,
        }
    }

    /// Returns the hardware index (`0..num_smrgs`) of this Stream Match Register Group.
    #[inline]
    pub const fn smrg_ndx(&self) -> u32 {
        self.smrg_ndx
    }

    /// Returns the encoded [`SmrValue`] (`MASK` and `ID`) matched by this group.
    #[inline]
    pub const fn stream_ids(&self) -> SmrValue {
        self.stream_ids
    }

    /// Returns the current [`S2crType`] operating mode of this group.
    #[inline]
    pub const fn mode(&self) -> S2crType {
        self.mode
    }

    /// Returns the associated Context Bank index, or [`Self::INVALID_CB_NDX`] if none.
    #[inline]
    pub const fn cb_ndx(&self) -> u32 {
        self.cb_ndx
    }

    /// Returns the `GR0` MMIO bank handle associated with this group.
    #[inline]
    pub const fn gr0_base(&self) -> SmmuMmio {
        self.gr0_base
    }

    /// Returns `true` if this SMRG is currently linked into an intrusive doubly-linked list.
    #[inline]
    pub fn in_container(&self) -> bool {
        self.list_node.in_container()
    }

    /// Disables the hardware `SMRn` and `S2CRn` registers for `smrg_ndx` in `gr0_base`.
    fn disable_regs(gr0_base: SmmuMmio, smrg_ndx: u32) {
        // Note: this code assumes that we are not running with extended (16-bit) stream IDs
        // enabled. This is enforced during initialization of the top-level SMMU itself.
        Smr::read_at(&gr0_base, smrg_ndx)
            .set_valid(0)
            .set_mask(0)
            .set_id(0)
            .write_at(&gr0_base, smrg_ndx);
        S2cr::read_at(&gr0_base, smrg_ndx)
            .set_s2cr_type(S2crType::Fault)
            .write_at(&gr0_base, smrg_ndx);
        SmmuEnv::device_memory_barrier();
    }

    /// Static helper that validates `smrg_ndx` is available in `smmu` and disables its hardware
    /// registers.
    pub fn disable_at<S: SmmuController>(smmu: &S, smrg_ndx: u32) {
        debug_assert!(smrg_ndx < smmu.num_smrgs());
        debug_assert!(smmu.available_smrgs().test_bit(smrg_ndx));
        Self::disable_regs(smmu.gr0_base(), smrg_ndx);
    }

    /// Disables this SMRG's hardware registers and clears out the bookkeeping that indicates it
    /// was enabled.
    ///
    /// There is no coming back from this state, and it will be checked as the
    /// [`StreamMatchRegGroup`] drops.
    ///
    /// Note: Exclusive access is statically enforced because every live [`StreamMatchRegGroup`] is
    /// owned by `SmmuBti::smrg_list` (`#[guarded_by(lock)]`) or exclusively held during
    /// `Smmu::init` before BTI publication, so obtaining `&mut self` requires holding the owning
    /// lock.
    pub fn disable(&mut self) {
        // Disable ourselves, and clear out the bookkeeping that indicates that we were enabled.
        // There is no coming back from this state, and it will be checked as we destruct.
        Self::disable_regs(self.gr0_base, self.smrg_ndx);
        self.gr0_base = SmmuMmio::null();
        self.mode = S2crType::Invalid;
        self.cb_ndx = Self::INVALID_CB_NDX;
    }

    /// Pinned wrapper for [`Self::disable`] when iterating over an intrusive list of
    /// [`StreamMatchRegGroup`]s (`SmmuBti::smrg_list`).
    #[inline]
    pub fn disable_pinned(self: Pin<&mut Self>) {
        // SAFETY: `disable` only mutates `gr0_base`, `mode`, and `cb_ndx`; it does not move `self`
        // or touch `list_node`.
        unsafe { self.get_unchecked_mut().disable() }
    }

    /// Clears the `VALID` flag in the `SMRn` match register to force a situation where Stream ID
    /// matching for a BTI fails (used only by debug console commands).
    pub fn invalidate(&self) {
        Smr::read_at(&self.gr0_base, self.smrg_ndx)
            .set_valid(0)
            .write_at(&self.gr0_base, self.smrg_ndx);
    }

    /// Called when users release quarantine on a BTI.
    ///
    /// Typically, when a BTI goes into fault mode the enforcement happens at the context bank
    /// level, but this is called as well just in case the reason for the faults is that someone
    /// used the console to invalidate the SIDs in a BTI.
    ///
    /// Note that during normal operation, we would never invalidate a BTI's SMRGs. That would only
    /// happen at the end of the operational life of a BTI. The kernel console can be used to
    /// deliberately invalidate a BTI's SIDs for manual testing purposes, or to help with the
    /// exploration of a platform's specific behavior during bringup of a new platform.
    pub fn revalidate(&self) {
        Smr::read_at(&self.gr0_base, self.smrg_ndx)
            .set_valid(1)
            .write_at(&self.gr0_base, self.smrg_ndx);
    }

    /// Validates that `smrg_ndx` is within `[0, smmu.num_smrgs())` and marked available in
    /// `smmu.available_smrgs()`.
    fn validate_ndx<S: SmmuController>(smmu: &S, smrg_ndx: u32) -> Result<(), Status> {
        if smrg_ndx >= smmu.num_smrgs() {
            return Err(Status::INVALID_ARGS);
        }

        if !smmu.available_smrgs().test_bit(smrg_ndx) {
            return Err(Status::ALREADY_BOUND);
        }

        Ok(())
    }

    /// Allocates a [`UniquePtr<StreamMatchRegGroup>`] and caches `smmu.gr0_base()`.
    fn create<S: SmmuController>(
        smmu: &S,
        smrg_ndx: u32,
        stream_ids: SmrValue,
    ) -> Result<UniquePtr<Self>, Status> {
        let mut smrg = UniquePtr::try_new(Self::new(smrg_ndx, stream_ids))
            .map_err(|AllocError| Status::NO_MEMORY)?;

        // Cache references to the base addresses for our registers. While we exist, we are the
        // only thing allowed to write to them. Specifically, we own the following registers in
        // global register space 0:
        //
        // + SMR(smrg_ndx)
        // + S2CR(smrg_ndx)
        smrg.gr0_base = smmu.gr0_base();
        Ok(smrg)
    }

    /// Validates `smrg_ndx`, allocates a [`UniquePtr<StreamMatchRegGroup>`], and disables its
    /// hardware registers (`SMRn` and `S2CRn`).
    pub fn create_and_lockdown<S: SmmuController>(
        smmu: &S,
        smrg_ndx: u32,
        stream_ids: SmrValue,
    ) -> Result<UniquePtr<Self>, Status> {
        Self::validate_ndx(smmu, smrg_ndx)?;
        let smrg = Self::create(smmu, smrg_ndx, stream_ids)?;

        // To lock-down, disable this SMRG at the register level, and we should be finished.
        Self::disable_regs(smrg.gr0_base, smrg.smrg_ndx);
        Ok(smrg)
    }

    /// Validates `smrg_ndx`, allocates a [`UniquePtr<StreamMatchRegGroup>`], and adopts its
    /// pre-existing hardware register state.
    pub fn create_and_adopt<S: SmmuController>(
        smmu: &S,
        smrg_ndx: u32,
        stream_ids: SmrValue,
    ) -> Result<UniquePtr<Self>, Status> {
        Self::validate_ndx(smmu, smrg_ndx)?;
        let mut smrg = Self::create(smmu, smrg_ndx, stream_ids)?;
        if let Err(status) = smrg.adopt_register_state(smmu) {
            // Temporary porting note: the C++ `CreateAndAdopt` returned the error without doing
            // any of this, so a failed adoption tripped `~StreamMatchRegGroup`'s debug asserts.
            //
            // `create` initialized `smrg.gr0_base` and `adopt_register_state` may have updated
            // `smrg.mode` / `smrg.cb_ndx` before returning an error. Reset these fields back to
            // their disabled/uninitialized sentinels before `smrg` is dropped so `Drop`'s debug
            // assertions do not trip on the error path.
            smrg.gr0_base = SmmuMmio::null();
            smrg.mode = S2crType::Invalid;
            smrg.cb_ndx = Self::INVALID_CB_NDX;
            return Err(status);
        }
        Ok(smrg)
    }

    /// Adopts the pre-existing hardware state of `S2CR(self.smrg_ndx)` from `smmu`.
    fn adopt_register_state<S: SmmuController>(&mut self, smmu: &S) -> Result<(), Status> {
        let s2cr = S2cr::read_at(&smmu.gr0_base(), self.smrg_ndx);
        self.mode = s2cr.s2cr_type();

        if self.mode == S2crType::Invalid {
            dprintf!(
                INFO,
                "{} ERROR: Bad S2CR type ({}) when attempting to adopt SMRG #{}\n",
                smmu.name(),
                self.mode as u32,
                self.smrg_ndx
            );
            return Err(Status::BAD_STATE);
        }

        if self.mode == S2crType::Translation {
            let s2cr_t0 = S2crType0::from_value(s2cr.reg_value());

            if s2cr_t0.cbndx() >= smmu.num_cbs() {
                dprintf!(
                    INFO,
                    "{} ERROR: Invalid context bank index ({}) when attempting to adopt SMRG #{}\n",
                    smmu.name(),
                    s2cr_t0.cbndx(),
                    self.smrg_ndx
                );
                return Err(Status::OUT_OF_RANGE);
            }

            self.cb_ndx = s2cr_t0.cbndx();
        }

        Ok(())
    }

    /// Enables this SMRG in translation mode pointing at `cb_ndx`.
    ///
    /// Note: Exclusive access is statically enforced because every live [`StreamMatchRegGroup`] is
    /// stored in `SmmuBti::smrg_list` (`#[guarded_by(lock)]`), so obtaining `&mut self` requires
    /// holding the owning BTI's lock.
    pub fn enable_for_context_bank(&mut self, cb_ndx: u32) {
        debug_assert!((self.mode == S2crType::Invalid) == (self.cb_ndx == Self::INVALID_CB_NDX));
        if self.mode == S2crType::Invalid {
            self.mode = S2crType::Translation;
            self.cb_ndx = cb_ndx;

            // Note: this code assumes that we are not running with extended (16-bit) stream IDs
            // enabled. This is enforced during initialization of the top-level SMMU itself.
            //
            // For the S2CR, start with a value of 0. This will set all of the overrides for all
            // of the hints (transient, instruction fetch, write allocation, etc.) to "default".
            //
            // Then configure the register to indicate translation mode, and direct it to our
            // context bank. Finally, attempt to configure PRIVCFG to unconditionally flag all
            // transactions as "Unprivileged". Whether or not this will actually change
            // transactions depends on the IDR2.DIPANS bit.
            //
            // Forcing accesses to be unprivileged provides finer-grained access control in
            // translation mode when using VMSAv8-64 translation tables. Specifically, it is not
            // possible to deny Read access to a privileged access when there exists a valid
            // translation, but it is possible when accesses are unprivileged. See section
            // D8.4.1.2.6 Table D8-62 in the ARM ARM.
            S2crType0::from_value(0)
                .set_s2cr_type(S2crType::Translation)
                .set_cbndx(self.cb_ndx)
                .set_privcfg(S2crPrivCfg::Unprivileged)
                .write_at(&self.gr0_base, self.smrg_ndx);

            // Temporary porting note: the C++ `EnableForContextBank` passed the entire encoded
            // `stream_ids_.value()` (`(MASK << 16) | ID`) to `set_ID` rather than just the ID.
            // That tripped hwreg's field-range debug assert for any non-zero mask, and only
            // produced the right register value in release builds because the stray bits landed
            // exactly on the MASK field. Only the ID is written to the ID field here.
            Smr::from_value(0)
                .set_valid(1)
                .set_mask(u32::from(self.stream_ids.mask()))
                .set_id(u32::from(self.stream_ids.id()))
                .write_at(&self.gr0_base, self.smrg_ndx);

            SmmuEnv::device_memory_barrier();
        }
    }

    /// Pinned wrapper for [`Self::enable_for_context_bank`] when iterating over an intrusive list
    /// of [`StreamMatchRegGroup`]s (`SmmuBti::smrg_list`).
    #[inline]
    pub fn enable_for_context_bank_pinned(self: Pin<&mut Self>, cb_ndx: u32) {
        // SAFETY: `enable_for_context_bank` only mutates `mode` and `cb_ndx`; it does not move
        // `self` or touch `list_node`.
        unsafe { self.get_unchecked_mut().enable_for_context_bank(cb_ndx) }
    }
}
