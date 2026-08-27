// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <stdint.h>

#include <kernel/ffi.h>
#include <phys/handoff.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_physhandoff_get_smbios_phys(uint64_t* smbios_phys);
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_physhandoff_get_acpi_rsdp(uint64_t* acpi_rsdp);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_physhandoff_get_smbios_phys(uint64_t* smbios_phys) {
  if (gPhysHandoff && gPhysHandoff->smbios_phys) {
    *smbios_phys = *gPhysHandoff->smbios_phys.to_std();
    return true;
  }
  return false;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_physhandoff_get_acpi_rsdp(uint64_t* acpi_rsdp) {
  if (gPhysHandoff && gPhysHandoff->acpi_rsdp) {
    *acpi_rsdp = *gPhysHandoff->acpi_rsdp.to_std();
    return true;
  }
  return false;
}

}  // extern "C"
