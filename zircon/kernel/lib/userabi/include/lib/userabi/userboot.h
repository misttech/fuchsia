// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_USERABI_INCLUDE_LIB_USERABI_USERBOOT_H_
#define ZIRCON_KERNEL_LIB_USERABI_INCLUDE_LIB_USERABI_USERBOOT_H_

#ifdef _KERNEL

#include <stddef.h>
#include <stdio.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <vm/handoff-end.h>

// Called at the end of the boot process in the main kernel initialization sequence.
void userboot_init(HandoffEnd handoff_end);

__BEGIN_CDECLS

void rust_userboot_init(HandoffEnd* handoff_end);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE size_t cpp_platform_crashlog_recover(FILE* tgt);
FFI_ALWAYS_INLINE void cpp_platform_crashlog_enable_uptime_updates(bool enabled);
FFI_ALWAYS_INLINE void cpp_boot_options_show(bool defaults, FILE* out);

#if ENABLE_ENTROPY_COLLECTOR_TEST
FFI_ALWAYS_INLINE bool cpp_entropy_was_lost();
FFI_ALWAYS_INLINE VmObject* cpp_entropy_vmo();
FFI_ALWAYS_INLINE size_t cpp_entropy_vmo_stream_size();
#endif

#if defined(__aarch64__) && ZX_DEBUG_ASSERT_IMPLEMENTED
FFI_ALWAYS_INLINE void cpp_arm64_print_midr_cpu_name(FILE* out);
#endif

__END_CDECLS

#endif  // _KERNEL

#endif  // ZIRCON_KERNEL_LIB_USERABI_INCLUDE_LIB_USERABI_USERBOOT_H_
