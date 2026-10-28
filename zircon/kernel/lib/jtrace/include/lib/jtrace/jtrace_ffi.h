// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_JTRACE_INCLUDE_LIB_JTRACE_JTRACE_FFI_H_
#define ZIRCON_KERNEL_LIB_JTRACE_INCLUDE_LIB_JTRACE_JTRACE_FFI_H_

#include <stddef.h>

#include <kernel/jtrace_config.h>

namespace jtrace {
enum class TraceBufferType;
struct SmallEntry;
struct LargeEntry;
#if JTRACE_USE_LARGE_ENTRIES
using ProdEntry = LargeEntry;
#else
using ProdEntry = SmallEntry;
#endif
}  // namespace jtrace

extern "C" {

void rust_jtrace_init();
void rust_jtrace_set_after_thread_init_early();
void rust_jtrace_set_location(void* ptr, size_t len);
void rust_jtrace_invalidate();
void rust_jtrace_log(jtrace::ProdEntry& e);
void rust_jtrace_dump(jtrace::TraceBufferType which);

}  // extern "C"

#endif  // ZIRCON_KERNEL_LIB_JTRACE_INCLUDE_LIB_JTRACE_JTRACE_FFI_H_
