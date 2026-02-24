// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_UNWINDER_UNWINDER_BASE_H_
#define SRC_LIB_UNWINDER_UNWINDER_BASE_H_

#include "sdk/lib/fit/include/lib/fit/function.h"
#include "src/lib/unwinder/error.h"
#include "src/lib/unwinder/frame.h"

namespace unwinder {

class AsyncMemory;
class ElfModuleCache;
class Memory;

// Base class for all unwinders.
class UnwinderBase {
 public:
  explicit UnwinderBase(const ElfModuleCache& module_cache) : module_cache_(module_cache) {}
  virtual ~UnwinderBase() = default;

  // Unwind one frame, populating |next| with the new register values. |next| is invalid if an error
  // is returned.
  virtual Error Step(Memory* stack, const Frame& current, Frame& next) = 0;

  // Unwind one frame, possibly asynchronously. The callback is issued with the resulting registers
  // for the new frame on success. If the error is populated, the registers are not valid.
  virtual void AsyncStep(AsyncMemory* stack, const Frame& current,
                         fit::callback<void(Error, Registers)> cb) {
    return cb(Error("Not implemented"), Registers(current.regs.arch()));
  }

  // The trust that should be associated with this unwinder.
  virtual Frame::Trust trust() const = 0;

  const ElfModuleCache& module_cache() const { return module_cache_; }

 private:
  const ElfModuleCache& module_cache_;
};

}  // namespace unwinder

#endif  // SRC_LIB_UNWINDER_UNWINDER_BASE_H_
