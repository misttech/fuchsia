// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_ARM64_CODE_PATCHES_INCLUDE_ARCH_CODE_PATCHES_CASE_ID_H_
#define ZIRCON_KERNEL_ARCH_ARM64_CODE_PATCHES_INCLUDE_ARCH_CODE_PATCHES_CASE_ID_H_

#include <stdint.h>

// Defines known code-patching case IDs for the kernel.
// Each should be listed below in CodePatchNames as well.
enum class CodePatchId : uint32_t {
  // This case serves as a verification that code-patching was performed before
  // the kernel was booted, `nop`ing out a trap among the kernel's earliest
  // instructions.
  kSelfTest,

  // The patched area is the one instruction that acts as the SMCCC conduit.
  // It is initially `smc #0` but may be replaced with `hvc #0`.
  kSmcccConduit,

  // The patched area is a single `mov w0, #...` instruction.  It gets patched
  // with the SMCCC function number used for SMCCC_ARCH_WORKAROUND_3.
  kSmcccWorkaroundFunction,
};

// The callback accepts an initializer-list of something constructible with
// {CodePatchId, std::string_view} and gets a list mapping kFooBar -> "FOO_BAR"
// name strings.  The names should be the kFooBar -> FOO_BAR transliteration of
// the enum names. In assembly code, these will be used as "CASE_ID_FOO_BAR".
inline constexpr auto WithCodePatchNames = [](auto&& callback) {
  return callback({
      {CodePatchId::kSelfTest, "SELF_TEST"},
      {CodePatchId::kSmcccConduit, "SMCCC_CONDUIT"},
      {CodePatchId::kSmcccWorkaroundFunction, "SMCCC_WORKAROUND_FUNCTION"},
  });
};

#endif  // ZIRCON_KERNEL_ARCH_ARM64_CODE_PATCHES_INCLUDE_ARCH_CODE_PATCHES_CASE_ID_H_
