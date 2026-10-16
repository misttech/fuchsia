// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/boot-options/boot-options.h>
#include <lib/memalloc/range.h>
#include <lib/zbi-format/board.h>
#include <lib/zbi-format/driver-config.h>
#include <lib/zbi-format/reboot.h>
#include <lib/zbi-format/zbi.h>
#include <lib/zbitl/items/cpu-topology.h>
#include <lib/zbitl/view.h>
#include <stdio.h>
#include <zircon/assert.h>

#include <efi/types.h>
#include <fbl/algorithm.h>
#include <ktl/algorithm.h>
#include <ktl/byte.h>
#include <ktl/iterator.h>
#include <ktl/limits.h>
#include <ktl/span.h>
#include <phys/address-space.h>
#include <phys/allocation.h>
#include <phys/arch/arch-handoff.h>
#include <phys/boot-constants.h>
#include <phys/handoff.h>
#include <phys/main.h>

#include "handoff-entropy.h"
#include "handoff-prep.h"

#include <ktl/enforce.h>

void HandoffPrep::SummarizeMiscZbiItems(ktl::span<ktl::byte> zbi) {
  size_t zbi_size = zbi.size_bytes();
  handoff_->zbi =
      MakePhysVmo({zbi.data(), (zbi_size + AddressSpace::kPageSize - 1) & -AddressSpace::kPageSize},
                  "zbi"sv, zbi_size);

  fbl::AllocChecker ac;
  EntropyHandoff entropy;

  zbitl::View view(zbi);
  for (auto it = view.begin(); it != view.end(); ++it) {
    auto [header, payload] = *it;
    switch (header->type) {
      case ZBI_TYPE_HW_REBOOT_REASON:
        ZX_ASSERT(payload.size() >= sizeof(zbi_hw_reboot_reason_t));
        boot_constants_->hw_reboot_reason =
            *reinterpret_cast<const zbi_hw_reboot_reason_t*>(payload.data());
        break;

      case ZBI_TYPE_PLATFORM_ID:
        ZX_ASSERT(payload.size() >= sizeof(zbi_platform_id_t));
        handoff_->platform_id = *reinterpret_cast<const zbi_platform_id_t*>(payload.data());
        break;

      case ZBI_TYPE_CPU_TOPOLOGY:
      case ZBI_TYPE_DEPRECATED_CPU_TOPOLOGY_V1:
      case ZBI_TYPE_DEPRECATED_CPU_TOPOLOGY_V2:
        // Normalize either item type into zbi_topology_node_t[] for handoff.
        if (auto table = zbitl::CpuTopologyTable::FromPayload(header->type, payload);
            table.is_ok()) {
          ktl::span handoff_table = New(handoff()->cpu_topology, ac, table->size());
          ZX_ASSERT_MSG(ac.check(), "cannot allocate %zu bytes for CPU topology handoff",
                        table->size_bytes());
          ZX_DEBUG_ASSERT(handoff_table.size() == table->size());
          ktl::copy(table->begin(), table->end(), handoff_table.begin());
        } else {
          printf("%s: NOTE: ignored invalid CPU topology payload: %.*s\n", ProgramName(),
                 static_cast<int>(table.error_value().size()), table.error_value().data());
        }
        break;

      case ZBI_TYPE_CRASHLOG:
        if (!payload.empty()) {
          ktl::span buffer = New(handoff_->crashlog, ac, payload.size());
          ZX_ASSERT_MSG(ac.check(), "cannot allocate %zu bytes for crash log", payload.size());
          memcpy(buffer.data(), payload.data(), payload.size_bytes());
          // The crashlog is propagated separately by the kernel.
        }
        break;

      case ZBI_TYPE_SECURE_ENTROPY:
        entropy.AddEntropy(it->payload);
        ZX_ASSERT(it.view().EditHeader(it, {.type = ZBI_TYPE_DISCARD}).is_ok());
        ZX_ASSERT(it->header->type == ZBI_TYPE_DISCARD);
#if ZX_DEBUG_ASSERT_IMPLEMENTED
        // Verify that the payload contents have been zeroed.
        for (auto b : it->payload) {
          ZX_DEBUG_ASSERT(static_cast<char>(b) == 0);
        }
#endif
        break;
      case ZBI_TYPE_ACPI_RSDP:
        ZX_ASSERT(payload.size() >= sizeof(uint64_t));
        handoff()->acpi_rsdp = *reinterpret_cast<const uint64_t*>(payload.data());
        break;
      case ZBI_TYPE_SMBIOS:
        ZX_ASSERT(payload.size() >= sizeof(uint64_t));
        handoff()->smbios_phys = *reinterpret_cast<const uint64_t*>(payload.data());
        break;
      case ZBI_TYPE_EFI_SYSTEM_TABLE:
        ZX_ASSERT(payload.size() >= sizeof(uint64_t));
        handoff()->efi_system_table = *reinterpret_cast<const uint64_t*>(payload.data());
        break;

      case ZBI_TYPE_EFI_MEMORY_ATTRIBUTES_TABLE: {
        ktl::span handoff_table = New(handoff()->efi_memory_attributes, ac, payload.size());
        ZX_ASSERT_MSG(ac.check(), "Cannot allocate %zu bytes for EFI memory attributes",
                      payload.size());
        ktl::copy(payload.begin(), payload.end(), handoff_table.begin());
        break;
      }

      // Default assumption is that the type is architecture-specific.
      default:
        ArchSummarizeMiscZbiItem(*header, payload);
        break;
    }
  }

  // Clears the contents of 'entropy_mixin' when consumed for security reasons.
  entropy.AddEntropy(*const_cast<BootOptions*>(BootOptions::Get()));

  // Depending on certain boot options, failure to meet entropy requirements may cause
  // the program to abort after this point.
  handoff_->entropy_pool = ktl::move(entropy).Take(*BootOptions::Get());

  // At this point we should have full confidence that the ZBI is properly
  // formatted.
  ZX_ASSERT(view.take_error().is_ok());
}
