// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "platform/pc/acpi.h"

#include <align.h>
#include <lib/acpi_lite.h>
#include <lib/acpi_lite/structures.h>
#include <lib/acpi_lite/zircon.h>
#include <lib/console.h>
#include <lib/fit/defer.h>
#include <lib/lazy_init/lazy_init.h>
#include <lib/zx/result.h>
#include <trace.h>
#include <zircon/types.h>

#include <arch/x86/acpi.h>
#include <arch/x86/bootstrap16.h>
#include <kernel/percpu.h>
#include <ktl/optional.h>
#include <lk/init.h>
#include <phys/handoff.h>

#include <ktl/enforce.h>

extern "C" {
zx_status_t rust_acpi_parser_init(zx_paddr_t rsdp_pa, void* out_parser);
void rust_acpi_parser_dump_tables(const void* parser);
size_t rust_acpi_parser_num_tables(const void* parser);
const acpi_lite::AcpiSdtHeader* rust_acpi_parser_get_table_at_index(const void* parser,
                                                                    size_t index);
const void* cpp_global_acpi_parser_state();
}

namespace {

// System-wide ACPI parser.
AcpiLiteParser* global_acpi_parser;

lazy_init::LazyInit<AcpiLiteParser, lazy_init::CheckType::None, lazy_init::Destructor::Disabled>
    g_parser;

int ConsoleAcpiDump(int argc, const cmd_args* argv, uint32_t flags) {
  if (global_acpi_parser == nullptr) {
    printf("ACPI not initialized.\n");
    return 1;
  }

  global_acpi_parser->DumpTables();
  return 0;
}

}  // namespace

AcpiLiteParser& GlobalAcpiLiteParser() {
  ASSERT_MSG(global_acpi_parser != nullptr, "PlatformInitAcpi() not called.");
  return *global_acpi_parser;
}

extern "C" const void* cpp_global_acpi_parser_state() {
  assert(global_acpi_parser);
  return &global_acpi_parser->state();
}

void PlatformInitAcpi(zx_paddr_t acpi_rsdp) {
  ASSERT(global_acpi_parser == nullptr);

  // Create AcpiParser.
  RustAcpiLite parser;
  zx_status_t status = rust_acpi_parser_init(acpi_rsdp, &parser);
  if (status != ZX_OK) {
    panic("Could not initialize ACPI. Error code: %d.", status);
  }

  g_parser.Initialize(parser);
  global_acpi_parser = &g_parser.Get();
}

static void platform_init_acpi(uint level) {
  PlatformInitAcpi(gPhysHandoff->acpi_rsdp.value_or(0));
}

void AcpiLiteParser::DumpTables() { rust_acpi_parser_dump_tables(&state_); }

size_t AcpiLiteParser::num_tables() const { return rust_acpi_parser_num_tables(&state_); }

const acpi_lite::AcpiSdtHeader* AcpiLiteParser::GetTableAtIndex(size_t index) const {
  return rust_acpi_parser_get_table_at_index(&state_, index);
}

LK_INIT_HOOK(platform_init_acpi, platform_init_acpi, LK_INIT_LEVEL_VM)

STATIC_COMMAND_START
STATIC_COMMAND("acpidump", "dump ACPI tables to console", &ConsoleAcpiDump)
STATIC_COMMAND_END(acpi)
