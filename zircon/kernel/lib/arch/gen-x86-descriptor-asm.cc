// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/x86/descriptor.h>

#include <hwreg/asm.h>

// Generate the <lib/arch/x86/descriptor-asm.h> header that assembly code uses.
// It provides the raw 64-bit values of standard segment descriptors so a GDT
// can be defined in assembly with `.quad` without magic numbers.

namespace {

constexpr uint64_t RawDesc32(const arch::Desc32& desc) {
  return static_cast<uint64_t>(desc.limit_base_lo16) |
         (static_cast<uint64_t>(desc.flags_base_hi16) << 32u);
}

// Flat 32-bit code segment: base 0, limit 4GiB, readable, executable.
constexpr uint64_t kFlatCode32 =
    RawDesc32(arch::Desc32{}.MakeFlat().set_type(arch::Desc32::CODE_RX));

// Flat 32-bit data segment: base 0, limit 4GiB, readable, writable.
constexpr uint64_t kFlatData32 =
    RawDesc32(arch::Desc32{}.MakeFlat().set_type(arch::Desc32::DATA_RW));

}  // namespace

int main(int argc, char** argv) {
  return hwreg::AsmHeader()
      .Macro("X86_DESC32_FLAT_CODE32", kFlatCode32)
      .Macro("X86_DESC32_FLAT_DATA32", kFlatData32)
      .Main(argc, argv);
}
