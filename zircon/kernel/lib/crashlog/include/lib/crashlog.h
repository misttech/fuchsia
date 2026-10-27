// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_CRASHLOG_INCLUDE_LIB_CRASHLOG_H_
#define ZIRCON_KERNEL_LIB_CRASHLOG_INCLUDE_LIB_CRASHLOG_H_

#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <zircon/boot/crash-reason.h>

#include <arch/crashlog_regs.h>
#include <kernel/persistent_ram.h>

// The crashlog is implemented in Rust; see //zircon/kernel/lib/crashlog/src/crashlog.rs.

#ifndef MIN_CRASHLOG_SIZE
#define MIN_CRASHLOG_SIZE 2048
#endif

static constexpr size_t kMinCrashlogSize = MIN_CRASHLOG_SIZE;
static_assert((kMinCrashlogSize % kPersistentRamAllocationGranularity) == 0,
              "Minimum reserved crashlog size must be a multiple of the persistent RAM allocation "
              "granularity");

struct crashlog_t {
  crashlog_regs_t regs;
};

// Defined in Rust.
extern crashlog_t g_crashlog;

// A FILE that writes to both |stdout| and the global panic buffer.  Defined in Rust.
extern "C" FILE stdout_panic_buffer;

#endif  // ZIRCON_KERNEL_LIB_CRASHLOG_INCLUDE_LIB_CRASHLOG_H_
