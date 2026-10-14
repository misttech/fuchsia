// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_CRASHLOG_INCLUDE_LIB_CRASHLOG_H_
#define ZIRCON_KERNEL_LIB_CRASHLOG_INCLUDE_LIB_CRASHLOG_H_

#include <lib/crashlog/panic_buffer.h>
#include <zircon/boot/crash-reason.h>

#include <arch/crashlog_regs.h>
#include <kernel/persistent_ram.h>
#include <ktl/span.h>

#ifndef MIN_CRASHLOG_SIZE
#define MIN_CRASHLOG_SIZE 2048
#endif

// Forward-declaration; declared in <vm/vm_object.h>
class VmObject;

static constexpr size_t kMinCrashlogSize = MIN_CRASHLOG_SIZE;
static_assert((kMinCrashlogSize % kPersistentRamAllocationGranularity) == 0,
              "Minimum reserved crashlog size must be a multiple of the persistent RAM allocation "
              "granularity");

struct crashlog_t {
  crashlog_regs_t regs;
};

extern crashlog_t g_crashlog;

// Serialize the crashlog to string into target. If `reason' is OOM, then a
// different preamble will be used, and the backtrace will not be included.
size_t crashlog_to_string(ktl::span<char> target, zircon_crash_reason_t reason);

// Stash the recovered crashlog for later retrieval with |crashlog_get_stashed|.
void crashlog_stash(fbl::RefPtr<VmObject> crashlog);

// Returns the previously stashed recovered crashlog, or nullptr.
fbl::RefPtr<VmObject> crashlog_get_stashed();

extern PanicBuffer panic_buffer;

// A FILE that writes to both |stdout| and the global |panic_buffer|.
extern FILE stdout_panic_buffer;

extern "C" {

// Writes `data` to both the console and the global panic buffer, exactly as
// `fprintf(&stdout_panic_buffer, ...)` does from C++. This lets the Rust panic
// handler stream its formatted output piecewise, with no intermediate buffer.
void cpp_crashlog_panic_write(const char* data, size_t len);
size_t cpp_crashlog_to_string(char* target_ptr, size_t target_len, zircon_crash_reason_t reason);

}  // extern "C"

#endif  // ZIRCON_KERNEL_LIB_CRASHLOG_INCLUDE_LIB_CRASHLOG_H_
