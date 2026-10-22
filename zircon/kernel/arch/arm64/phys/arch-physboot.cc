// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/arm64/system.h>

#include "physboot.h"

void ArchPhysbootBeforeInitMemory() {
  // Ensure we configure EL2 (staying in EL2 with VHE if supported, or dropping
  // to EL1 otherwise) first so that we set up our address space there and can
  // hand that off to the kernel proper without reconstruction.
  arch::ArmDropToEl1();
}
