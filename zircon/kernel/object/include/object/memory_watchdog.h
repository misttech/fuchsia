// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_MEMORY_WATCHDOG_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_MEMORY_WATCHDOG_H_

#include <lib/object-constants.h>
#include <stdint.h>

#include <kernel/ffi.h>

// Object is thread safe.
// Delegates business logic to its Rust state stored in opaque_storage_.
class MemoryWatchdog {
 public:
  enum PressureLevel : uint8_t {
    kOutOfMemory = 0,
    kImminentOutOfMemory,
    kCritical,
    kWarning,
    kNormal,
    kNumLevels,
  };

  MemoryWatchdog();
  ~MemoryWatchdog();

  MemoryWatchdog(const MemoryWatchdog&) = delete;
  MemoryWatchdog& operator=(const MemoryWatchdog&) = delete;

  uint64_t DebugNumBytesTillPressureLevel(PressureLevel level);
  void Dump();

 private:
  OpaqueStorage<kMemoryWatchdogStateSize, kMemoryWatchdogStateAlign> opaque_storage_;
};

MemoryWatchdog& GetMemoryWatchdog();

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" FFI_ALWAYS_INLINE const MemoryWatchdog* cpp_get_memory_watchdog();

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_MEMORY_WATCHDOG_H_
