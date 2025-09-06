// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "phys/address-space.h"

#include <lib/arch/cache.h>
#include <lib/arch/riscv64/page-table.h>
#include <lib/boot-options/boot-options.h>

void ArchSetUpAddressSpace(AddressSpace& aspace) {
  if (gBootOptions && !gBootOptions->riscv64_phys_mmu) {
    return;
  }
  aspace.Init();
  aspace.SetUpIdentityMappings();
  aspace.Install();
}

// The MMU will be off when the trampoline runs, so there is nothing to do.
void ArchPrepareAddressSpaceForTrampoline() {}

void AddressSpace::ArchInstall() const {
  arch::RiscvSatp::Get()
      .FromValue(0)
      .set_mode(LowerPaging::kMode)
      .set_root_address(root_paddr())
      .set_asid(0)
      .Write();
  arch::InvalidateLocalTlbs();  // Acts as a barrier as well.
}
