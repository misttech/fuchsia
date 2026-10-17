// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_HWP_H_
#define ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_HWP_H_

#include <lib/boot-options/boot-options.h>  // IntelHwpPolicy

#include <arch/x86/cpuid.h>
#include <arch/x86/platform_access.h>
#include <ktl/optional.h>
#include <ktl/string_view.h>

namespace x86 {

using IntelHwpPolicy = IntelHwpPolicy;

// Parse a string as an HWP policy.
ktl::optional<IntelHwpPolicy> IntelHwpParsePolicy(const char* str);

// Initialise the Intel HWP on the current CPU.
//
// If HWP is not supported on the current CPU, no action will be taken.
void IntelHwpInit(const cpu_id::CpuId*, MsrAccess*, IntelHwpPolicy);

// Determine if Intel HWP is supported on given CPU.
bool IntelHwpSupported(const cpu_id::CpuId* cpuid);

}  // namespace x86

extern "C" {
bool cpp_x86_intel_hwp_parse_policy(const char* str, uint8_t* out_policy);
void cpp_x86_intel_hwp_init(uint8_t policy);
uint64_t cpp_x86_make_hwp_request(uint8_t min_perf, uint8_t max_perf, uint8_t desired_perf,
                                  uint8_t epp);
bool cpp_x86_intel_hwp_supported();
}  // extern "C"

#endif  // ZIRCON_KERNEL_ARCH_X86_INCLUDE_ARCH_X86_HWP_H_
