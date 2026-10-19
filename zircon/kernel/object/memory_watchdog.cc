// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/boot-options/boot-options.h>
#include <lib/debuglog.h>
#include <lib/ktrace.h>
#include <lib/object-constants.h>
#include <lib/page/size.h>

#include <kernel/ffi.h>
#include <object/memory_watchdog.h>
#include <platform/halt_helper.h>
#include <vm/evictor.h>
#include <vm/page_queues.h>
#include <vm/pmm.h>
#include <vm/pmm_node.h>
#include <vm/vm.h>

static_assert(sizeof(MemoryWatchdog) == kMemoryWatchdogStateSize);
static_assert(alignof(MemoryWatchdog) == kMemoryWatchdogStateAlign);

extern "C" {

void rust_memory_watchdog_construct(ffi::Uninitialized<MemoryWatchdog>* storage);
void rust_memory_watchdog_destroy(void* storage);
uint64_t rust_memory_watchdog_debug_num_bytes_till_pressure_level(const void* storage,
                                                                  uint8_t level);
void rust_memory_watchdog_dump(const void* storage);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const MemoryWatchdog* cpp_get_memory_watchdog() { return &GetMemoryWatchdog(); }

}  // extern "C"

MemoryWatchdog::MemoryWatchdog() {
  rust_memory_watchdog_construct(
      reinterpret_cast<ffi::Uninitialized<MemoryWatchdog>*>(&opaque_storage_));
}

MemoryWatchdog::~MemoryWatchdog() { rust_memory_watchdog_destroy(&opaque_storage_); }

uint64_t MemoryWatchdog::DebugNumBytesTillPressureLevel(PressureLevel level) {
  return rust_memory_watchdog_debug_num_bytes_till_pressure_level(&opaque_storage_,
                                                                  static_cast<uint8_t>(level));
}

void MemoryWatchdog::Dump() { rust_memory_watchdog_dump(&opaque_storage_); }
