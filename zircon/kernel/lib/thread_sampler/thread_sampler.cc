// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/boot-options/boot-options.h>
#include <lib/fit/defer.h>
#include <lib/fxt/serializer.h>
#include <lib/thread_sampler/per_cpu_state.h>
#include <lib/thread_sampler/thread_sampler.h>
#include <lib/zx/time.h>

#include <fbl/array.h>
#include <kernel/dpc.h>
#include <kernel/event.h>
#include <kernel/mp.h>
#include <kernel/spinlock.h>
#include <lk/init.h>
#include <object/io_buffer_dispatcher.h>
#include <object/process_dispatcher.h>

// We have only a single global thread sampler at a time. Another callers will get
// ZX_ERR_ALREADY_EXISTS until the existing sampler is released.
namespace sampler {
sampler::ThreadSampler gThreadSampler{};
}  // namespace sampler

zx::result<> sampler::ThreadSampler::SetUp(const zx_sampler_config_t& config) {
  fbl::Array<sampler::internal::PerCpuState> per_cpu_state;
  Guard<Mutex> guard(ThreadSamplerLock::Get());
  SamplingState state = State();
  if (state != SamplingState::Unallocated && state != SamplingState::Allocated) {
    return zx::error(ZX_ERR_ALREADY_EXISTS);
  }

  const size_t num_cpus = percpu::processor_count();
  if (!per_cpu_state_) {
    // Perform the allocations for the state without the lock held as this may potentially block
    // waiting for memory.
    zx::result<> result;
    guard.CallUnlocked([&]() {
      fbl::AllocChecker ac;
      per_cpu_state = fbl::MakeArray<sampler::internal::PerCpuState>(&ac, num_cpus);
      if (!ac.check()) {
        result = zx::error(ZX_ERR_NO_MEMORY);
        return;
      }

      // Even though the buffer is per_cpu, we are fine to set up each cpu state here on a single
      // cpu. When we start sampling, we call mp_sync_exec which will synchronize the written
      // per_cpu_states.
      for (unsigned i = 0; i < num_cpus; i++) {
        if (zx::result<> setup_result = per_cpu_state[i].SetUp(config, i);
            setup_result.is_error()) {
          result = setup_result.take_error();
          return;
        }
      }
    });
    // Propagate any errors.
    if (result.is_error()) {
      return result;
    }
    // Reload and check state again as it may have changed while the lock was dropped.
    state = State();
    if (state != SamplingState::Unallocated && state != SamplingState::Allocated) {
      return zx::error(ZX_ERR_ALREADY_EXISTS);
    }
  }
  // Re-check whether there is per_cpu_state_ as this may have changed while the lock was dropped.
  // If we raced and someone else allocated state before us this is fine and we will just drop the
  // local allocation.
  if (per_cpu_state_) {
    // We don't have a method of atomically tracking outstanding sample request, so it's possible
    // that an outstanding sample has a reference to the per_cpu_states. Instead, we Clear the
    // buffers which is safe as we use a lockless spsc buffer.
    for (unsigned i = 0; i < num_cpus; i++) {
      per_cpu_state_[i].Drain();
    }
  } else {
    ASSERT(per_cpu_state);
    per_cpu_state_ = ktl::move(per_cpu_state);
  }

  SetState(SamplingState::Configured);
  return zx::ok();
}

zx::result<> sampler::ThreadSampler::Start() {
  Guard<Mutex> guard(ThreadSamplerLock::Get());
  if (State() != SamplingState::Configured) {
    return zx::error(ZX_ERR_BAD_STATE);
  }

  DEBUG_ASSERT(!per_cpu_state_.empty());
  for (sampler::internal::PerCpuState& state : per_cpu_state_) {
    state.EnableWrites();
  }

  mp_sync_exec(
      mp_ipi_target::ALL, 0,
      [](void* sampler) { static_cast<sampler::ThreadSampler*>(sampler)->SetCurrCpuTimer(); },
      this);

  SetState(SamplingState::Running);
  return zx::ok();
}

zx::result<> sampler::ThreadSampler::Stop() {
  Guard<Mutex> guard(ThreadSamplerLock::Get());
  if (State() != SamplingState::Running) {
    return zx::error(ZX_ERR_BAD_STATE);
  }
  StopLocked();
  return zx::ok();
}

