// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_SAMPLER_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_SAMPLER_DISPATCHER_H_

#include <arch.h>
#include <lib/object-constants.h>
#include <lib/zx/result.h>
#include <zircon/rights.h>
#include <zircon/syscalls/sampler.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <object/dispatcher.h>
#include <object/handle.h>

class SamplerDispatcher;
extern "C" {
zx_status_t cpp_sampler_dispatcher_create(
    ffi::Uninitialized<KernelHandle<SamplerDispatcher>>* handle_out);
}

// A Sampler manages sampling threads and writing the results out to per cpu buffers.
class SamplerDispatcher final : public Dispatcher {
 public:
  // Given a thread's registers, pid, and tid, walk the thread's user stack and write each
  // pointer to the sampling buffers if sampling is enabled.
  //
  // WARNING: SampleThread both
  //     a) does a large number of user copies, and
  //     b) allocates a large amount of stack space
  //
  // It should only be called from Thread::Current::ProcessPendingSignals where we can be sure that
  // the user copies are safe to do and where the current stack size should be relatively shallow.
  static zx::result<> SampleThread(zx_koid_t pid, zx_koid_t tid, GeneralRegsSource source,
                                   const void* gregs, uint64_t session_id);

  ~SamplerDispatcher() final;

  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_SAMPLER; }
  zx_koid_t get_related_koid() const final { return ZX_KOID_INVALID; }
  bool is_waitable() const final { return ZX_DEFAULT_SAMPLER_RIGHTS & ZX_RIGHT_WAIT; }

  // When the user drops their end of the buffer/sampler, we need to stop sampling and clean up the
  // state.
  void on_zero_handles() final;

  zx_status_t user_signal_self(uint32_t clear_mask, uint32_t set_mask) final;
  zx_status_t user_signal_peer(uint32_t clear_mask, uint32_t set_mask) final {
    return ZX_ERR_NOT_SUPPORTED;
  }

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  friend zx_status_t cpp_sampler_dispatcher_create(
      ffi::Uninitialized<KernelHandle<SamplerDispatcher>>*);
  SamplerDispatcher();

  OpaqueStorage<kSamplerDispatcherStateSize, kSamplerDispatcherStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_SAMPLER_DISPATCHER_H_
