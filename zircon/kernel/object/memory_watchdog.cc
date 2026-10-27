// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/memory_watchdog.h"

static_assert(sizeof(MemoryWatchdog) == kMemoryWatchdogStateSize);
static_assert(alignof(MemoryWatchdog) == kMemoryWatchdogStateAlign);

extern "C" {

void rust_memory_watchdog_construct(ffi::Uninitialized<MemoryWatchdog>* storage);
void rust_memory_watchdog_destroy(void* storage);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const MemoryWatchdog* cpp_get_memory_watchdog() { return &GetMemoryWatchdog(); }

}  // extern "C"

MemoryWatchdog::MemoryWatchdog() {
  rust_memory_watchdog_construct(
      reinterpret_cast<ffi::Uninitialized<MemoryWatchdog>*>(&opaque_storage_));
}

MemoryWatchdog::~MemoryWatchdog() { rust_memory_watchdog_destroy(&opaque_storage_); }