void sampler::ThreadSampler::StopLocked() {
  for (sampler::internal::PerCpuState& state : per_cpu_state_) {
    state.DisableWrites();
    state.CancelTimer();
  }

  // Some timers may not have not been able to be canceled, so we need to wait for any samples that
  // have already started to finish.
  constexpr zx_duration_t warn_duration = ZX_SEC(30);
  zx_duration_t sleep_duration = ZX_MSEC(1);
  zx_instant_mono_t next_warn_time = zx_time_add_duration(current_mono_time(), warn_duration);
  int64_t warn_events = 0;
  constexpr zx_duration_t max_sleep_duration = ZX_SEC(1);
  for (const sampler::internal::PerCpuState& i : per_cpu_state_) {
    while (i.PendingTimer() || i.PendingWrites()) {
      // Warn if we have spend an 'unreasonable' amount of time waiting.
      if (current_mono_time() > next_warn_time) {
        warn_events++;
        printf("WARNING: Waited more than %ld seconds for sampling to finish\n",
               (warn_events * warn_duration) / ZX_SEC(1));
        next_warn_time = zx_time_add_duration(next_warn_time, warn_duration);
      }
      Thread::Current::SleepRelative(sleep_duration);
      // Scale up the sleep duration to balance being initially responsive and not consuming
      // excessive CPU.
      sleep_duration = ktl::min(sleep_duration * 2, max_sleep_duration);
    }
  }

  // At this point, there are no longer pending writes. There may still be threads:
  //
  // 1) signaled to be sampled but haven't reached ProcessPendingSignals yet, or
  // 2) are mid taking a sample but haven't yet reserved a PendingWrite
  //
  // For 1): Such threads will block on on getting the dispatcher lock which we currently hold to
  // read the state. When they acquire it, they will see that the session is no longer running and
  // skip taking a sample.
  //
  // For 2): Threads will check the PerCpuState and see that writes are disabled and will skip
  // writing the sample. While taking a sample, threads have taken an fbl::RefPtr to the sampling
  // state so that the PerCpuStates are not at risk of being destroyed.
  SetState(SamplingState::Configured);
}

