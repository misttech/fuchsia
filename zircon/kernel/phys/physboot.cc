// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "physboot.h"

#include <inttypes.h>
#include <lib/boot-options/boot-options.h>
#include <lib/code-patching/code-patches.h>
#include <lib/fit/function.h>
#include <lib/zbitl/error-stdio.h>
#include <lib/zbitl/view.h>
#include <stdlib.h>
#include <zircon/assert.h>

#include <arch/code-patches/case-id.h>
#include <arch/kernel_aspace.h>
#include <ktl/array.h>
#include <ktl/byte.h>
#include <ktl/initializer_list.h>
#include <ktl/span.h>
#include <ktl/string_view.h>
#include <ktl/utility.h>
#include <phys/address-space.h>
#include <phys/allocation.h>
#include <phys/boot-zbi.h>
#include <phys/elf-image.h>
#include <phys/handoff/handoff.h>
#include <phys/kernel-package.h>
#include <phys/main.h>
#include <phys/stdio.h>
#include <phys/symbolize.h>
#include <phys/uart-console.h>

#include "handoff-prep.h"
#include "log.h"

#include <ktl/enforce.h>

PhysBootTimes gBootTimes;

namespace {

// physboot itself and the kernel.
constexpr size_t kMaxModules = 2;

constexpr ktl::string_view kElfPhysKernel = "vmzircon";

void LogSerial(FILE* out = stdout) {
  fprintf(out, "%s: Console configured as ", ProgramName());
  BootOptions::Get()->Show("kernel.serial"sv, false, out);
}

void PatchElfKernel(ElfImage& kernel, const ArchPatchInfo& patch_info) {
  auto apply_patch = [&patch_info](
                         code_patching::Patcher& patcher, CodePatchId id,
                         ktl::span<ktl::byte> code_to_patch,
                         ElfImage::PrintPatchFunction print) -> fit::result<ElfImage::Error> {
    if (ArchPatchCode(patcher, patch_info, code_to_patch, id, ktl::move(print))) {
      return fit::ok();
    }
    print({"unrecognized patch case ID"});
    ZX_PANIC("%s: code-patching: unrecognized patch case ID %" PRIu32, gSymbolize->name(),
             static_cast<uint32_t>(id));
  };

  debugf("%s: Applying %zu patches...\n", gSymbolize->name(), kernel.patch_count());
  // Apply patches to the kernel image.
  auto result = kernel.ForEachPatch<CodePatchId>(apply_patch);
  if (result.is_error()) {
    zbitl::PrintBootfsError(result.error_value());
    abort();
  }

  // There's always the self-test patch, so there should never be none.
  ZX_ASSERT(kernel.has_patches());
}

void RelocateElfKernel(ElfImage& kernel) {
  debugf("%s: Relocating ELF kernel to [%#" PRIx64 ", %#" PRIx64 ")...\n", gSymbolize->name(),
         kernel.load_address(), kernel.load_address() + kernel.vaddr_size());
  kernel.Relocate();
}

[[noreturn]] void BootZircon(UartDriver& uart, KernelStorage kernel_storage) {
  KernelStorage::Bootfs package = kernel_storage.GetKernelPackage();

  ElfImage kernel;
  debugf("%s: Locating ELF kernel in kernel package...\n", gSymbolize->name());
  if (auto result = kernel.Init(package, kElfPhysKernel, true); result.is_error()) {
    printf("%s: Cannot load ELF kernel \"%.*s/%.*s\" from STORAGE_KERNEL item BOOTFS: ",
           gSymbolize->name(), static_cast<int>(package.directory().size()),
           package.directory().data(), static_cast<int>(kElfPhysKernel.size()),
           kElfPhysKernel.data());
    zbitl::PrintBootfsError(result.error_value());
    abort();
  }

  // Make sure the kernel was built to match this physboot binary.
  kernel.AssertInterpMatchesBuildId(gSymbolize->name(), gSymbolize->build_id());

  if (kernel.machine() != elfldltl::ElfMachine::kNative) [[unlikely]] {
    ZX_PANIC("ELF kernel e_machine %#" PRIx16 " != expected %#" PRIx16 "\n",
             static_cast<uint16_t>(kernel.machine()),
             static_cast<uint16_t>(elfldltl::ElfMachine::kNative));
  }

  // Use the putative eventual virtual address to relocate the kernel.
  Allocation loaded_kernel = kernel.Load(memalloc::Type::kKernel, kHandoffVirtualAddress);

  const ArchPatchInfo patch_info = ArchPreparePatchInfo();
  PatchElfKernel(kernel, patch_info);

  RelocateElfKernel(kernel);

  // When verbose, also log the kernel as being another module with the same
  // segments at their physical addresses too so code that runs at the physical
  // address (e.g. secondary CPU startup) can also be symbolized.
  gSymbolize->ModuleContext(kernel, static_cast<unsigned int>(gSymbolize->modules().size()), true);

  if (kernel.memory_image().size_bytes() > KERNEL_IMAGE_MAX_SIZE) {
    ZX_PANIC(
        "%s: Attempting to load kernel of size %#zx. Max supported kernel size is %#zx (\"KERNEL_IMAGE_MAX_SIZE\").\n",
        gSymbolize->name(), kernel.memory_image().size_bytes(),
        static_cast<size_t>(KERNEL_IMAGE_MAX_SIZE));
  }

  // Prepare the handoff data structures.
  HandoffPrep prep(ktl::move(kernel));

  if (BootOptions::Get()->phys_verbose) {
    Allocation::GetPool().PrintMemoryRanges(gSymbolize->name());
  }

  prep.DoHandoff(uart, kernel_storage.zbi().storage(), package, patch_info);
}

}  // namespace

