// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_SAMPLER_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_SAMPLER_DISPATCHER_H_

#include <lib/zx/result.h>
#include <zircon/rights.h>
#include <zircon/types.h>

#include <object/dispatcher.h>
#include <object/handle.h>
#include <object/thread_dispatcher.h>

// A Sampler manages sampling threads and writing the results out to per cpu buffers.
class SamplerDispatcher : public SoloDispatcher<SamplerDispatcher, ZX_DEFAULT_SAMPLER_RIGHTS> {
 public:
  ~SamplerDispatcher() override = default;

  // When the user drops their end of the buffer/sampler, we need to stop sampling and clean up the
  // state.
  void on_zero_handles() override;

  zx_obj_type_t get_type() const override { return ZX_OBJ_TYPE_SAMPLER; }

  static zx::result<KernelHandle<SamplerDispatcher>> Create(const zx_sampler_config_t& config);
  zx::result<> Start();
  zx::result<> Stop();
  zx::result<> AddThread(const fbl::RefPtr<ThreadDispatcher>& thread);

  // Given a thread's registers, pid, and tid, walk the thread's user stack and write each
  // pointer to the sampling buffers if sampling is enabled.
  //
  // WARNING: SampleThread both
  //     a) does a large number of user copies, and
  //     b) allocates a large amount of stack space
  //
  // It should only be called from Thread::Current::ProcessPendingSignals where we can be user that
  // the user copies are safe to do and where the current stack size should be relatively shallow.
  static zx::result<> SampleThread(zx_koid_t pid, zx_koid_t tid, GeneralRegsSource source,
                                   const void* gregs);

  // Read out the data contained in the sampler buffers into `ptr` return the number of bytes
  // written. The Sampling state must be Stopped before calling this function.
  //
  // `len` _must_ be at least equal to the total size of the sampler buffers, which can be queried
  // by passing a nullptr `ptr`. In this case, no data will be written and the return value will be
  // the required minimum size of the buffer to write to.
  ktl::pair<zx_status_t, size_t> ReadUser(user_out_ptr<void> ptr, size_t len);

 protected:
  SamplerDispatcher() = default;

  // Given information about a thread and its registers, walk its userstack and write out a sample
  // if sampling is enabled.
  zx::result<> SampleThreadImpl(zx_koid_t pid, zx_koid_t tid, GeneralRegsSource source,
                                const void* gregs);
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_SAMPLER_DISPATCHER_H_