zx::result<> sampler::ThreadSampler::SampleThread(zx_koid_t pid, zx_koid_t tid,
                                                  GeneralRegsSource source, const void* gregs) {
  // We are going to attempt a usercopy below which might fault, so interrupts cannot be disabled.
  DEBUG_ASSERT(!arch_ints_disabled());
  // We need to be a little bit careful here because we could be racing with a Stop operation. The
  // Stop operation:
  //
  // 1) Disables Writes
  // 2) Cancels each Timer
  // 3) Waits for all PendingWrites to finish
  //
  // It does this while holding the ThreadSamplerDispatcher lock. This means if SetPendingWrite and
  // then attempt to obtain the ThreadSamplerDispatcher lock, we could deadlock.
  //
  // Instead, we'll do a single enabled check here before attempting to read the stack, which will
  // take some time. Once we've collected our data and are ready to write out, we'll
  // SetPendingWrite to hold onto the buffers for the duration of the write.
  //
  // If we find that writes are enabled, we are safe to write to the buffers as
  // Stop will not destroy them until we lower the PendingWrite bit.
  //
  // If we find that writes are disabled, we throw away our sample as it's no longer safe to write
  // to the buffers.
  if (State() != SamplingState::Running) {
    return zx::error(ZX_ERR_BAD_STATE);
  }

  size_t frame_num = 0;
  constexpr size_t kMaxUserBacktraceSize = 64;
  // We're dropping 512 bytes on the kernel stack here and we need a be careful not to overflow it.
  //
  // This amount of bytes _should_ be safe because SampleThread is only called during
  // Thread::Current::ProcessPendingSignals which occurs directly before returning to usermode. At
  // this point, the stack will be shallow.
  vaddr_t bt[kMaxUserBacktraceSize]{};

  vaddr_t fp = 0;
  vaddr_t pc = 0;
  switch (source) {
    case GeneralRegsSource::None:
      break;
    case GeneralRegsSource::Iframe:
#ifdef __x86_64__
      fp = reinterpret_cast<const iframe_t*>(gregs)->rbp;
      pc = reinterpret_cast<const iframe_t*>(gregs)->ip;
#endif
#ifdef __aarch64__
      bt[frame_num++] = (reinterpret_cast<const iframe_t*>(gregs)->elr) - 4;
      fp = reinterpret_cast<const iframe_t*>(gregs)->r[29];
      pc = (reinterpret_cast<const iframe_t*>(gregs)->lr) - 4;
#endif
#ifdef __riscv
      fp = reinterpret_cast<const iframe_t*>(gregs)->regs.s0;
      pc = reinterpret_cast<const iframe_t*>(gregs)->regs.pc;
#endif
      break;
#ifdef __x86_64__
    case GeneralRegsSource::Syscall:
      fp = reinterpret_cast<const syscall_regs_t*>(gregs)->rbp;
      pc = reinterpret_cast<const syscall_regs_t*>(gregs)->rip;
      break;
#endif
  }

  if (pc == 0) {
    return zx::error(ZX_ERR_BAD_STATE);
  }

  bt[frame_num++] = pc;

  while (frame_num < kMaxUserBacktraceSize) {
    vaddr_t actual_fp = fp;
    if (fp == 0) {
      // We've reached the top of the frame pointer chain.
      break;
    }

    // RISC-V has a nonstandard frame pointer which points to the CFA instead of
    // the previous frame pointer. Since the frame pointer and return address are
    // always just below the CFA, subtract 16 bytes to get to the actual frame pointer.
#if __riscv
    actual_fp -= 16;
#endif

    user_in_ptr<const vaddr_t> user_next_fp{reinterpret_cast<vaddr_t*>(actual_fp)};
    user_in_ptr<const vaddr_t> user_pc{reinterpret_cast<vaddr_t*>(actual_fp + 8)};

    // A well formed frame pointer chain ends in 0 and should never fail to copy. If a thread's
    // stack is not readable or well formatted, we return an error to indicate that sampling should
    // be disabled for the offending thread.
    zx_status_t copy_res = user_pc.copy_from_user(&pc);
    if (copy_res != ZX_OK) {
      // We eat the copy_res and return ZX_ERR_NOT_SUPPORTED here and below to indicate that we
      // failed to take a sample, but we might still succeed in the future. A thread may not
      // necessarily have valid frame pointers at all points in execution, so don't give on this
      // thread just yet.
      return zx::error(ZX_ERR_NOT_SUPPORTED);
    }
    if (pc == 0) {
      break;
    }
    bt[frame_num++] = pc;
    copy_res = user_next_fp.copy_from_user(&fp);
    if (copy_res != ZX_OK) {
      return zx::error(ZX_ERR_NOT_SUPPORTED);
    }
  }

  // Up until this point, interrupts are enabled so that we can handle faults when doing usercopies.
  // However, once we want to write, we aren't using a concurrent writing algorithm. We need to
  // ensure we don't get interrupted or context switched while we are writing. Otherwise, we could
  // SetPendingWrite, get context switched out, and then have another thread attempt to
  // SetPendingWrite which would assert.
  InterruptDisableGuard irqd;
  sampler::internal::PerCpuState& cpu_state = GetPerCpuState(arch_curr_cpu_num());
  const bool enabled = cpu_state.SetPendingWrite();
  if (!enabled) {
    // Even though we didn't successfully write a sample, we return a success result -- we should
    // still try to sample the thread as it may later be scheduled on a different cpu.
    return zx::ok();
  }
  auto d = fit::defer([&cpu_state]() { cpu_state.ResetPendingWrite(); });

  constexpr fxt::StringRef<fxt::RefType::kId> empty_string{0};
  const fxt::ThreadRef current_thread{pid, tid};
  zx_status_t write_result = fxt::WriteLargeBlobRecordWithMetadata(
      &cpu_state, current_mono_ticks(), empty_string, empty_string, current_thread, bt,
      sizeof(uint64_t) * frame_num);

  if (write_result != ZX_OK) {
    cpu_state.DisableWrites();
    dprintf(INFO, "Buffer full, disabling writes on cpu: %u\n", arch_curr_cpu_num());
  }
  return zx::ok();
}

