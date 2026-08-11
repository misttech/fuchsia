// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <assert.h>
#include <zircon/types.h>

#include <arch/x86/feature.h>
#include <arch/x86/platform_access.h>
#include <arch/x86/pv.h>
#include <dev/interrupt.h>
#include <object/resource_dispatcher.h>

void shutdown_interrupts_curr_cpu(void) {
  if (x86_hypervisor_has_pv_eoi()) {
    MsrAccess msr;
    PvEoi::get()->Disable(&msr);
  }

  // TODO(maniscalco): Walk interrupt redirection entries and make sure nothing targets this CPU.
}
