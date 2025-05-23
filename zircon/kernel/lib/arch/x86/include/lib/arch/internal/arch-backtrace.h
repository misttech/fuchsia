// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_ARCH_X86_INCLUDE_LIB_ARCH_INTERNAL_ARCH_BACKTRACE_H_
#define ZIRCON_KERNEL_LIB_ARCH_X86_INCLUDE_LIB_ARCH_INTERNAL_ARCH_BACKTRACE_H_

#include <stddef.h>

namespace arch::internal {

// Frame pointers point directly to the FP, PC pair on the stack.
inline constexpr ptrdiff_t kArchFpOffset = 0;

}  // namespace arch::internal

#endif  // ZIRCON_KERNEL_LIB_ARCH_X86_INCLUDE_LIB_ARCH_INTERNAL_ARCH_BACKTRACE_H_
