// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/assert.h>

#include <dev/arm_smmu/context_bank.h>
#include <dev/arm_smmu/smmu.h>
#include <dev/arm_smmu/smmu_bti.h>
#include <dev/arm_smmu/smmu_pmt.h>
#include <dev/arm_smmu/stream_match_reg_group.h>
#include <dev/arm_smmu/utils.h>
#include <dev/iommu/pmt.h>
#include <fbl/ref_ptr.h>
#include <ktl/utility.h>

namespace arm_smmu {

SmmuBti::SmmuBti(fbl::RefPtr<Smmu> smmu, uint64_t bti_id)
    : iommu::Bti{bti_id}, smmu_{ktl::move(smmu)} {
  DEBUG_ASSERT(smmu_.get() != nullptr);
}

SmmuBti::~SmmuBti() {
  // By the time we destruct, we should have been shut down already and all of
  // our resources explicitly released.
  DEBUG_ASSERT(smmu_ == nullptr);
  DEBUG_ASSERT(context_bank_ == nullptr);
  DEBUG_ASSERT(smrg_list_.is_empty());
}

zx::result<fbl::RefPtr<iommu::Pmt>> SmmuBti::Map(PinnedVmObject pinned_vmo, uint32_t perms,
                                                 iommu::RequireContiguousMapping req_contig) {
  const uint64_t vmo_offset = pinned_vmo.offset();
  const uint64_t vmo_size = pinned_vmo.size();

  // Start by checking the basics of our request.  IOW - the stuff we can
  // validate without needing to lock anything.
  if (!IsPageRounded(vmo_offset) || vmo_size == 0) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  if (perms & ~(IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE | IOMMU_FLAG_PERM_EXECUTE)) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  if (perms == 0) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  // If there has been a request for a contiguous mapping, then check to see if
  // our pinned VMO is (in fact) contiguous before attempting obtaining our
  // lock.  Once we are holding our lock, we can safely check to see if we are
  // operating in Bypass mode, where we will need to fail the operation if the
  // user desires a contiguous mapping, but the pinned VMO is not contiguous.
  const zx_status_t contiguous_vmo_status = [&]() {
    if (req_contig == iommu::RequireContiguousMapping::No) {
      return ZX_OK;
    }

    paddr_t paddr{iommu::INVALID_PADDR};
    VmObject& vmo = *pinned_vmo.vmo();
    return vmo.LookupContiguous(vmo_offset, vmo_size, &paddr);
  }();

  // Hold our spinlock while we check to make sure we are in a state where
  // mapping is possible. IOW - we should be in either Bypass or Translation
  // mode.  If are in something like fault mode, adopted mode, or shutdown mode,
  // we are likely to stay there for a long time (if not forever), and it is
  // worth it to make a quick check to be sure that there is a chance that this
  // request can succeed before going further.
  //
  // After that, speculatively allocate a SmmuPmt object.  We have to do that
  // with our lock dropped (can't hold a spinlock and touch the heap).
  //
  // After this, however, we need to re-enter our lock and re-validate our mode.
  // It is possible that while we were off fetching memory from the heap, we
  // took a context interrupt and entered the fault state.  In a situation like
  // this, we need to deny the map request as we required to do when a BTI is
  // faulted.
  BtiMode observed_mode;
  {
    Guard<SpinLock, IrqSave> guard{&lock_};

    if ((mode() != BtiMode::kBypass) && (mode() != BtiMode::kTranslation)) {
      return zx::error(ZX_ERR_BAD_STATE);
    }

    // If we are not in translation mode, and the user both wants a contiguous
    // mapping but the underlying pinned VMO is not contiguous (computed in
    // `contiguous_vmo_status`, above) then we need to bail out here.
    if ((mode() != BtiMode::kTranslation) && (contiguous_vmo_status != ZX_OK)) {
      return zx::error(contiguous_vmo_status);
    }

    observed_mode = mode();
  }

  Guard<Mutex> pmt_guard{&pmt_lock_};
  fbl::RefPtr<SmmuPmt> pmt = SmmuPmt::Create(*this, ktl::move(pinned_vmo), observed_mode);
  if (pmt == nullptr) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  // Now enter our lock and attempt to finish the mapping operation.  We need to
  // hold the state of the BTI constant during the rest of the process.
  Guard<SpinLock, IrqSave> guard{&lock_};

  // We made it this far because we observed our BTI as being in either the
  // Bypass or Translation state.  There is no legal way for a BTI to change from
  // Bypass to Translation (or vice-versa) after being created, so if the
  // current state does not match our observed state, then we must have either
  // faulted or been shut down and will need to bail out.  The VMO which had
  // been pinned has been transferred to our PMT and will be unpinned
  // automatically as the PMT destructs.
  //
  // Note that this assumption is predicated on the notion that the Dispatcher
  // level is holding locks which prevent us from:
  //
  // 1) Faulting at an IRQ level and having the driver level BTI enter the fault
  //    state, then
  // 2) Recovering from the fault with a syscall call to ReleaseQuarantine while
  //    we were creating the PMT object.
  if (mode() != observed_mode) {
    return zx::error(ZX_ERR_BAD_STATE);
  }

  // Add the PMT to the list of active PMTs, and mark it as being in the kActive
  // state.  If we are in bypass mode, this should be all we need to do in order
  // to be finished.  If we are in translate mode, we will need reserve a region
  // in our device's address space, then proceed to create the PTE needed to map
  // those device vaddrs to the underlying paddrs of the PMT.
  [&]() TA_REQ(pmt_lock_) {
    pmt->AssertOwnerPmtLockHeld();
    pmt->set_state(SmmuPmt::State::kActive);
    active_pmt_list_.push_back(pmt);
  }();

  if (mode() == BtiMode::kTranslation) {
    // We do not (yet) support translation mode, so we definitely should not
    // find ourselves in translation mode.
    ASSERT(false);
  }

  return zx::ok(ktl::move(pmt));
}

void SmmuBti::ReleaseQuarantine() {
  // The mode we will want to return to depends on the mode our SMMU is
  // operating in.
  const BtiMode target_mode = [&]() -> BtiMode {
    switch (smmu().op_mode()) {
      case ArmSmmuMode::kPassthru:
        return BtiMode::kBypass;

      case ArmSmmuMode::kEnforced:
        return BtiMode::kTranslation;

      case ArmSmmuMode::kDisabled:
      default:
        ASSERT(false);
        return BtiMode::kFault;
    }
  }();

  {
    Guard<Mutex> pmt_guard{&pmt_lock_};
    Guard<SpinLock, IrqSave> guard{&lock_};

    // No one should be calling ReleaseQuarantine on us unless we are in one of
    // the 3 operational states:  Fault, Bypass, or Translation.
    DEBUG_ASSERT((mode() == BtiMode::kFault) || (mode() == BtiMode::kBypass) ||
                 (mode() == BtiMode::kTranslation));

    // ReleaseQuarantine should be idempotent.  If we are already operating in
    // Bypass or Translation, there is nothing to do.
    if (mode() == BtiMode::kFault) {
      // Reset our "quarantine" bookkeeping.
      quarantined_pmt_count_ = 0;
      quarantined_page_count_ = 0;

      zx::result<> set_mode_result = SetModeLocked(target_mode);
      DEBUG_ASSERT(set_mode_result.is_ok());
    }
  }

  // Re-enable our context bank interrupt if needed.
  smmu_->ReenableContextBankIrq(*this);
}

void SmmuBti::OnDispatcherZeroHandles() {
  // Start by checking to see if we still have any PMTs, either active or
  // quarantined.  If we do, we cannot actually fully shut down yet.  Instead,
  // we have become "orphaned".
  //
  // If we still have active PMTs (nothing quarantined) we have to stick around,
  // allowing our HW to access the memory it has pinned until it formally unpins
  // the last PMT it had pinned.  Only then can we fully shut down.
  //
  // If we have quarantined memory, the situation is slightly worse, at least if
  // we are in passthru mode.  If a PMT has been leaked, and its BTI is
  // orphaned, there is no way for user-mode to tell the kernel that it has
  // taken control of its hardware and that it is now safe to un-quarantine its
  // quarantined PMTs.  Typically that would happen with a call to
  // `zx_bti_release_quarantine`, but the last handle to the BTI has been closed
  // preventing user-mode from properly following the protocol.
  //
  // In passthru mode, we can only grant or deny a device access to *all* of
  // physical memory, not only portions of it.  This is good in-so-much as the
  // hardware can be prevented from accessing any memory (allowing us to return
  // quarantined memory to the physical pool) however we need to stay in
  // deny/fault mode pretty much forever as there is no way for user-mode to
  // give us an all-clear sign given that we are orphaned.
  //
  // We should be able to do better in enforced mode.  Once all of the active
  // PMTs have either been formally unpinned, or even leaked and quarantined, we
  // should be able to destroy the BTI instance even if it has been orphaned.
  // The PTE entries will already have been removed and the HW will not be able
  // to access any memory.  If a new instance happens to be re-created, newly
  // pinned memory will only be addressable on a per-physical-page basis, and
  // the newly created BTI will not have any valid PTEs, so the hardware will
  // not be able to access any memory, at least not until it successfully pins
  // new pages.  It is very important for a driver to make certain that has
  // stopped all DMA before pinning any new memory.  Failure to do so risks the
  // driver either leaking or corrupting its newly pinned memory because of its
  // out of control DMA.
  //
  uint64_t quarantined_page_count = 0;
  uint64_t quarantined_pmt_count = 0;
  bool was_orphaned = false;
  {
    Guard<Mutex> pmt_guard{&pmt_lock_};
    Guard<SpinLock, IrqSave> guard{&lock_};
    DEBUG_ASSERT(!orphaned_);

    quarantined_pmt_count = quarantined_pmt_count_;
    quarantined_page_count = quarantined_page_count_;

    // These must either both be zero, or both be non-zero.
    DEBUG_ASSERT((quarantined_pmt_count == 0) == (quarantined_page_count == 0));

    if (quarantined_page_count) {
      // If we have quarantined PMTs, we should already be in the kFault state.
      DEBUG_ASSERT(mode() == BtiMode::kFault);
      orphaned_ = true;
    } else {
      // If we have no (logically) quarantined PMTs, then we are orphaned iff we
      // have active PMTs which are still alive.
      orphaned_ = !active_pmt_list_.is_empty();
    }

    was_orphaned = orphaned_;
  }

  if (!was_orphaned) {
    // If we were not orphaned, then we have no active PMTs and no quarantined
    // PMTs, and user-mode no longer has any handles to us.  We can now safely
    // shut down and remove ourselves from our parent SMMU instance.
    OnEndOfLife();
  } else {
    // If we are now orphaned because we have quarantined PMTs, we need to log a
    // DRIVER OOPS warning.
    PrintQuarantineWarning(BtiPageLeakReason::BtiOrphanedWithQuarantinedPmts,
                           quarantined_page_count, quarantined_pmt_count);
  }
}

void SmmuBti::OnEndOfLife() {
  // We should never be calling OnEndOfLife twice.
  DEBUG_ASSERT(smmu_ != nullptr);
  smmu_->ShutdownBti(*this);

  // The Smmu ref pointer is const member of SmmuBti to prevent us from
  // accidentally releasing it.  That said, here is the only place where we do
  // want to explicitly release the pointer, so take care of it using a
  // const-cast.
  const_cast<fbl::RefPtr<Smmu>&>(smmu_).reset();
}

uint64_t SmmuBti::minimum_contiguity() const {
  switch (smmu_->op_mode()) {
    // If we are operating in passthru mode, then we are not actually doing any
    // translation.  We cannot guarantee any contiguity beyond the CPUs' page
    // size.
    case ArmSmmuMode::kPassthru:
      return kPageSize;

    // If we are operating in fully-enforced mode, then we currently guarantee
    // that all of our mappings will be made contiguous from the perspective of
    // the initiator HW.
    case ArmSmmuMode::kEnforced:
      return ktl::numeric_limits<uint64_t>::max();

    // It should be impossible for our SMMU to be in Disabled mode.  If the
    // bootloaders configured the SMMUs to operate in Disabled mode, then we
    // should never have created an Smmu instance, and therefore should not have
    // been able to create any SmmuBti instances either.  We should have ended
    // up (under the hood) creating a StubIommu instance instead.
    case ArmSmmuMode::kDisabled:
    default:
      DEBUG_ASSERT(false);
      return 0;
  }
}

uint64_t SmmuBti::aspace_size() const {
  switch (smmu_->op_mode()) {
    // If we are operating in passthru mode, then the address space that
    // initiator HW sees is going to be be the physical address space of the
    // system, which is functionally unlimited in size.
    case ArmSmmuMode::kPassthru:
      return ktl::numeric_limits<uint64_t>::max();

    // If we are operating in fully-enforced mode, then the address space that
    // initiator HW sees depends on this BTI's context bank's configuration.
    case ArmSmmuMode::kEnforced: {
      Guard<SpinLock, IrqSave> guard{&lock_};
      return context_bank_->aspace_size();
    }

    // See above, it should be impossible to get here.
    case ArmSmmuMode::kDisabled:
    default:
      DEBUG_ASSERT(false);
      return 0;
  }
}

uint64_t SmmuBti::pmo_count() const {
  Guard<Mutex> pmt_guard{&pmt_lock_};
  return active_pmt_list_.size() + quarantined_pmt_count_;
}

size_t SmmuBti::quarantine_count() const {
  Guard<Mutex> pmt_guard{&pmt_lock_};
  return quarantined_pmt_count_;
}

bool SmmuBti::in_fault_state() const {
  Guard<SpinLock, IrqSave> guard{&lock_};
  // It should not be possible to get here unless we are in an operational state.
  DEBUG_ASSERT((mode() == BtiMode::kFault) || (mode() == BtiMode::kBypass) ||
               (mode() == BtiMode::kTranslation));
  return mode() == BtiMode::kFault;
}

fbl::RefPtr<SmmuBti> SmmuBti::Create(Smmu& smmu, ktl::unique_ptr<StreamMatchRegGroup> smrg,
                                     ktl::unique_ptr<ContextBank> context_bank,
                                     BtiMode initial_mode) {
  // We always use the raw value of our first SMRG's match register as our
  // opaque "bti id".
  DEBUG_ASSERT(smrg != nullptr);
  const uint64_t bti_id = smrg->stream_ids().value();

  fbl::AllocChecker ac;
  fbl::RefPtr<SmmuBti> bti = fbl::AdoptRef<SmmuBti>(new (&ac) SmmuBti(fbl::RefPtr{&smmu}, bti_id));
  if (!ac.check()) {
    return nullptr;
  }

  {
    Guard<SpinLock, IrqSave> guard{&bti->lock_};

    // If this hardware is being adopted, set our initial mode to adopted before
    // proceeding.  This will prevent SetModeLocked from attempting to
    // reconfigure the hardware.  Adopted hardware is supposed to stay in the
    // state it was in when handed to us by the bootloader, and should not even
    // actually change state.
    if (initial_mode == BtiMode::kAdopted) {
      DEBUG_ASSERT(!context_bank || context_bank->mode() == BtiMode::kAdopted);
      bti->mode_ = BtiMode::kAdopted;
    }

    if (context_bank != nullptr) {
      DEBUG_ASSERT(smmu.available_cbs_.TestBit(context_bank->cb_ndx()));
      smmu.available_cbs_.ClrBit(context_bank->cb_ndx());
      bti->context_bank_ = ktl::move(context_bank);
    }

    bti->AddSmrgLocked(smmu, ktl::move(smrg));

    // In theory, setting the initial mode can never fail.  All of the resources
    // which might be needed (the SMRG and Context Bank) have already been
    // allocated, we just need to set up their registers.  No PTE pages ever
    // have to be allocated at this point in time as we will (initially) have no
    // pinned memory.
    [[maybe_unused]] zx::result<> res = bti->SetModeLocked(initial_mode);
    DEBUG_ASSERT(!res.is_error());

    // If we have a context bank, associate ourselves with our context bank's
    // interrupt, then enable the interrupt at the context bank level.
    if (bti->context_bank_ != nullptr) {
      const uint32_t cb_ndx = bti->context_bank_->cb_ndx();
      smmu.AssociateBtiIrq(*bti, cb_ndx);

      hwreg::RegisterMmio cb_base = smmu.get_cb_base(cb_ndx);
      s1cbr::SCTLR::Get().ReadFrom(&cb_base).set_CFIE(1).WriteTo(&cb_base);
    }
  }

  return bti;
}

void SmmuBti::Shutdown(Smmu& smmu) {
  fbl::SizedDoublyLinkedList<fbl::RefPtr<SmmuPmt>> local_pmt_list;
  fbl::DoublyLinkedList<ktl::unique_ptr<StreamMatchRegGroup>> local_smrg_list;
  ktl::unique_ptr<ContextBank> local_context_bank;

  {
    Guard<Mutex> pmt_guard{&pmt_lock_};
    Guard<SpinLock, IrqSave> guard{&lock_};
    DEBUG_ASSERT(mode() != BtiMode::kShutdown);

    // Lock down all of the hardware, and place ourselves in the shutdown
    // state first. This should never fail as we should never be shutting down
    // any adopted configuration.
    zx::result mode_res = SetModeLocked(BtiMode::kShutdown);
    ASSERT(mode_res.is_ok());

    // Mark our resources in the SMMU bookkeeping as being available for
    // allocation once again.  We are finished messing with the registers; they
    // belong to our SMMU instance once again.
    for (const StreamMatchRegGroup& smrg : smrg_list_) {
      DEBUG_ASSERT(!smmu.available_smrgs_.TestBit(smrg.smrg_ndx()));
      smmu.available_smrgs_.SetBit(smrg.smrg_ndx());
    }

    if (context_bank_ != nullptr) {
      // At this point in time, we should be certain that we no longer have any
      // registered context bank IRQs, and that there are not any context bank
      // interrupts in flight targeting this BTI.  This was taken care of for us
      // by our SMMU instance during Smmu::ShutdownBti.
      DEBUG_ASSERT(!smmu.available_cbs_.TestBit(context_bank_->cb_ndx()));
      smmu.available_cbs_.SetBit(context_bank_->cb_ndx());
    }

    // If we are shutting down, we should not have any active or quarantined PMTs left.
    DEBUG_ASSERT(active_pmt_list_.is_empty());
    DEBUG_ASSERT(quarantined_pmt_count_ == 0);
    DEBUG_ASSERT(quarantined_page_count_ == 0);

    // Move our resources outside of our spinlock's scope so that they can
    // return to the heap after the lock has been dropped.
    local_smrg_list = ktl::move(smrg_list_);
    local_context_bank = ktl::move(context_bank_);
  }

  // Now go ahead and destroy the resource bookkeeping.
  local_smrg_list.clear();
  local_context_bank.reset();
}

bool SmmuBti::SmrIntersects(SmrValue stream_ids) const {
  Guard<SpinLock, IrqSave> guard{&lock_};

  for (const StreamMatchRegGroup& smrg : smrg_list_) {
    if (smrg.stream_ids().Intersects(stream_ids)) {
      return true;
    }
  }

  return false;
}

void SmmuBti::AssertOwned(StreamMatchRegGroup& smrg) {
  // Right now, in all of the practical systems we have encountered so far, a
  // BTI's list of SMRGs is pretty much only one, or at most 2, entries long.
  // It seems reasonable to perform this O(n) validation in systems with
  // DEBUG_ASSERT implemented if N is expected to be a low as this.
  if constexpr (DEBUG_ASSERT_IMPLEMENTED) {
    for (const StreamMatchRegGroup& x : smrg_list_) {
      if (&x == &smrg) {
        return;
      }
    }
    DEBUG_ASSERT(false);
  }
}

void SmmuBti::AddSmrg(Smmu& smmu, ktl::unique_ptr<StreamMatchRegGroup> smrg) {
  Guard<SpinLock, IrqSave> guard{&lock_};
  AddSmrgLocked(smmu, ktl::move(smrg));
}

void SmmuBti::AddSmrgLocked(Smmu& smmu, ktl::unique_ptr<StreamMatchRegGroup> smrg) {
  // Currently, there are only three places that we ever add an SMRG to a BTI.
  //
  // 1) As we create the BTI for a user's new BtiDispatcher.
  // 2) As we adopt the SMRG from initial hardware state and create a new BTI to
  //    contain it.
  // 3) During adoption when we find a another SMRG to adopt which is in
  //    translation mode and whose CBNdx matches this BTI's currently configured
  //    context bank.
  //
  // In case #1, the SMRG we just created should have come from the pool of free
  // SMRGs, and should be in the Invalid state with an invalid context bank
  // index.
  //
  // For case #2, the SMRG may or may not have an associated context bank, but
  // if it does, the SMRG should be in translation mode, and the BTI we created
  // for the adopted SMRG should already have an adopted the ContextBank as
  // specified by the SMRG's register state.
  //
  // Finally, for case #3, the only reason the adoption code should be adding
  // another SMRG to this BTI would be because it discovered one in Translation
  // mode who's cb_ndx matches the index of this BTI's context bank.
  //
  // So, if this SMRG is in translation mode, assert that we have a context
  // bank, and the index of the context bank matches that of the adopted SMRG.
  //
  // TODO(johngro): When we get around to allowing user-mode to adding a new
  // SMRG to an existing BTI, we need make sure that the SMRG (which will be in
  // a locked down state) is reconfigured to match the BTI's current state.  IOW
  // - If the BTI is operating, the SMRG will need to be configured for
  // translation, pointed at this BTI's CB, and enabled.
  //
  if (smrg->mode() == S2CR_Type::kTranslation) {
    DEBUG_ASSERT(context_bank_ != nullptr);
    DEBUG_ASSERT(context_bank_->cb_ndx() == smrg->cb_ndx());
  }

  // The SMRG we are adding should not be marked as in-use at the SMMU level
  // (yet).
  DEBUG_ASSERT(smmu.available_smrgs_.TestBit(smrg->smrg_ndx()));

  // Everything checks out.  Add the SMRG to our list, and mark it as in use in
  // our parent SMMU.
  smmu.available_smrgs_.ClrBit(smrg->smrg_ndx());
  smrg_list_.push_back(ktl::move(smrg));
}

zx::result<> SmmuBti::SetModeLocked(BtiMode target_mode) {
  // Are we already there?
  if (target_mode == mode_) {
    return zx::ok();
  }

  // Adopted and Shutdown BTIs can never change mode.
  if ((mode_ == BtiMode::kAdopted) || (mode_ == BtiMode::kShutdown)) {
    return zx::error{ZX_ERR_BAD_STATE};
  }

  // We were not adopted, so we should be able to assert that we have a context
  // bank.  Do so, then configure our context bank for its new mode.
  DEBUG_ASSERT(context_bank_ != nullptr);
  context_bank_->SetMode(*this, target_mode);

  for (StreamMatchRegGroup& smrg : smrg_list_) {
    // Except for when we are shutting down (and we disable access at all
    // levels), we control all of our enforcement policy through only the
    // context bank.  So, if we are shutting down, disable all of the match
    // registers, otherwise enable them and point them at our context bank.
    if (target_mode == BtiMode::kShutdown) {
      smrg.Disable(*this);
    } else {
      smrg.EnableForContextBank(*this, context_bank_->cb_ndx());
    }
  }

  mode_ = target_mode;
  return zx::ok();
}

void SmmuBti::HandleFaultLocked() {
  // TODO(johngro) currently, the only thing for us to do here is to enter
  // "Fault" mode. Some day, we'd like to define a signal that we can raise on
  // our BTI object so that user-mode is aware that this has happened.  See the
  // section "BTI Dispatchers should have a 'faulted' signal" in the top-level
  // README.md for more details.
  //
  // There should be nothing for us to do if we are already shut down.
  if (mode() != BtiMode::kShutdown) {
    // The only way this should ever be the case is if we have adopted hardware
    // whose SMRG registers didn't specify a valid context bank, but secretly
    // actually had one which was configured using virtualised register
    // techniques by the system at either EL2/sEL3.  In theory, we can promote
    // this to an ASSERT, but for now it is just a warning.
    if (context_bank_ == nullptr) {
      dprintf(INFO,
              "%s: WARNING - Context fault received, but BTI has no associated context bank.\n",
              smmu_->name());
    }

    const zx::result<> result = SetModeLocked(BtiMode::kFault);
    ktl::array<char, 128> sid_buffer{0};
    if (result.is_error()) {
      dprintf(INFO,
              "%s: WARNING - Failed to place BTI controlling StreamID(s) [%s] into "
              "fault mode (err %d).\n",
              smmu_->name(), RenderSidList(sid_buffer), result.error_value());
    } else {
      dprintf(INFO, "%s: BTI controlling StreamID(s) [%s] has entered fault mode.\n", smmu_->name(),
              RenderSidList(sid_buffer));
    }
  }
}

uint32_t SmmuBti::cb_ndx_locked() const {
  return context_bank_ ? context_bank_->cb_ndx() : ktl::numeric_limits<uint32_t>::max();
}

uint32_t SmmuBti::cb_ndx() const {
  Guard<SpinLock, IrqSave> guard{&lock_};
  return cb_ndx_locked();
}

zx::result<> SmmuBti::InvalidateSids() {
  Guard<SpinLock, IrqSave> guard{&lock_};

  // Adopted and Shutdown BTIs can never invalidate their Stream IDs.
  if ((mode_ == BtiMode::kAdopted) || (mode_ == BtiMode::kShutdown)) {
    return zx::error{ZX_ERR_BAD_STATE};
  }

  for (StreamMatchRegGroup& smrg : smrg_list_) {
    smrg.Invalidate(*this);
  }

  return zx::ok();
}

const char* SmmuBti::RenderSidList(ktl::span<char> buffer) const {
  // StringFile will stop accumulating characters and automatically truncate the
  // string if it runs out of room in |buffer|.
  StringFile f{buffer};
  bool first{true};

  for (const StreamMatchRegGroup& smrg : smrg_list_) {
    for (const uint16_t id : smrg.stream_ids()) {
      fprintf(&f, "%s0x%04hx", first ? "" : " ", id);
      first = false;
    }
  }

  return ktl::move(f).take().data();
}

void SmmuBti::OnPmtUnpin(SmmuPmt& pmt) {
  // If there is still a PMO to unpin, move it outside of the scope of the lock
  // before doing so.
  //
  // Use a local lambda to prevent the AssertOwnerPmtLockHeld from escaping the
  // scope of the Guard.
  PinnedVmObject released_vmo = [&]() {
    Guard<Mutex> pmt_guard{&pmt_lock_};
    Guard<SpinLock, IrqSave> guard{&lock_};

    pmt.AssertOwnerPmtLockHeld();
    if (pmt.state() == SmmuPmt::State::kActive) {
      DEBUG_ASSERT(pmt.InContainer());
      active_pmt_list_.erase(pmt);
      pmt.set_state(SmmuPmt::State::kReleased);
    } else {
      DEBUG_ASSERT(!pmt.InContainer());
      DEBUG_ASSERT(pmt.pinned_vmo().vmo() == nullptr);
    }

    return pmt.TakePinnedVmo();
  }();

  // Now that the lock is dropped, if we took ownership of the PMO, we can let
  // it go out of scope now, releasing the pages back to the PMM as we do.
  released_vmo.reset();
}

void SmmuBti::OnPmtZeroHandles(SmmuPmt& pmt) {
  uint64_t quarantined_page_count{0};
  uint64_t quarantined_pmt_count{0};
  bool has_active_pmts{false};
  bool was_orphaned{false};

  // If there is still a PMO to unpin, move it outside of the scope of the lock
  // before doing so.
  //
  // Use a small local lambda to prevent the AssertOwnerPmtLockHeld from escaping
  // the scope of the Guard.
  PinnedVmObject quarantined_vmo = [&]() {
    Guard<Mutex> pmt_guard{&pmt_lock_};
    Guard<SpinLock, IrqSave> guard{&lock_};
    pmt.AssertOwnerPmtLockHeld();

    DEBUG_ASSERT(pmt.state() != SmmuPmt::State::kInitial);
    if (pmt.state() == SmmuPmt::State::kActive) {
      // If a PMT has its last handle closed before being formally unpinned,
      // then we have leaked the PMT and will need to quarantine it.
      //
      // Start by entering fault mode.
      zx::result<> set_fault_mode_result = SetModeLocked(BtiMode::kFault);
      ASSERT(set_fault_mode_result.is_ok());

      // Remove the PMT from the active set of PMTs and flag it as quarantined.
      DEBUG_ASSERT(pmt.InContainer());
      active_pmt_list_.erase(pmt);
      pmt.set_state(SmmuPmt::State::kQuarantined);

      // Update our stats and take note of what they are so we can log an
      // appropriate warning.
      quarantined_page_count = (quarantined_page_count_ += pmt.pages());
      quarantined_pmt_count = (quarantined_pmt_count_ += 1);
    } else {
      // If this PMT was not active, then it should be in the Released state and
      // not on the active list.
      DEBUG_ASSERT(pmt.state() == SmmuPmt::State::kReleased);
      DEBUG_ASSERT(!pmt.InContainer());
      DEBUG_ASSERT(pmt.pinned_vmo().vmo() == nullptr);
    }

    has_active_pmts = !active_pmt_list_.is_empty();
    was_orphaned = orphaned_;
    return pmt.TakePinnedVmo();
  }();

  // Now that we are outside of the locks, we can handle any final cleanup
  // tasks.
  //
  // 1) If this PMT was leaked, we need to log a warning.
  // 2) If we were orphaned, and this was the last PMT (active or quarantined),
  //    then we have reached end of life and can clean up now.
  // 3) If we quarantined a pinned memory object, we can now return its pages to
  //    the PMM.  We don't need to actually keep it around, since we have either
  //    revoked access to that specific region of memory (if we are in enforced
  //    mode), or revoked the device's access to all memory (because we are in
  //    passthru mode).

  // #1 : Log a warning if needed.
  if (quarantined_vmo.vmo() != nullptr) {
    const BtiPageLeakReason reason = was_orphaned ? BtiPageLeakReason::PmtQuarantinedWhenBtiOrphaned
                                                  : BtiPageLeakReason::PmtQuarantined;
    PrintQuarantineWarning(reason, quarantined_page_count, quarantined_pmt_count);
  }

  // #2 : Shutdown if it is time.
  DEBUG_ASSERT((quarantined_page_count == 0) == (quarantined_pmt_count == 0));
  if (was_orphaned && !has_active_pmts && !quarantined_pmt_count) {
    OnEndOfLife();
  }

  // #3 : Release our VMO (if any).
  quarantined_vmo.reset();
}

}  // namespace arm_smmu
