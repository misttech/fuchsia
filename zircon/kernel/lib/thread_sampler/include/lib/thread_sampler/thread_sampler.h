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
#include <object/dispatcher.h>
#include <object/thread_dispatcher.h>
#include <vm/pinned_vm_object.h>

class ThreadSamplerDispatcher;
namespace sampler {
class ThreadSampler;

/**
 * The current state of the sampler.
 *
 * Valid state transitions are:
 *
 * ```
 *                             /---------------------------\     /----------\
 *                             v                           |     v          |
 * [ Unallocated ] -> [ Allocated ] -> [ Configured ] -> [ Running ] -> [ Reading ]
 *                          ^  ^         |                                  |
 *                          |  \---------/                                  |
 *                          \----------------------------[ Destroying ] <---/
 * ```
 */
enum class SamplingState : uint8_t {
  // There are no buffers allocated for the sampler. This should only occur if the sampler has
  // never been used. Once we allocate buffers once, we never dealloc them.
  Unallocated = 0,

  // The idle state for the sampler. We have buffers allocated, but we're not actively sampling
  // nor is there a user handle associated with us.
  Allocated,

  // We have buffers allocated and the user has a handle to us and can start a session.
  Configured,

  // The session is in progress. We are taking samples and writing data.
  Running,

  // We have a read in flight. If we get a destruction request, we need to delay destruction of
  // resources until the read as completed.
  Reading,

  // We requested a session teardown while were we reading. Once the read finishes, we'll clear the
  // buffers and become allocated.
  Destroying,
};

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
  friend class ::sampler::ThreadSampler;
};

class ThreadSampler {
 public:
  ThreadSampler() = default;
  ~ThreadSampler() = default;

  // Set a timer based on the configured duration. When the timer expires, the currently running
  // thread will be marked to take a sample.
  void SetCurrCpuTimer();

  zx::result<size_t> ReadUser(user_out_ptr<void> ptr, uint32_t offset, size_t len);

  zx::result<> SetUp(const zx_sampler_config_t& config) TA_EXCL(ThreadSamplerLock::Get());
  zx::result<> Start() TA_EXCL(ThreadSamplerLock::Get());
  zx::result<> Stop() TA_EXCL(ThreadSamplerLock::Get());
  zx::result<> Destroy() TA_EXCL(ThreadSamplerLock::Get());

  SamplingState State() const {
    return static_cast<SamplingState>(state_.load(ktl::memory_order_acquire) & kStateMask);
  }

  zx::result<sampler::ReadToken> PrepareRead() TA_EXCL(ThreadSamplerLock::Get());
  void FinishRead(sampler::ReadToken&& token) TA_EXCL(ThreadSamplerLock::Get());
  // ReadUser calls into VmObject::ReadUser. As we could be copying to pager backed user memory, we
  // must not hold any locks.
  ktl::pair<zx_status_t, size_t> ReadUser(const sampler::ReadToken& token, user_out_ptr<void> ptr,
                                          size_t len) TA_EXCL(ThreadSamplerLock::Get());

  sampler::internal::PerCpuState& GetPerCpuState(cpu_num_t cpu_num) const {
    DEBUG_ASSERT(cpu_num < per_cpu_state_.size());
    return per_cpu_state_[cpu_num];
  }

  // Given information about a thread and its registers, walk its userstack and write out a sample
  // if sampling is enabled.
  zx::result<> SampleThread(zx_koid_t pid, zx_koid_t tid, GeneralRegsSource source,
                            const void* gregs) TA_EXCL(ThreadSamplerLock::Get());

 private:
  DECLARE_SINGLETON_MUTEX(ThreadSamplerLock);

  void SetState(SamplingState new_state) TA_REQ(ThreadSamplerLock::Get()) {
    // We require the ThreadSamplerLock to modify state. It's fine to use store rather than a
    // cmpxchg as we can't be racing with another write.
    state_.store(static_cast<uint64_t>(new_state), ktl::memory_order_release);
  }

  void StopLocked() TA_REQ(ThreadSamplerLock::Get());

  // per_cpu_state_ and state_ may be READ without acquiring the ThreadSamplerLock.
  // However, the lock must be acquired to WRITE them.
  //
  // per_cpu_state_ must not be modified while the session is in the states:
  //  - Configured
  //  - Running
  //  - Reading
  // state_ is eight bytes composed as:
  //
  // RR RR RR RR RR RR RR SS
  //
  // SS: 8 bytes, SamplingState
  // RR: 56 bytes, Reserved
  static constexpr uint64_t kStateMaskShift = 0;
  static constexpr uint64_t kStateMask = 0xFF << kStateMaskShift;

  ktl::atomic<uint64_t> state_ = 0;
  fbl::Array<sampler::internal::PerCpuState> per_cpu_state_{nullptr};
};
}  // namespace sampler

// A ThreadSampler manages sampling threads and writing the results out to per cpu buffers.
class ThreadSamplerDispatcher
    : public SoloDispatcher<ThreadSamplerDispatcher, ZX_DEFAULT_SAMPLER_RIGHTS> {
 public:
  ~ThreadSamplerDispatcher() override = default;

  // When the user drops their end of the buffer/sampler, we need to stop sampling and clean up the
  // state.
  void on_zero_handles() override;

  zx_obj_type_t get_type() const override { return ZX_OBJ_TYPE_SAMPLER; }

  static zx::result<KernelHandle<ThreadSamplerDispatcher>> Create(
      const zx_sampler_config_t& config);
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
  ThreadSamplerDispatcher() = default;

  // Given information about a thread and its registers, walk its userstack and write out a sample
  // if sampling is enabled.
  zx::result<> SampleThreadImpl(zx_koid_t pid, zx_koid_t tid, GeneralRegsSource source,
                                const void* gregs);
};

#endif  // ZIRCON_KERNEL_LIB_THREAD_SAMPLER_INCLUDE_LIB_THREAD_SAMPLER_THREAD_SAMPLER_H_
