// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <dev/arm_smmu/context_bank.h>
#include <dev/arm_smmu/smmu.h>
#include <dev/arm_smmu/smmu_registers.h>
#include <fbl/ref_ptr.h>
#include <ktl/utility.h>

namespace arm_smmu {

ContextBank::ContextBank(uint32_t cb_ndx) : cb_ndx_(cb_ndx) {}
ContextBank::~ContextBank() {
  DEBUG_ASSERT(gr1_base_.base() == 0);
  DEBUG_ASSERT(cb_base_.base() == 0);
  DEBUG_ASSERT(mode_ == BtiMode::kShutdown);
}

ktl::unique_ptr<ContextBank> ContextBank::Create(Smmu& smmu, uint32_t cb_ndx) {
  fbl::AllocChecker ac;
  ktl::unique_ptr<ContextBank> cb{new (&ac) ContextBank{cb_ndx}};
  if (!ac.check()) {
    return nullptr;
  }

  // Cache references to the base addresses for our registers.  While we exist,
  // we are the only thing allowed to write to them.  Specifically, we own all
  // of the registers in context bank register page #cb_ndx, and the following
  // registers in global register space 1.
  //
  // + CBAR(cb_ndx)
  // + CB2R(cb_ndx)
  // + CBFRSSYNRA(cb_ndx)
  //
  cb->gr1_base_ = smmu.gr1_base_;
  cb->cb_base_ = smmu.get_cb_base(cb_ndx);

  return cb;
}

zx::result<ktl::unique_ptr<ContextBank>> ContextBank::CreateAndLockdown(Smmu& smmu,
                                                                        uint32_t cb_ndx) {
  if (zx::result<> res = ValidateNdx(smmu, cb_ndx); res.is_error()) {
    return res.take_error();
  }

  ktl::unique_ptr<ContextBank> cb = Create(smmu, cb_ndx);
  if (cb == nullptr) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  DisableRegs(cb->gr1_base_, cb->cb_base_, cb->cb_ndx_);
  return zx::ok(ktl::move(cb));
}

zx::result<ktl::unique_ptr<ContextBank>> ContextBank::CreateAndAdopt(Smmu& smmu, uint32_t cb_ndx) {
  if (zx::result<> res = ValidateNdx(smmu, cb_ndx); res.is_error()) {
    return res.take_error();
  }

  ktl::unique_ptr<ContextBank> cb = Create(smmu, cb_ndx);
  if (cb == nullptr) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  if (zx::result<> res = cb->AdoptRegisterState(smmu); res.is_error()) {
    return res.take_error();
  }

  return zx::ok(ktl::move(cb));
}

uint64_t ContextBank::aspace_size() {
  if (mode() == BtiMode::kTranslation) {
    // We currently only support AArch64 addressing, and only ever use the
    // bottom half of our address space (via TTBR0).  Computing the effective
    // size of our address space can be done using TCR.T0SZ with the formula:
    //
    // size = 2**(64 - TCR.T0SZ)
    //
    // See Section 1.5.1 "Defining the VA subranges for stage 1 translations"
    // for more details.
    //
    DEBUG_ASSERT(addr_mode() == AddrMode::k64Bit);  // We should always be using 64 bit mode
    s1cbr::TCR_64Bit tcr = s1cbr::TCR_64Bit::Get().ReadFrom(&cb_base_);

    // We should never configure for a full 64-bit address space as doing so
    // would make it impossible to report our actual `aspace_size` in byte
    // units, which is what the current ABI demands.
    DEBUG_ASSERT(tcr.T0SZ() != 0);

    return uint64_t{1} << (64 - tcr.T0SZ());
  } else if (mode() == BtiMode::kBypass) {
    // When we are operating in passthru mode, the addresses we will be
    // returning for pinned memory will be PA/IPAs of the underlying VMO.  The max physical address
    // we can encounter should be determined by our page size and translation table format.  For
    // VMSAv8-64, we get 36 bits of address, plus the number of bits in our page size.  This is the
    // maximum size of a physical address an MMU can output, so it should be safe to report it as
    // the "size of the device's address space".  Any address it gets back from a pin operation
    // should be somewhere in this range (48 bits, when we are using 4k pages).
    //
    constexpr uint32_t kAddrSpaceBits = 36 + kPageShift;
    static_assert(kAddrSpaceBits < 64,
                  "Page size too large to compute maximum physical address size for this system");
    return uint64_t{1} << kAddrSpaceBits;
  } else {
    return 0;
  }
}

uint32_t ContextBank::DecodeGranuleSizeBits(uint32_t reg_bits) const {
  switch (reg_bits) {
    case 0:
      return 12;  // 4KB
    case 1:
      return 16;  // 64KB
    case 2:
      return 14;  // 16KB
    default:
      dprintf(INFO, "WARNING - unrecognized encoded context bank granule size (%u) in CB %u\n",
              reg_bits, cb_ndx_);
      return 0;
  }
}

void ContextBank::DecodeTtbrRegions(uint32_t t0sz, uint32_t t1sz) {
  // First valid address for TTBR0 is always 0x0.
  ttbrs_[0].first_valid_addr = 0x0;

  switch (addr_mode_) {
    case AddrMode::k64Bit: {
      // Section 1.5.1, Figure 1-7
      //
      // Notes about the valid range of t[01]sz.  The register field width of
      // the size fields in the TCR is 6 bits (when using VA64 addressing), so
      // we should be able to assert that these values are < 64.
      //
      // Values on the range [0, 12) don't really make any sense.  The maximum
      // size of an address space when using VSMAv8-64 page tables is 52 bits,
      // 36 bits from the page tables and a 16 bit (max) page size.  That said,
      // while they don't make sense, they can be programmed.  When filling out
      // bookkeeping while adopting register state, compute the range's size in
      // a naive fashion, simply applying the `2**(64 - t[01]sz)` formula while
      // ignoring the fact that values less than 12 might be nonsensical.  Be
      // sure to special case sz == 0.  It is technically illegal to shift a 64
      // bit value to the left by >= 64 bits.
      DEBUG_ASSERT(t0sz < 64);
      DEBUG_ASSERT(t1sz < 64);
      ttbrs_[0].last_valid_addr = t0sz ? ((uint64_t{1} << (64 - t0sz)) - 1) : 0xFFFF'FFFF'FFFF'FFFF;
      ttbrs_[1].first_valid_addr = t1sz ? ~((uint64_t{1} << (64 - t1sz)) - 1) : 0x0;
      ttbrs_[1].last_valid_addr = 0xFFFF'FFFF'FFFF'FFFF;
    } break;
    case AddrMode::kExt32Bit: {
      // Section 1.5.1, Table 1-7
      DEBUG_ASSERT(t0sz < 8);
      DEBUG_ASSERT(t1sz < 8);

      // TODO(johngro); this is true for S1 translation, figure out what it is
      // for S2 translation.
      constexpr uint64_t max_addr_bits = 32;
      constexpr uint64_t max_addr = (uint64_t{1} << max_addr_bits) - 1;

      if (t1sz == 0) {
        // Section 1.5.1, Figure 1-5
        // "Control of TTBR0 and TTBR1 regions, when SMMU_CBn_TCR.T1SZ is zero"
        ttbrs_[0].last_valid_addr = (uint64_t{1} << (32 - t0sz)) - 1;

        if (t0sz == 0) {
          if (ttbrs_[1].enabled) {
            dprintf(INFO, "WARNING - TTBR1 is unused but enabled in context bank %u\n", cb_ndx_);
          }
        } else {
          ttbrs_[1].first_valid_addr = ttbrs_[0].last_valid_addr + 1;
          ttbrs_[1].last_valid_addr = max_addr;
        }
      } else {
        // Section 1.5.1, Figure 1-6
        // "Control of TTBR0 and TTBR1 regions when SMMU_CBn_TCR.T1SZ is nonzero"
        ttbrs_[1].first_valid_addr = (uint64_t{1} << 32) - (uint64_t{1} << (32 - t1sz));
        ttbrs_[1].last_valid_addr = max_addr;

        if (t0sz == 0) {
          ttbrs_[0].last_valid_addr = ttbrs_[1].first_valid_addr - 1;
        } else {
          ttbrs_[0].last_valid_addr = (uint64_t{1} << (32 - t0sz)) - 1;
        }
      }
    } break;
    case AddrMode::k32Bit: {
      // Section 1.5.1, Table 1-6
      DEBUG_ASSERT(t0sz < 8);
      ttbrs_[0].last_valid_addr = (uint64_t{1} << (32 - t0sz)) - 1;

      if (t0sz) {
        ttbrs_[1].first_valid_addr = ttbrs_[0].last_valid_addr + 1;
        ttbrs_[1].last_valid_addr = 0xFFFF'FFFF;
      } else {
        // TTBR1 is not used and should be disabled, but go ahead and write
        // something to these values so that they have deterministic values,
        // even if they are technically undefined.
        ttbrs_[1].first_valid_addr = 0;
        ttbrs_[1].last_valid_addr = 0;
        if (ttbrs_[1].enabled) {
          dprintf(INFO, "WARNING - TTBR1 is unused but enabled in context bank %u\n", cb_ndx_);
        }
      }
    } break;
    default: {
      // The addressing mode is invalid.  If either TTBR is enabled, print a
      // warning, otherwise don't bother.  If neither TTBR is enabled, we are
      // not going to be performing any translations anyway.
      if (ttbrs_[0].enabled || ttbrs_[1].enabled) {
        dprintf(
            INFO,
            "WARNING - Bad addressing mode (%u) when decoding TTBR regions for context bank %u\n",
            static_cast<uint32_t>(addr_mode_), cb_ndx_);
      }
    } break;
  }
}

zx::result<> ContextBank::ValidateNdx(Smmu& smmu, uint32_t cb_ndx) {
  if (cb_ndx >= smmu.num_cbs()) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  if (!smmu.available_cbs_.TestBit(cb_ndx)) {
    return zx::error(ZX_ERR_ALREADY_BOUND);
  }

  return zx::ok();
}

void ContextBank::SetMode(SmmuBti& owner, BtiMode target_mode) {
  owner.AssertOwned(*this);

  // Upper levels should ensure that we never call this method for:
  //
  // 1) Adopted context banks.
  // 2) Context bank which have already shut down.
  // 3) To attempt to move from mode X -> mode X.
  // 4) To attempt to move from non-faulting mode -> a non-faulting mode.
  DEBUG_ASSERT(mode_ != BtiMode::kAdopted);
  DEBUG_ASSERT(mode_ != BtiMode::kShutdown);
  DEBUG_ASSERT(mode_ != target_mode);
  DEBUG_ASSERT(!(((mode_ == BtiMode::kTranslation) || (mode_ == BtiMode::kBypass)) &&
                 ((target_mode == BtiMode::kTranslation) || (target_mode == BtiMode::kBypass))));

  // Start by disabling ourselves at the register level.  During operation, we
  // expect to only ever see the following mode transitions.
  //
  // 1) Initial register state (unknown) -> Initial desired mode (fault/bypass/translate)
  // 2) Current mode -> Shutdown
  // 3) Non-faulting mode (bypass/translate) -> Fault mode
  // 4) Fault mode -> Non-faulting mode (bypass/translate)
  //
  // #1 and #2 cover the create/shutdown cases.  #3 and #4 cover the entering
  // into and exiting from quarantine states respectively.
  //
  // In no situation should we ever go from a non-faulting mode to another
  // non-faulting mode.  Because of this, it should always be safe to
  // first unconditionally disable all access at the register level.  This is
  // either already the case (because we are in a faulting mode), or it needs to
  // become the case (because we are about to switch to a faulting mode).
  //
  DisableRegs(gr1_base_, cb_base_, cb_ndx_);

  // If we have any page tables allocated, free them now.
  for (TTBRInfo& ttbr : ttbrs_) {
    if (ttbr.enabled) {
      DEBUG_ASSERT(false);  // TODO(johngro): walk and free pages here
      ttbr.enabled = false;
    }
  }

  // Make sure that we are configured for 64 bit addressing mode, whether or not
  // we are going to actually perform translation.
  gr1::CBA2R::Get(cb_ndx_)
      .ReadFrom(&gr1_base_)
      .set_VA64(1)
      .set_MONC(0)
      .set_VMID16(0)
      .WriteTo(&gr1_base_);
  addr_mode_ = AddrMode::k64Bit;

  // Set up a 48-bit address space with 4k pages, once again whether or not we
  // are going to actually perform translation.  Note that we can ask for this
  // configuration all that we want, but both the HW and the Hypervisor/Secure
  // Monitor might choose to override our decision.  When attempting to
  // determine the actual address space configuration, be sure to read back the
  // register values instead of assuming that they stuck.
  s1cbr::TCR_64Bit::Get()
      .ReadFrom(&cb_base_)
      .set_TG0(0)           // TTBR0 uses 4k pages
      .set_T0SZ(16)         // TTBR0 has a 48-bit address space.
      .set_TG1(2)           // _If_ we used TTBR1, we'd use 4k pages.
      .set_T1SZ(0x3f)       // Smallest we can make the TTBR1 aspace size is 2 bytes.
      .WriteTo(&cb_base_);  // We don't enable TTBR1, so T1SZ is unused anyway.

  // Finally, set up for our new mode of operation.
  switch (target_mode) {
    // In theory, we _should_ already be configured for faulting since
    // DisableRegs tried to put us into S1TS2Fault mode.  That said, systems
    // sometimes do not allow us to configure for this mode.  Either the HW or
    // the Hypervisor can block such requests.
    //
    // So, if we are not in S2Fault mode, do this instead.  Enable the MMU, but
    // leave page table walking for both of the TTBRs disabled, and configure
    // for S1TS2Bypass.  Basically, we are in translate mode, but page table
    // walking is disabled, so all accesses _should_ end up faulting, even
    // though the HW is in translation mode.  Said another way, Fault mode is
    // Translate mode but with no valid PTEs.
    case BtiMode::kFault: {
#ifdef DEBUG_ASSERT_IMPLEMENTED
      const s1cbr::TCR_64Bit tcr = s1cbr::TCR_64Bit::Get().ReadFrom(&cb_base_);
      DEBUG_ASSERT_MSG((tcr.EPD0() == 1) && (tcr.EPD1() == 1),
                       "Failed to disable page table walking in TCR (0x%08x) for Context Bank #%u",
                       tcr.reg_value(), cb_ndx_);

      const s1cbr::SCTLR sctlr = s1cbr::SCTLR::Get().ReadFrom(&cb_base_);
      DEBUG_ASSERT_MSG((sctlr.M() == 1),
                       "Failed to enable MMU in SCTLR (0x%08x) for Context Bank #%u",
                       sctlr.reg_value(), cb_ndx_);

      const gr1::CBAR cbar = gr1::CBAR::Get(cb_ndx_).ReadFrom(&gr1_base_);
      DEBUG_ASSERT_MSG((cbar.TYPE() == CBAR_Type::kS1TS2Bypass),
                       "Failed to configure for S1TS2Bypass CBAR (0x%08x) for Context Bank #%u",
                       cbar.reg_value(), cb_ndx_);
#endif
    } break;

    // We are shutting down, and will not become enabled again.  Our registers
    // now belong to our top level SMMU instance, so go ahead zero out the base
    // addresses here so that if anything in the ContextBank code attempts to
    // touch the registers, it will cause a fault. Our destructor is going to
    // check to confirm that our register addresses have been zeroed as an
    // indication that we have been shut down on the way out.
    case BtiMode::kShutdown:
      gr1_base_ = hwreg::RegisterMmio{0};
      cb_base_ = hwreg::RegisterMmio{0};
      break;

    // From a disabled state, entering either bypass or translate mode involves
    // setting our CBAR type to S1TS2Bypass.  The only difference is that
    // translation mode requires that the MMU be enabled first.  There is no
    // need (just now) to allocate any PTEs or enable any TTBRs.  That will
    // happen when we pin our first block of memory.
    case BtiMode::kTranslation:
      gr1::CBAR_S1TS2Bypass::Get(cb_ndx_)
          .FromValue(0)
          .set_TYPE(CBAR_Type::kS1TS2Bypass)  // Bypass mode
          .set_BPSHCFG(3)                     // Non-sharable
          //.set_IRPTNDX(0)                   // We only support SMMUv2, which has an interrupt
          .WriteTo(&gr1_base_);  // per context bank, and IRPTNDX is ignored.

      s1cbr::SCTLR::Get()
          .ReadFrom(&cb_base_)
          .set_M(1)  // MMU enabled => Translation enabled
          .WriteTo(&cb_base_);
      break;

    case BtiMode::kBypass:
      gr1::CBAR_S1TS2Bypass::Get(cb_ndx_)
          .FromValue(0)
          .set_TYPE(CBAR_Type::kS1TS2Bypass)  // Bypass mode
          .set_BPSHCFG(3)                     // Non-sharable
          //.set_IRPTNDX(0)                   // We only support SMMUv2, which has an interrupt
          .WriteTo(&gr1_base_);  // per context bank, and IRPTNDX is ignored.

      s1cbr::SCTLR::Get()
          .ReadFrom(&cb_base_)
          .set_M(0)  // MMU disabled => Translation disabled => passthru
          .WriteTo(&cb_base_);
      break;

    // We should never be in any of these cases
    case BtiMode::kAdopted:
      __FALLTHROUGH;
    case BtiMode::kInvalid:
      __FALLTHROUGH;
    default:
      DEBUG_ASSERT_MSG(false, "%s : Invalid target mode (%s) for context bank %u.\n",
                       owner.smmu().name(), BtiModeToString(target_mode), cb_ndx_);
      return;
  }

  mode_ = target_mode;
}

void ContextBank::DisableRegs(hwreg::RegisterMmio gr1_base, hwreg::RegisterMmio cb_base,
                              uint32_t cb_ndx) {
  // Make sure we are configured to use a 64 bit translation table.  This will
  // determine the format of the TCR, which we are about to configure.
  //
  // Why is it OK to potentially change the TCR format when it might be in use?
  // Generally speaking, it probably isn't.  That said, there are two cases to
  // consider here.  This code is running after initialization, or during
  // initialization.
  //
  // If this is happening after initialization, then we are already in VA64 mode
  // as this is what we always configure.  So, after-init, this is a no-op.
  //
  // During initialization, we follow a specific sequence for initializing the
  // hardware.  The first thing we will do is disable all of the SMRGs, meaning
  // that none of them refer to any context banks.  With no context banks in
  // use, we should be free to go through transient configuration states which
  // would otherwise be invalid.
  gr1::CBA2R::Get(cb_ndx).ReadFrom(&gr1_base).set_VA64(1).WriteTo(&gr1_base);

  // Unconditionally disable page table walking for both TTBRs.  If any
  // transaction makes it to this context bank, we want it to fail, and we
  // definitely do not want it  touching stale TTBRs.
  s1cbr::TCR_64Bit::Get().ReadFrom(&cb_base).set_EPD0(1).set_EPD1(1).WriteTo(&cb_base);
  arch::DeviceMemoryBarrier();

  // And explicitly _enable_ the MMU.  When the MMU is enabled, but both TTBRs
  // are disabled, translation is being demanded, but no translation table walks
  // can take place, guaranteeing a fault even if we failed to configure this CB
  // for S1TS2Fault.
  //
  // Make sure we set some other expected values while we are messing with the
  // SCLTR.
  //
  // - Translation tables (E=0) are expected to use little endian.
  // - Terminate transactions (CFCFG=0) when a fault occurs.
  // - Do not return an ABORT to the initiator during a fault (CFRE=0).  Simply
  //   implement RAZ/WI behavior.
  //
  // TODO(johngro): Research what the UCI bit _specifically_ controls, and
  // decide if we should be enabling it here, enabling it later, or leaving it
  // disabled at all times.
  //
  s1cbr::SCTLR::Get()
      .ReadFrom(&cb_base)
      .set_M(1)      // MMU Enabled
      .set_E(0)      // Little endian
      .set_UCI(0)    // Cache maintenance operations from EL0 are disabled.
      .set_HUPCF(1)  // Process all transaction independently, even with a pending fault interrupt.
      .set_CFIE(0)   // Disabled context fault interrupts by default.
      .set_CFCFG(0)  // Terminate transactions on fault
      .set_CFRE(0)   // Do not return an abort to transactions which fault.
      .WriteTo(&cb_base);

  // Configure for S1TS2Bypass.  Since we have disabled the TTBRs but enabled
  // the MMU, this should result in a Stage 1 fault for every transaction.
  gr1::CBAR::Get(cb_ndx).FromValue(0).set_TYPE(CBAR_Type::kS1TS2Bypass).WriteTo(&gr1_base);

  // Just for a bit of extra credit, zero out the TTBR address fields.
  s1cbr::TTBR0_64Bit::Get().ReadFrom(&cb_base).set_BaseAddress(0).set_ASID(0).WriteTo(&cb_base);
  s1cbr::TTBR1_64Bit::Get().ReadFrom(&cb_base).set_BaseAddress(0).set_ASID(0).WriteTo(&cb_base);

  // Invalidate all TLB entries for this context bank.
  s1cbr::TLBIALL::Get().FromValue(0).WriteTo(&cb_base);
  s1cbr::TLBSYNC::Get().FromValue(0).WriteTo(&cb_base);
  while (s1cbr::TLBSTATUS::Get().ReadFrom(&cb_base).SACTIVE() != 0) {
    arch::Yield();
  }
}

zx::result<> ContextBank::AdoptRegisterState(Smmu& smmu) {
  // We should only be adopting registers during initialization.
  DEBUG_ASSERT(mode_ == BtiMode::kInvalid);

  // Figure out whether or not this context bank is configured in a way which
  // matches one of our standard modes.
  //
  // Note that we are using the stage-1 translation definition of SCTLR here,
  // even though we have not determined yet if the CBAR has us configured for
  // stage-2-only translation.  This should be fine; S2.SCTLR is basically the
  // same as S1, just with a few bits removed.  We only really care about
  // SCTLR.M right now, which is the same between the two.
  gr1::CBAR cbar = gr1::CBAR::Get(cb_ndx_).ReadFrom(&gr1_base_);
  s1cbr::SCTLR sctlr = s1cbr::SCTLR::Get().ReadFrom(&cb_base_);

  // We don't understand either S2-only or S1->S2 translation.  Basically, we
  // don't understand much of anything about systems which support stage 2
  // translation, or operating as a virtual guest in such systems.  If we are
  // adopting register state which indicates either of these two modes, print a
  // warning.
  if ((cbar.TYPE() == CBAR_Type::kS1TS2Translate) || (cbar.TYPE() == CBAR_Type::kS2Translation)) {
    dprintf(INFO,
            "%s : WARNING - stage 2 translation enabled in adopted context bank %u (mode %s)\n",
            smmu.name(), cb_ndx_, ArmCbarTypeToString(cbar.TYPE()));
  }

  // Regardless of whether or not the MMU is enabled, determine what our
  // addressing mode _would_ be if enabled in order to keep our internal
  // bookkeeping consistent.  While the TCR format is technically different
  // between S1 and S2 context banks when in 32 bit addressing mode, the
  // differences don't actually matter to us here as register is in the same
  // location, and the EAE bit (which is all we care about) is in the same bit
  // position.
  if (gr1::CBA2R::Get(cb_ndx_).ReadFrom(&gr1_base_).VA64()) {
    addr_mode_ = AddrMode::k64Bit;
  } else {
    if (s1cbr::TCR::Get().ReadFrom(&cb_base_).EAE()) {
      addr_mode_ = AddrMode::kExt32Bit;
    } else {
      addr_mode_ = AddrMode::k32Bit;
    }
  }

  // If translation is enabled at all, we may need to print some warnings
  // depending on whether or not either of the TTBRs are enabled for
  // translation.  If they are, it implies that we have actual translation
  // tables somewhere in physical memory (at the location specified by the
  // TTBR), and those translation tables had better already be in memory which
  // was reserved, and both unavailable to the physical page allocator, as well
  // as to user mode (from the perspective of creating physical VMOs).
  if (sctlr.M()) {
    // Now figure out which of the two TTBRs (if any) are enabled.
    if (cbar.TYPE() == CBAR_Type::kS2Translation) {
      // Stage 2 translation has only one TTBR, and from what I can tell,
      // translation table walking cannot be turned off.  TTBR0 is always valid
      // for S2-only context banks.
      //
      // Fill out the cached TTBR info for the benefit of someone bringing up a
      // new platform who is interested in dumping the configuration using the
      // kernel console.
      ttbrs_[0].enabled = true;
      switch (addr_mode_) {
        case AddrMode::k64Bit: {
          const s2cbr::TCR_64Bit tcr = s2cbr::TCR_64Bit::Get().ReadFrom(&cb_base_);
          ttbrs_[0].granule_size_bits = DecodeGranuleSizeBits(tcr.TG0());
          DecodeTtbrRegions(tcr.T0SZ(), 0);
        } break;
        case AddrMode::kExt32Bit: {
          const s2cbr::TCR_Ext32Bit tcr = s2cbr::TCR_Ext32Bit::Get().ReadFrom(&cb_base_);
          ttbrs_[0].granule_size_bits = 12;  // Extended 32-bit always uses 4KB.
          DecodeTtbrRegions(tcr.T0SZ(), 0);
        } break;
        default: {
          dprintf(
              INFO,
              "%s : WARNING - bad addressing mode (%u) in adopted stage-2 only context bank (%u)\n",
              smmu.name(), static_cast<uint32_t>(addr_mode_), cb_ndx_);
        } break;
      }

      ttbrs_[0].ttbr_paddr = s2cbr::TTBR0::Get().ReadFrom(&cb_base_).BaseAddrValue();
    } else {
      // All other CBAR types may perform stage 1 translation, depending on the
      // SCTLR::M and TCD::EPD[01] bits.
      switch (addr_mode_) {
        case AddrMode::k64Bit: {
          const s1cbr::TCR_64Bit tcr = s1cbr::TCR_64Bit::Get().ReadFrom(&cb_base_);

          ttbrs_[0].enabled = !tcr.EPD0();
          ttbrs_[1].enabled = !tcr.EPD1();

          ttbrs_[0].granule_size_bits = DecodeGranuleSizeBits(tcr.TG0());
          ttbrs_[1].granule_size_bits = DecodeGranuleSizeBits(tcr.TG1());

          DecodeTtbrRegions(tcr.T0SZ(), tcr.T1SZ());

          ttbrs_[0].ttbr_paddr = s1cbr::TTBR0_64Bit::Get().ReadFrom(&cb_base_).BaseAddrValue();
          ttbrs_[1].ttbr_paddr = s1cbr::TTBR1_64Bit::Get().ReadFrom(&cb_base_).BaseAddrValue();
        } break;
        case AddrMode::kExt32Bit: {
          const s1cbr::TCR_Ext32Bit tcr = s1cbr::TCR_Ext32Bit::Get().ReadFrom(&cb_base_);

          ttbrs_[0].enabled = !tcr.EPD0();
          ttbrs_[1].enabled = !tcr.EPD1();

          ttbrs_[0].granule_size_bits = 12;  // Extended 32-bit always uses 4KB.
          ttbrs_[1].granule_size_bits = 12;

          DecodeTtbrRegions(tcr.T0SZ(), tcr.T1SZ());

          ttbrs_[0].ttbr_paddr = s1cbr::TTBR0_Ext32Bit::Get().ReadFrom(&cb_base_).BaseAddrValue();
          ttbrs_[1].ttbr_paddr = s1cbr::TTBR1_Ext32Bit::Get().ReadFrom(&cb_base_).BaseAddrValue();
        } break;
        case AddrMode::k32Bit: {
          const s1cbr::TCR_32Bit tcr = s1cbr::TCR_32Bit::Get().ReadFrom(&cb_base_);

          ttbrs_[0].enabled = !tcr.PD0();
          ttbrs_[1].enabled = !tcr.PD1();

          ttbrs_[0].granule_size_bits = 12;  // 32-bit always uses 4KB.
          ttbrs_[1].granule_size_bits = 12;

          DecodeTtbrRegions(tcr.T0SZ(), 0);

          ttbrs_[0].ttbr_paddr = s1cbr::TTBR0_32Bit::Get().ReadFrom(&cb_base_).BaseAddrValue();
          ttbrs_[1].ttbr_paddr = s1cbr::TTBR1_32Bit::Get().ReadFrom(&cb_base_).BaseAddrValue();
        } break;
        default: {
          dprintf(INFO,
                  "%s : WARNING - bad addressing mode (%u) in adopted stage-1 context bank (%u)\n",
                  smmu.name(), static_cast<uint32_t>(addr_mode_), cb_ndx_);
        } break;
      }
    }

    for (uint32_t i = 0; i < ttbrs_.size(); ++i) {
      if (ttbrs_[i].enabled) {
        dprintf(
            INFO,
            "%s : WARNING - Translation enabled and TTBR%u valid in adopted context bank (%u).\n",
            smmu.name(), i, cb_ndx_);
        dprintf(
            INFO,
            "%s : WARNING - Make sure that PTE pages starting at TTBR%u paddr 0x%lx are reserved.\n",
            smmu.name(), i, ttbrs_[i].ttbr_paddr);
      }
    }
  }

  mode_ = BtiMode::kAdopted;
  return zx::ok();
}

}  // namespace arm_smmu
