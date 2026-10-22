// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/zbitl/error-stdio.h>
#include <lib/zbitl/view.h>
#include <stdio.h>

#include <cstdint>

#include <ktl/array.h>
#include <ktl/byte.h>
#include <ktl/span.h>
#include <ktl/string_view.h>
#include <phys/address-space.h>
#include <phys/elf-image.h>
#include <phys/kernel-package.h>
#include <phys/symbolize.h>

#include "../test-main.h"
#include "get-int.h"

#ifdef __aarch64__
#include <lib/arch/arm64/system.h>
#endif

#include <ktl/enforce.h>

namespace {

constexpr const char* kTestName = "virtual-address-loading-test";

// The name of ELF module to be loaded.
constexpr ktl::string_view kGetInt = "get-int.virtual-address-loading-test";

// The BOOTFS namespace under which kGetInt lives.
constexpr ktl::string_view kNamespace = "virtual-address-loading-test-data";

// TODO(https://fxbug.dev/42172722): Pick a load address through a sort of allocator
// (i.e., as we would in production).
constexpr uint64_t kLoadAddress = 0xffff'ffff'0000'0000;

}  // namespace

int TestMain(void* zbi_ptr, ktl::optional<EarlyBootZbi> early_zbi, arch::EarlyTicks) {
  MainSymbolize symbolize(kTestName);

#ifdef __aarch64__
  // There is no upper address space in EL2+ by default, so drop down to EL1
  // before setting up the address space, as physboot does.
  arch::ArmDropToEl1();
#endif

  // Initialize memory for allocation/free.
  AddressSpace aspace;
  InitMemory(zbi_ptr, ktl::move(early_zbi), &aspace);

  zbitl::View zbi(
      zbitl::StorageFromRawHeader<ktl::span<ktl::byte>>(static_cast<zbi_header_t*>(zbi_ptr)));
  KernelStorage kernelfs;
  kernelfs.Init(zbi);

  KernelStorage::Bootfs bootfs;
  if (auto result = kernelfs.root().subdir(kNamespace); result.is_error()) {
    zbitl::PrintBootfsError(result.error_value());
    return 1;
  } else {
    bootfs = ktl::move(result).value();
  }

  ktl::array<const ElfImage*, 2> modules;
  symbolize.EnableModuleLoading(Symbolize::ModuleList(ktl::span(modules)));

  printf("Loading %.*s...\n", static_cast<int>(kGetInt.size()), kGetInt.data());
  ElfImage elf;
  if (auto result = elf.Init(bootfs, kGetInt, true); result.is_error()) {
    zbitl::PrintBootfsError(result.error_value());
    return 1;
  }

  ZX_ASSERT(!elf.has_patches());

  // The GN target for get-int uses kernel_elf_interp() on this test binary.
  printf("Verifying PT_INTERP matches test build ID...\n");
  elf.AssertInterpMatchesBuildId(kGetInt, symbolize.build_id());

  Allocation loaded = elf.Load(memalloc::Type::kPhysElf, kLoadAddress);
  elf.Relocate();

  // TODO(https://fxbug.dev/42172722): Set up C++ ABI support and map and jump into a
  // loaded C++ program.
  if (auto result = elf.MapInto(aspace); result.is_error()) {
    printf("Failed to map loaded image\n");
    return 1;
  }

  auto entry = reinterpret_cast<decltype(GetInt)*>(elf.entry() + elf.load_bias());
  printf("Calling virtual entry point %p...\n", entry);

  // We should now be able to access GetInt()!
  constexpr int kExpected = 42;
  if (int actual = entry(); actual != kExpected) {
    printf("FAILED: Expected %d; got %d\n", kExpected, actual);
    return 1;
  }

  return 0;
}