zx::result<> sampler::ThreadSampler::Destroy() {
  Guard<Mutex> guard(ThreadSamplerLock::Get());
  SamplingState state = State();
  if (state == SamplingState::Reading) {
    // There's a read in flight, we can't destroy our buffers yet. We set the state to Destroying,
    // and when the read finishes, it will also clean up the buffers.
    SetState(SamplingState::Destroying);
    return zx::ok();
  }

  // The userspace end of the sampler has closed. Time to clean up our state
  if (state == SamplingState::Running) {
    StopLocked();
  }

  // After StopLocked, we have prevented further threads from accessing the per_cpu_states, and then
  // waited for any threads that were accessing the states to finish.
  //
  // It's now safe to destroy our cpu states. This will destroy the mappings and pinnings that the
  // kernel keeps to write to.
  SetState(SamplingState::Allocated);
  return zx::ok();
}

void sampler::ThreadSampler::SetCurrCpuTimer() { GetPerCpuState(arch_curr_cpu_num()).SetTimer(); }

zx::result<sampler::ReadToken> sampler::ThreadSampler::PrepareRead() {
  Guard<Mutex> guard(ThreadSamplerLock::Get());
  if (State() != sampler::SamplingState::Configured) {
    return zx::error(ZX_ERR_BAD_STATE);
  }
  SetState(SamplingState::Reading);
  return zx::ok(sampler::ReadToken{});
}

void sampler::ThreadSampler::FinishRead(sampler::ReadToken&& token) {
  Guard<Mutex> guard(ThreadSamplerLock::Get());
  SamplingState state = State();
  DEBUG_ASSERT(state == SamplingState::Reading || state == SamplingState::Destroying);
  if (state == SamplingState::Destroying) {
    // The dispatcher was closed while we held a lock doing the read. We were reading, so we delayed
    // cleaning the buffers as to not corrupt the read. However, now we're responsible for the
    // remaining buffer clean up.
    for (sampler::internal::PerCpuState& pcs : per_cpu_state_) {
      pcs.Drain();
    }
    SetState(SamplingState::Allocated);
  } else {
    // No additional action is needed.
    SetState(SamplingState::Configured);
  }
  token.disarmed = true;
}

ktl::pair<zx_status_t, size_t> sampler::ThreadSampler::ReadUser(const sampler::ReadToken&,
                                                                user_out_ptr<void> ptr,
                                                                size_t len) {
  // We're going to call into VmObject::ReadUser which could be copying to pager backed user memory.
  // We can't be holding any locks.
  lockdep::AssertNoLocksHeld();

  const size_t num_buffers = per_cpu_state_.size();
  // All buffers are the same size.
  const size_t buffer_size = per_cpu_state_[0].BufferSize();

  // The caller can query the required buffer size by passing in a nulltpr.
  if (!ptr) {
    return {ZX_OK, buffer_size * num_buffers};
  }

  // If the per-CPU buffers have not been initialized, there's nothing to do, so return early.
  if (!per_cpu_state_) {
    return {ZX_OK, 0};
  }

  // Eventually, this should support users passing in buffers smaller than the sum of the size of
  // all per-CPU buffers, but for now we do not allow this.
  if (len < (buffer_size * num_buffers)) {
    return {ZX_ERR_INVALID_ARGS, 0};
  }

  // Iterate through each per-CPU buffer and read its contents.
  size_t bytes_read = 0;
  user_out_ptr<ktl::byte> byte_ptr = ptr.reinterpret<ktl::byte>();

  auto copy_fn = [&](uint32_t byte_offset, ktl::span<ktl::byte> src) {
    // This is safe to do while holding the lock_ because the KTrace lock is a leaf lock that is
    // not acquired during the course of a page fault.
    zx_status_t status = ZX_ERR_BAD_STATE;
    // Compute the destination address for this segment.
    user_out_ptr out_ptr = byte_ptr.byte_offset(bytes_read + byte_offset);

    // Copy the trace data to the user segment.
    status = out_ptr.copy_array_to_user(src.data(), src.size());
    return status;
  };

  for (uint32_t i = 0; i < num_buffers; i++) {
    const zx::result<size_t> result = per_cpu_state_[i].Read(copy_fn, static_cast<uint32_t>(len));
    if (result.is_error()) {
      // If we copied some data from a previous buffer, we have to return the fact that we did so
      // here. Otherwise, that data will be lost.
      return {result.status_value(), bytes_read};
    }
    bytes_read += result.value();
  }
  return {ZX_OK, bytes_read};
}
