// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_LIB_FLOW_ID_H_
#define ZIRCON_KERNEL_INCLUDE_LIB_FLOW_ID_H_

#include <stdint.h>

#if ENABLE_RUST_IN_ZIRCON

// Generates globally unique 64-bit flow IDs for tracing (C-exported).
extern "C" uint64_t flow_id_generate();

#else  // ENABLE_RUST_IN_ZIRCON

#include <ktl/atomic.h>

// Generates globally unique 64bit flow ids for tracing.
class FlowId {
 public:
  // Allocates and returns a flow id.
  static uint64_t Generate() {
    return flow_id_generator_.fetch_add(1ULL, ktl::memory_order_relaxed);
  }

 private:
  // Decrease the likelihood of collisions with zero-based userspace flow id
  // generators by starting in the second half of the flow id space.
  static constexpr uint64_t kFirstKernelFlowId{uint64_t{1} << 63};

  inline static ktl::atomic<uint64_t> flow_id_generator_{kFirstKernelFlowId};
};

#endif  // ENABLE_RUST_IN_ZIRCON

#endif  // ZIRCON_KERNEL_INCLUDE_LIB_FLOW_ID_H_
