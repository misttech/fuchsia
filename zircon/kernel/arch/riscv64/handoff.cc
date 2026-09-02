// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <stdint.h>

#include <kernel/ffi.h>
#include <phys/arch/arch-handoff.h>

// Accessors for the fields of `ArchPhysHandoff` that the Rust arch code needs.
//
// The struct itself cannot be described in Rust: it has `std::optional` members,
// and the layout of `arch::RiscvFeatures` is an implementation detail of
// `<lib/arch/riscv64/feature.h>`.  These are all read exactly once at boot.

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language
// inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_riscv64_handoff_boot_hart_id(const ArchPhysHandoff* handoff) {
  return handoff->boot_hart_id;
}

}  // extern "C"
