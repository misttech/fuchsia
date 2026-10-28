// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_THREAD_SAMPLER_INCLUDE_LIB_THREAD_SAMPLER_THREAD_SAMPLER_H_
#define ZIRCON_KERNEL_LIB_THREAD_SAMPLER_INCLUDE_LIB_THREAD_SAMPLER_THREAD_SAMPLER_H_

#include <arch.h>
#include <lib/thread_sampler/thread_sampler_ffi.h>
#include <lib/zx/result.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

namespace sampler {

class ThreadSampler {
 public:
  constexpr ThreadSampler() = default;
  ~ThreadSampler() = default;

  static void RescheduleMarking() { rust_thread_sampler_reschedule_marking(); }

  static zx::result<> SampleThread(zx_koid_t pid, zx_koid_t tid, GeneralRegsSource source,
                                   const void* gregs, uint64_t session_id) {
    return zx::make_result(rust_thread_sampler_sample_thread(pid, tid, source, gregs, session_id));
  }
};

inline ThreadSampler gThreadSampler{};

// Joins the sampling session on the current cpu if one exists.
inline void sampler_percpu_init() { rust_sampler_percpu_init(); }

// Exit the sampling session on the current cpu if one exists.
inline void sampler_percpu_shutdown() { rust_sampler_percpu_shutdown(); }

}  // namespace sampler

#endif  // ZIRCON_KERNEL_LIB_THREAD_SAMPLER_INCLUDE_LIB_THREAD_SAMPLER_THREAD_SAMPLER_H_
