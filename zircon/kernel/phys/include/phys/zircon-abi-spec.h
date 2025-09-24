// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_ZIRCON_ABI_SPEC_H_
#define ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_ZIRCON_ABI_SPEC_H_

#include <stddef.h>
#include <stdint.h>
#include <zircon/assert.h>

#include <ktl/type_traits.h>

struct PhysHandoff;  // <phys/handoff.h>

// This is the entry point function for the ELF kernel.  This symbol is not
// used directly as the ELF entry point (Ehdr::e_entry).  Instead it's found
// via the ZirconAbiSpec data structure (below), which is where e_entry points.
extern "C" [[noreturn, clang::cfi_unchecked_callee]] void PhysbootHandoff(PhysHandoff* handoff);

// The kernel ABI specifications needed at the phys stage to properly prepare
// handoff.  The contents are initialized in the definition of kZirconAbiSpec;
// physboot reads that directly after loading and relocating the kernel image.
// To make it simple to find
struct ZirconAbiSpec {
  struct Stack {
    template <size_t PageSize>
    constexpr void AssertValid() const {
      ZX_ASSERT(size_bytes == (size_bytes & -PageSize));
      ZX_ASSERT(lower_guard_size_bytes == (lower_guard_size_bytes & -PageSize));
      ZX_ASSERT(upper_guard_size_bytes == (upper_guard_size_bytes & -PageSize));
    }

    // The size of the stack. Must be page-aligned.
    uint32_t size_bytes = 0;

    // The size of the unmapped 'guard' region to ensure lies below the mapped
    // stack. Must be page-aligned.
    uint32_t lower_guard_size_bytes = 0;

    // The size of the unmapped 'guard' region to ensure lies above the mapped
    // stack. Must be page-aligned.
    uint32_t upper_guard_size_bytes = 0;
  };

  template <size_t PageSize>
  constexpr void AssertValid() const {
    ZX_ASSERT(magic == kMagic);
    machine_stack.AssertValid<PageSize>();
    shadow_call_stack.AssertValid<PageSize>();
    unsafe_stack.AssertValid<PageSize>();
    ZX_ASSERT(entry);
  }

  // This never changes and is just checked by assertions.
  static constexpr uint64_t kMagic = 0xfeed'f00d'bad'4'face;
  const uint64_t magic = kMagic;

  // These instruct physboot what kinds of stack to set up for the kernel.
  Stack machine_stack;
  Stack shadow_call_stack;
  Stack unsafe_stack;

  // This instructs physboot where to enter the kernel.  The kernel's first PC
  // expects the ABI of the normal C++ function signature of PhysbootHandoff,
  // but the call is made at the top of the stack with a zero return address,
  // and the function must not return.  The member is declared const so that
  // its default initializer still applies with designated initializers.
  decltype(PhysbootHandoff)* const entry = PhysbootHandoff;
};
static_assert(ktl::is_trivially_destructible_v<ZirconAbiSpec>);

// This is defined in RODATA (or RELRO) by the kernel as the linke-time e_entry
// address; physboot finds it in the image after loading and relocation.
extern "C" constinit const ZirconAbiSpec kZirconAbiSpec;

#endif  // ZIRCON_KERNEL_PHYS_INCLUDE_PHYS_ZIRCON_ABI_SPEC_H_
