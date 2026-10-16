// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_ZBITL_INCLUDE_LIB_ZBITL_ITEMS_KERNEL_H_
#define SRC_LIB_ZBITL_INCLUDE_LIB_ZBITL_ITEMS_KERNEL_H_

// This file describes the machine-specific aspects of the ZBI kernel item
// (ZBI_TYPE_KERNEL_*) and the bootable ZBI kernel image it heads.  The macros
// are usable from assembly; see <lib/zbitl/asm.h> for assembly helpers that
// build on them.

#ifdef __ASSEMBLER__
#include <zbi/asm/zbi.h>
#else
#include <lib/zbi-format/kernel.h>
#include <lib/zbi-format/zbi.h>

#include <cstdint>
#endif  // __ASSEMBLER__

#if defined(__aarch64__)

#define ARCH_KERNEL_TYPE (ZBI_TYPE_KERNEL_ARM64)

// Alignment required for an arm64 kernel ZBI.
#define ARCH_KERNEL_ALIGNMENT (1 << 16)

// Alignment required for an arm64 data ZBI.
#define ARCH_DATA_ALIGNMENT (1 << 12)

#elif defined(__riscv)

#define ARCH_KERNEL_TYPE (ZBI_TYPE_KERNEL_RISCV64)

// Alignment required for a riscv64 kernel ZBI.
#define ARCH_KERNEL_ALIGNMENT (1 << 12)

// Alignment required for a riscv64 data ZBI.
#define ARCH_DATA_ALIGNMENT (1 << 12)

#elif defined(__x86_64__) || defined(__i386__)

#define ARCH_KERNEL_TYPE (ZBI_TYPE_KERNEL_X64)

// Alignment required for an x86 kernel ZBI.
#define ARCH_KERNEL_ALIGNMENT (1 << 12)

// Alignment required for an x86 data ZBI.
#define ARCH_DATA_ALIGNMENT (1 << 12)

#else
#error "unsupported machine"
#endif

#ifndef __ASSEMBLER__

namespace zbitl {

constexpr uint32_t kKernelType = ARCH_KERNEL_TYPE;

constexpr uintptr_t kKernelAlignment = ARCH_KERNEL_ALIGNMENT;

constexpr uintptr_t kDataAlignment = ARCH_DATA_ALIGNMENT;

// Represents (the headers of) a bootable ZBI, loaded into memory by the boot
// loader.
struct KernelImage {
  zbi_header_t hdr_file;
  zbi_header_t hdr_kernel;
  zbi_kernel_t data_kernel;
};

}  // namespace zbitl

#endif  // !__ASSEMBLER__

#endif  // SRC_LIB_ZBITL_INCLUDE_LIB_ZBITL_ITEMS_KERNEL_H_