[[noreturn]] void ZbiMain(void* zbi_ptr, EarlyBootZbi zbi, arch::EarlyTicks ticks) {
  gBootTimes.Set(PhysBootTimes::kZbiEntry, ticks);

  // Symbolize uses its output for the panic path.  So that should not be
  // redirected to the log code path.  Instead keep it pointed at a copy of the
  // current stdout, which is just the console.
  FILE console = *stdout;
  Symbolize symbolize("physboot", &console);
  if (BootOptions::Get()->phys_verbose) {
    symbolize.Context();
    LogSerial();
  }

  ArchPhysbootBeforeInitMemory();

  AddressSpace aspace;
  InitMemory(zbi_ptr, ktl::move(zbi), &aspace);

  // This marks the interval between handoff from the boot loader (kZbiEntry)
  // and phys environment setup with identity-mapped memory management et al.
  gBootTimes.SampleNow(PhysBootTimes::kPhysSetup);

  // Start collecting the log in memory as well as logging to the console.
  Log log;

  {
    // Prime the log with what would already have been written to the console
    // under kernel.phys.verbose=true (even if it wasn't), but don't send that
    // to the console.
    FILE log_file{&log};
    symbolize.ContextAlways(&log_file);
    LogSerial(&log_file);
    Allocation::GetPool().PrintMemoryRanges(symbolize.name(), &log_file);
  }

  // Now mirror all stdout to the log, and write debugf there even if verbose
  // logging to stdout is disabled.
  gLog = &log;
  log.SetStdout();

  auto zbi_storage =
      zbitl::StorageFromRawHeader<ktl::span<ktl::byte>>(reinterpret_cast<zbi_header_t*>(zbi_ptr));

  // Unpack the compressed KERNEL_STORAGE payload.
  KernelStorage kernel_storage;
  kernel_storage.Init(zbitl::View{zbi_storage});
  kernel_storage.GetTimes(gBootTimes);

  // Provide space for loading modules.
  ktl::array<const ElfImage*, kMaxModules> modules_storage;
  symbolize.EnableModuleLoading(Symbolize::ModuleList(modules_storage));

  BootZircon(GetUartDriver(), ktl::move(kernel_storage));
}
