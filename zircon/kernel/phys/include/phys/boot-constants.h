// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_BOOT_CONSTANTS_H_
#define ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_BOOT_CONSTANTS_H_

#include <lib/zbi-format/reboot.h>
#include <stdint.h>

#include <ktl/type_traits.h>

#include "handoff-ptr.h"

// This file provides what are constants as far as the kernel is concerned.
// But physboot fills them in as variables as boot time.  These constants are
// found directly in RODATA by kernel code without indirection and without
// anything else that can be clobbered.  To keep the ZirconAbiSpec pointers to
// a minimum, unrelated things are all gathered into this one struct.  This
// header lets them all be found directly with a cheap const variable access.
//
// Things should be placed here only when they should be permanently accessible
// as read-only global variables for the lifetime of the kernel proper after
// boot.  Things that are only needed temporarily at boot to initialize other
// kernel state are instead passed in PhysHandoff (see <phys/handoff.h>), which
// is discarded after boot time.
//
// Since this header thus could be used by disparate parts of the kernel code,
// its header dependencies should be kept to a minimum.

struct BootConstants {
  // The physical address at which the kernel was loaded.
  // The virtual address __executable_start / __ehdr_start is mapped to this.
  uintptr_t kernel_physical_load_address = 0;

  // ZBI_TYPE_HW_REBOOT_REASON payload (or as initialized if no ZBI item).
  zbi_hw_reboot_reason_t hw_reboot_reason = ZBI_HW_REBOOT_REASON_UNDEFINED;

  // This indicates the kernel.bypass-debuglog option or its compile-time
  // override in kZirconAbiSpec.always_bypass_debuglog.  In the kernel proper,
  // only this is consulted, not BootOptions::bypass_debuglog.  bypass_debuglog
  // will cause the kernel proper's printfs to write directly to the console.
  // It also has the side effect of disabling uart Tx interrupts, which causes
  // all of the serial writes to be polling.
  bool bypass_debuglog = false;

  // ZBI container of items to be propagated in mexec.
  PhysHandoffPermanentSpan<const std::byte> mexec_data;
};
static_assert(ktl::is_trivially_destructible_v<BootConstants>);
static_assert(ktl::is_standard_layout_v<BootConstants>);

// The compiler just knows this will be provided at link time, so there is no
// way to optimize out (first) actual reads from the memory physboot wrote.
extern "C" constinit const BootConstants kBootConstants;

#endif  // ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_BOOT_CONSTANTS_H_
