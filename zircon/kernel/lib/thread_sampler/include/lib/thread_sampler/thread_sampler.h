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
#include <object/thread_dispatcher.h>
#include <vm/pinned_vm_object.h>

namespace thread_sampler_tests {
class TestThreadSampler;
}  // namespace thread_sampler_tests

namespace sampler {
class ThreadSampler;

/**
 * The current state of the sampler.
 *
 * Valid state transitions are:
 *
 * ```
 *
 *    /- [ Destroying ] <- [ Reading ]
 *    |                      ^   |
 *    v                      |   v
 * [ Unallocated ] -> [ Configured ] -> [ Running ]
 *          ^           |      ^          |
 *          \----------/       |          v
 *                             \-[ Stopping ]
 *
 * ```
 */
enum class SamplingState : uint8_t {
  // The idle state for the sampler. We're not actively sampling nor is there a user handle
  // associated with us.
  Unallocated = 0,

  // We have buffers allocated and the user has a handle to us and can start a session.
  Configured,

  // The session is in progress. We are taking samples and writing data.
  Running,

  // The session is stopping, no more references to the buffers are allowed to be created and we're
  // waiting for existing references to be released.
  Stopping,

  // We have a read in flight. If we get a destruction request, we need to delay destruction of
  // resources until the read is completed.
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

  SamplingState State() const {
    return static_cast<SamplingState>(state_.load(ktl::memory_order_acquire) & kStateMask);
  }

  zx::result<size_t> ReadUser(user_out_ptr<void> ptr, uint32_t offset, size_t len);

  zx::result<> SetUp(const zx_sampler_config_t& config) TA_EXCL(ThreadSamplerLock::Get());
  zx::result<> Start() TA_EXCL(ThreadSamplerLock::Get());
  zx::result<> Stop() TA_EXCL(ThreadSamplerLock::Get());
  zx::result<> Destroy() TA_EXCL(ThreadSamplerLock::Get());

  zx::result<sampler::ReadToken> PrepareRead() TA_EXCL(ThreadSamplerLock::Get());
  void FinishRead(sampler::ReadToken&& token) TA_EXCL(ThreadSamplerLock::Get());
  // ReadUser calls into VmObject::ReadUser. As we could be copying to pager backed user memory, we
  // must not hold any locks.
  ktl::pair<zx_status_t, size_t> ReadUser(const sampler::ReadToken& token, user_out_ptr<void> ptr,
                                          size_t len) TA_EXCL(ThreadSamplerLock::Get());

  class PerCpuStateRef {
   public:
    explicit PerCpuStateRef(ktl::atomic<uint64_t>& state,
                            sampler::internal::PerCpuState& per_cpu_state)
        : per_cpu_state_(per_cpu_state), state_(state) {}
    ~PerCpuStateRef() { state_.fetch_sub(kBufferRefCountIncrement, ktl::memory_order_acq_rel); }
    sampler::internal::PerCpuState& Get() { return per_cpu_state_; }

   private:
    sampler::internal::PerCpuState& per_cpu_state_;
    ktl::atomic<uint64_t>& state_;
  };

  // Atomically acquire a reference to the buffers and ensure that the buffers are not destroyed
  // until the reference is released.
  ktl::optional<PerCpuStateRef> GetPerCpuState(cpu_num_t cpu_num) {
    if (cpu_num >= per_cpu_state_.size()) {
      return ktl::nullopt;
    }
    uint64_t expected = state_.load(ktl::memory_order_relaxed);
    bool success = false;
    do {
      const SamplingState state = static_cast<SamplingState>(expected & kStateMask);
      if (state != SamplingState::Running) {
        return ktl::nullopt;
      }
      DEBUG_ASSERT((expected & kBufferRefCountMask) != kBufferRefCountMask);
      const uint64_t desired = expected + kBufferRefCountIncrement;
      success = state_.compare_exchange_weak(expected, desired, ktl::memory_order_acq_rel,
                                             ktl::memory_order_relaxed);
    } while (!success);
    return ktl::make_optional<PerCpuStateRef>(state_, per_cpu_state_[cpu_num]);
  }

  // Given information about a thread and its registers, walk its userstack and write out a sample
  // if sampling is enabled.
  zx::result<> SampleThread(zx_koid_t pid, zx_koid_t tid, GeneralRegsSource source,
                            const void* gregs) TA_EXCL(ThreadSamplerLock::Get());

 private:
  friend class ::thread_sampler_tests::TestThreadSampler;
  DECLARE_SINGLETON_MUTEX(ThreadSamplerLock);

  void SetState(SamplingState new_state) TA_REQ(ThreadSamplerLock::Get()) {
    // While the SamplingState component of `state_` won't change out from under us as we require a
    // mutex to change it, the writes in flight counter could change, so we use a cmpxchg loop to
    // avoid losing a buffer ref count increment or decrement.
    uint64_t expected = state_.load(ktl::memory_order_relaxed);
    bool success = false;
    do {
      const uint64_t desired = (expected & ~(kStateMask)) | static_cast<uint64_t>(new_state);
      success = state_.compare_exchange_weak(expected, desired, ktl::memory_order_acq_rel,
                                             ktl::memory_order_relaxed);
    } while (!success);
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
  // RR RR RR RR RR BB BB SS
  //
  // SS: 8 bits, SamplingState
  // BB: 16 bits, BufferRefCount
  // RR: 40 bits, Reserved
  static constexpr uint64_t kStateMaskShift = 0;
  static constexpr uint64_t kStateMask = 0xFF << kStateMaskShift;

  static constexpr uint64_t kBufferRefCountShift = 8;
  static constexpr uint64_t kBufferRefCountIncrement = 1ul << kBufferRefCountShift;
  static constexpr uint64_t kBufferRefCountMask = uint64_t{0xFFFF} << kBufferRefCountShift;

  ktl::atomic<uint64_t> state_ = 0;
  fbl::Array<sampler::internal::PerCpuState> per_cpu_state_{nullptr};
};

extern ThreadSampler gThreadSampler;

}  // namespace sampler

#endif  // ZIRCON_KERNEL_LIB_THREAD_SAMPLER_INCLUDE_LIB_THREAD_SAMPLER_THREAD_SAMPLER_H_
