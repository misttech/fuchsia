// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
//
#ifndef ZIRCON_KERNEL_LIB_THREAD_SAMPLER_INCLUDE_LIB_THREAD_SAMPLER_THREAD_SAMPLER_H_
#define ZIRCON_KERNEL_LIB_THREAD_SAMPLER_INCLUDE_LIB_THREAD_SAMPLER_THREAD_SAMPLER_H_

#include <arch.h>
#include <lib/thread_sampler/per_cpu_state.h>
#include <lib/zx/result.h>
#include <zircon/errors.h>
#include <zircon/syscalls-next.h>
#include <zircon/types.h>

#include <fbl/ref_ptr.h>
#include <kernel/lockdep.h>
#include <kernel/mutex.h>
#include <kernel/spinlock.h>
#include <object/io_buffer_dispatcher.h>
#include <object/thread_dispatcher.h>
#include <vm/pinned_vm_object.h>

class ThreadSamplerDispatcher;
namespace sampler {

// A helper type to ensure we always call our Read functions in the order of PrepareRead,
// ReadUserImpl, then FinishRead.
//
// The only way to get a token is through PrepareRead. Calling ReadUserImpl requires a token, and
// the only way to "disarm" the token (prevent an assert on destruction), is to pass it to
// FinishRead.
struct ReadToken {
 public:
  ReadToken(const ReadToken&) = delete;
  ReadToken& operator=(const ReadToken&) = delete;
  ReadToken(ReadToken&& other) : disarmed(other.disarmed) { other.disarmed = true; }
  ReadToken& operator=(ReadToken&& other) {
    disarmed = other.disarmed;
    other.disarmed = true;
    return *this;
  }
  ~ReadToken() { DEBUG_ASSERT_MSG(disarmed, "FinishRead was not called after Reading"); }

 private:
  ReadToken() = default;

  bool disarmed = false;
  friend class ::ThreadSamplerDispatcher;
};

}  // namespace sampler

// A ThreadSampler is really just an IOBuffer with some added control methods on top to start and
// stop sampling.
class ThreadSamplerDispatcher : public IoBufferDispatcher {
 public:
  ~ThreadSamplerDispatcher() override = default;

  // Set a timer based on the configured duration. When the timer expires, the currently running
  // thread will be marked to take a sample.
  void SetCurrCpuTimer();

  // When the user drops their end of the buffer/sampler, we need to stop sampling and clean up the
  // state.
  void OnPeerZeroHandlesLocked() TA_REQ(get_lock()) override;

  enum class SamplingState : uint8_t {
    Configured,
    Running,
    Reading,
    Destroying,
    Destroyed,
  };

  SamplingState State() const {
    Guard<CriticalMutex> guard(get_lock());
    return state_;
  }

  zx::result<size_t> ReadUser(user_out_ptr<void> ptr, uint32_t offset, size_t len);

  static zx::result<KernelHandle<ThreadSamplerDispatcher>> Create(
      const zx_sampler_config_t& config);
  static zx::result<> Start(const fbl::RefPtr<IoBufferDispatcher>& disp);
  static zx::result<> Stop(const fbl::RefPtr<IoBufferDispatcher>& disp);
  static zx::result<> AddThread(const fbl::RefPtr<IoBufferDispatcher>& disp,
                                const fbl::RefPtr<ThreadDispatcher>& thread);

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
                                   void* gregs);

  // Read out the data contained in the sampler buffers into `ptr` return the number of bytes
  // written. The Sampling state must be Stopped before calling this function.
  //
  // `len` _must_ be at least equal to the total size of the sampler buffers, which can be queried
  // by passing a nullptr `ptr`. In this case, no data will be written and the return value will be
  // the required minimum size of the buffer to write to.
  static ktl::pair<zx_status_t, size_t> ReadUser(const fbl::RefPtr<IoBufferDispatcher>& disp,
                                                 user_out_ptr<void> ptr, size_t len);

 protected:
  sampler::internal::PerCpuState& GetPerCpuState(size_t i) const { return per_cpu_state_[i]; }

  ThreadSamplerDispatcher(fbl::RefPtr<PeerHolder<IoBufferDispatcher>> holder,
                          IobEndpointId endpoint_id, fbl::RefPtr<SharedIobState> shared_state)
      : IoBufferDispatcher(ktl::move(holder), endpoint_id, ktl::move(shared_state)) {}

  // Create a ThreadSamplerDispatcher. The ThreadSamplerDispatcher is a peered object with one end
  // readable and one end writable. The write end is retained by the kernel to write samples to. The
  // user receives the read end of the buffer so that they may read the samples written.
  static zx::result<> CreateImpl(const zx_sampler_config_t& config,
                                 KernelHandle<ThreadSamplerDispatcher>& read_handle_out,
                                 KernelHandle<ThreadSamplerDispatcher>& write_handle_out);

  // Given information about a thread and its registers, walk its userstack and write out a sample
  // if sampling is enabled.
  zx::result<> SampleThreadImpl(zx_koid_t pid, zx_koid_t tid, GeneralRegsSource source,
                                void* gregs);
  zx::result<> StartImpl() TA_EXCL(get_lock());
  zx::result<> StopImpl() TA_EXCL(get_lock());

 private:
  void StopLocked() TA_REQ(get_lock());

  zx::result<sampler::ReadToken> PrepareRead() TA_REQ(ThreadSamplerLock::Get());
  void FinishRead(sampler::ReadToken&& token) TA_REQ(ThreadSamplerLock::Get());

  // ReadUser calls into VmObject::ReadUser. As we could be copying to pager backed user memory, we
  // must not hold any locks.
  ktl::pair<zx_status_t, size_t> ReadUserImpl(const sampler::ReadToken& token,
                                              user_out_ptr<void> ptr, size_t len)
      TA_EXCL(ThreadSamplerLock::Get(), get_lock());

  SamplingState state_ TA_GUARDED(get_lock()){SamplingState::Configured};
  fbl::Array<sampler::internal::PerCpuState> per_cpu_state_;

  DECLARE_SINGLETON_MUTEX(ThreadSamplerLock);
  // We have only a single global thread sampler at a time. Another callers will get
  // ZX_ERR_ALREADY_EXISTS until the existing sampler is released.
  static KernelHandle<ThreadSamplerDispatcher> gThreadSampler_ TA_GUARDED(ThreadSamplerLock::Get());
};

#endif  // ZIRCON_KERNEL_LIB_THREAD_SAMPLER_INCLUDE_LIB_THREAD_SAMPLER_THREAD_SAMPLER_H_
