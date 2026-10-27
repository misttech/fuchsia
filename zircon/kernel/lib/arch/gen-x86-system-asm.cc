// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/x86/feature.h>
#include <lib/arch/x86/paging-traits.h>
#include <lib/arch/x86/system.h>

#include <hwreg/asm.h>

// Generate the <lib/arch/x86/system-asm.h> header that assembly code uses.
// It provides bit masks for the main x86 system registers, as well as the
// non-address bits of the few kinds of page table entries that static,
// assembly-defined page tables need, computed with the same code the C++ page
// table code uses.

namespace {

constexpr arch::AccessPermissions kSupervisorRwx = {
    .readable = true,
    .writable = true,
    .executable = true,
};

// A present, non-terminal PML4 entry: the address bits are added at runtime.
uint64_t Pml4Entry() {
  return arch::X86PagingStructure<arch::X86PagingLevel::kPml4Table>{}
      .Set({}, {.present = true, .terminal = false, .access = kSupervisorRwx})
      .reg_value();
}

// A present, terminal (1GiB page) PDPT entry mapping address 0.
uint64_t Pdpte1GiB() {
  return arch::X86PagingStructure<arch::X86PagingLevel::kPageDirectoryPointerTable>{}
      .Set({}, {.present = true, .terminal = true, .access = kSupervisorRwx})
      .reg_value();
}

}  // namespace

int main(int argc, char** argv) {
  return hwreg::AsmHeader()
      .Register<arch::X86Cr0>("X86_CR0_")
      .Register<arch::X86Cr4>("X86_CR4_")
      .Register<arch::X86ExtendedFeatureEnableRegisterMsr>("X86_EFER_")
      .Macro("X86_PAGING_PML4E_RWX", Pml4Entry())
      .Macro("X86_PAGING_PDPTE_1GIB_RWX", Pdpte1GiB())
      .Main(argc, argv);
}
