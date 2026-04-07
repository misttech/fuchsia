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

KernelHandle<ThreadSamplerDispatcher> ThreadSamplerDispatcher::gThreadSampler_;

zx::result<> ThreadSamplerDispatcher::CreateImpl(
    const zx_sampler_config_t& config, KernelHandle<ThreadSamplerDispatcher>& read_handle,
    KernelHandle<ThreadSamplerDispatcher>& write_handle) {
  const size_t num_cpus = percpu::processor_count();

  fbl::AllocChecker ac;
  // Start by creating the buffer, a la IoBufferDispatcher::Create
  auto holder0 = fbl::MakeRefCountedChecked<PeerHolder<IoBufferDispatcher>>(&ac);
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  auto holder1 = holder0;

  fbl::RefPtr<SharedIobState> shared_regions = fbl::AdoptRef(new (&ac) SharedIobState{
      .regions = nullptr,
  });

  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  KernelHandle write_dispatcher{fbl::AdoptRef(
      new (&ac) ThreadSamplerDispatcher(ktl::move(holder0), IobEndpointId::Ep0, shared_regions))};
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  KernelHandle read_dispatcher{fbl::AdoptRef(
      new (&ac) ThreadSamplerDispatcher(ktl::move(holder1), IobEndpointId::Ep1, shared_regions))};
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  read_dispatcher.dispatcher()->InitPeer(write_dispatcher.dispatcher());
  write_dispatcher.dispatcher()->InitPeer(read_dispatcher.dispatcher());

  write_dispatcher.dispatcher()->per_cpu_state_ =
      fbl::MakeArray<sampler::internal::PerCpuState>(&ac, num_cpus);
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  // Even though the buffer is per_cpu, we are fine to set up each cpu state here on a single cpu.
  // When we start sampling, we call mp_sync_exec which will synchronize the written
  // per_cpu_states.
  for (unsigned i = 0; i < num_cpus; i++) {
    if (zx::result<> setup_result =
            write_dispatcher.dispatcher()->per_cpu_state_[i].SetUp(config, i);
        setup_result.is_error()) {
      return setup_result.take_error();
    }
  }

  read_handle = ktl::move(read_dispatcher);
  write_handle = ktl::move(write_dispatcher);
  return zx::ok();
}

zx::result<> ThreadSamplerDispatcher::StartImpl() TA_EXCL(get_lock()) {
  Guard<CriticalMutex> guard(get_lock());
  if (state_ != SamplingState::Configured) {
    return zx::error(ZX_ERR_BAD_STATE);
  }

  DEBUG_ASSERT(!per_cpu_state_.empty());
  DEBUG_ASSERT(GetEndpointId() == IobEndpointId::Ep0);
  for (sampler::internal::PerCpuState& state : per_cpu_state_) {
    state.EnableWrites();
  }

  mp_sync_exec(
      mp_ipi_target::ALL, 0,
      [](void* s) { reinterpret_cast<ThreadSamplerDispatcher*>(s)->SetCurrCpuTimer(); }, this);
  state_ = SamplingState::Running;
  return zx::ok();
}

zx::result<> ThreadSamplerDispatcher::StopImpl() TA_EXCL(get_lock()) {
  Guard<CriticalMutex> guard(get_lock());
  DEBUG_ASSERT(GetEndpointId() == IobEndpointId::Ep0);
  if (state_ != SamplingState::Running) {
    return zx::error(ZX_ERR_BAD_STATE);
  }
  StopLocked();
  return zx::ok();
}

void ThreadSamplerDispatcher::StopLocked() TA_REQ(get_lock()) {
  DEBUG_ASSERT(GetEndpointId() == IobEndpointId::Ep0);

  for (sampler::internal::PerCpuState& state : per_cpu_state_) {
    state.DisableWrites();
    state.CancelTimer();
  }

  // Some timers may not have not been able to be canceled, so we need to wait for any samples that
  // have already started to finish.
  zx_instant_mono_t deadline = zx_time_add_duration(current_mono_time(), ZX_SEC(30));
  for (const sampler::internal::PerCpuState& i : per_cpu_state_) {
    bool pending_timers;
    bool pending_writes;
    do {
      pending_timers = i.PendingTimer();
      pending_writes = i.PendingWrites();
      if (pending_timers || pending_writes) {
        Thread::Current::SleepRelative(ZX_MSEC(1));
      }
    } while ((pending_writes || pending_timers) && (current_mono_time() < deadline));
    // We'll wait an unreasonable amount of time for the timer to finish. If the timer really
    // haven't finished by this point, something has gone terribly wrong.
    ZX_ASSERT(!pending_writes || !pending_timers);
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
  state_ = SamplingState::Configured;
}

zx::result<> ThreadSamplerDispatcher::SampleThreadImpl(zx_koid_t pid, zx_koid_t tid,
                                                       GeneralRegsSource source, void* gregs) {
  DEBUG_ASSERT(GetEndpointId() == IobEndpointId::Ep0);
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
      fp = reinterpret_cast<iframe_t*>(gregs)->rbp;
      pc = reinterpret_cast<iframe_t*>(gregs)->ip;
#endif
#ifdef __aarch64__
      bt[frame_num++] = (reinterpret_cast<iframe_t*>(gregs)->elr) - 4;
      fp = reinterpret_cast<iframe_t*>(gregs)->r[29];
      pc = (reinterpret_cast<iframe_t*>(gregs)->lr) - 4;
#endif
#ifdef __riscv
      fp = reinterpret_cast<iframe_t*>(gregs)->regs.s0;
      pc = reinterpret_cast<iframe_t*>(gregs)->regs.pc;
#endif
      break;
#ifdef __x86_64__
    case GeneralRegsSource::Syscall:
      fp = reinterpret_cast<syscall_regs_t*>(gregs)->rbp;
      pc = reinterpret_cast<syscall_regs_t*>(gregs)->rip;
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

void ThreadSamplerDispatcher::OnPeerZeroHandlesLocked() {
  DEBUG_ASSERT(GetEndpointId() == IobEndpointId::Ep0);

  // We purposely don't emit a call to IoBufferDispatcher::OnPeerZeroHandlesLocked() here. It's used
  // to coordinate and delay ZX_IOB_PEER_CLOSED until any mapped regions have been unmapped. We
  // don't need the logic here. Userspace will never see a ZX_IOB_PEER_CLOSED as we will not close
  // the endpoint the kernel holds until after userspace closes the last handle to their endpoint.
  // When that happens, we end up here and are going to destroy our state anyways.
  if (state_ == SamplingState::Reading) {
    // There's a read in flight, we can't destroy our buffers yet. We set the state to Destroying,
    // and when the read finishes, it will also clean up the buffers.
    state_ = SamplingState::Destroying;
  }

  // The userspace end of the iobuffer has closed. Time to clean up our state
  if (state_ == SamplingState::Running) {
    StopLocked();
  }

  // After StopLocked, we have prevented further threads from accessing the per_cpu_states, and then
  // waited for any threads that were accessing the states to finish.
  //
  // It's now safe to destroy our cpu states. This will destroy the mappings and pinnings that the
  // kernel keeps to write to, but if userspace has their own mappings, those will remain continue
  // to remain valid.
  per_cpu_state_.reset();
  state_ = SamplingState::Destroyed;
}

void ThreadSamplerDispatcher::SetCurrCpuTimer() { GetPerCpuState(arch_curr_cpu_num()).SetTimer(); }

zx::result<KernelHandle<ThreadSamplerDispatcher>> ThreadSamplerDispatcher::Create(
    const zx_sampler_config_t& config) {
  {
    Guard<Mutex> guard(ThreadSamplerLock::Get());
    if (gThreadSampler_.dispatcher() != nullptr &&
        gThreadSampler_.dispatcher()->State() !=
            ThreadSamplerDispatcher::SamplingState::Destroyed) {
      return zx::error(ZX_ERR_ALREADY_EXISTS);
    }
  }

  KernelHandle<ThreadSamplerDispatcher> write_handle;
  KernelHandle<ThreadSamplerDispatcher> read_handle;
  zx::result res = ThreadSamplerDispatcher::CreateImpl(config, read_handle, write_handle);
  if (res.is_error()) {
    return res.take_error();
  }

  {
    Guard<Mutex> guard(ThreadSamplerLock::Get());
    // Ensure that someone hasn't created a new sampler since we created ours
    if ((gThreadSampler_.dispatcher() != nullptr &&
         gThreadSampler_.dispatcher()->State() !=
             ThreadSamplerDispatcher::SamplingState::Destroyed)) {
      return zx::error(ZX_ERR_ALREADY_EXISTS);
    }
    gThreadSampler_ = ktl::move(write_handle);
  }

  return zx::ok(ktl::move(read_handle));
}

zx::result<> ThreadSamplerDispatcher::Stop(const fbl::RefPtr<IoBufferDispatcher>& disp) {
  Guard<Mutex> guard(ThreadSamplerLock::Get());
  if (gThreadSampler_.dispatcher() == nullptr) {
    return zx::error(ZX_ERR_BAD_STATE);
  }
  if (disp->get_koid() != gThreadSampler_.dispatcher()->get_related_koid()) {
    return zx::error(ZX_ERR_BAD_HANDLE);
  }
  return gThreadSampler_.dispatcher()->StopImpl();
}

zx::result<> ThreadSamplerDispatcher::Start(const fbl::RefPtr<IoBufferDispatcher>& disp) {
  Guard<Mutex> guard(ThreadSamplerLock::Get());
  if (gThreadSampler_.dispatcher() == nullptr) {
    return zx::error(ZX_ERR_BAD_STATE);
  }
  if (disp->get_koid() != gThreadSampler_.dispatcher()->get_related_koid()) {
    return zx::error(ZX_ERR_BAD_HANDLE);
  }
  return gThreadSampler_.dispatcher()->StartImpl();
}

zx::result<> ThreadSamplerDispatcher::SampleThread(zx_koid_t pid, zx_koid_t tid,
                                                   GeneralRegsSource source, void* gregs) {
  fbl::RefPtr<ThreadSamplerDispatcher> sampler_ref;
  {
    // We hold the global ThreadSamplerLock only long enough to increase the reference count on
    // the current sampler if it exists.
    //
    // The sampling is relatively slow and we don't want to prevent other threads on a different
    // core from making progress.
    //
    // Once we release the ThreadSamplerLock, it may be that the global sampler is replaced with a
    // new one, however the ThreadSamplerDispatcher maintains enough state that the SampleThread
    // and SetCurrCpuTimers will simply short circuit and return early if that is the case.
    Guard<Mutex> guard(ThreadSamplerLock::Get());
    if (gThreadSampler_.dispatcher() == nullptr ||
        gThreadSampler_.dispatcher()->State() != ThreadSamplerDispatcher::SamplingState::Running) {
      return zx::error(ZX_ERR_BAD_STATE);
    }
    sampler_ref = gThreadSampler_.dispatcher();
  }

  return sampler_ref->SampleThreadImpl(pid, tid, source, gregs);
}

ktl::pair<zx_status_t, size_t> ThreadSamplerDispatcher::ReadUser(
    const fbl::RefPtr<IoBufferDispatcher>& disp, user_out_ptr<void> ptr, size_t len) {
  // We unfortunately run into some complexity here: while the buffer our samples in is created by
  // the kernel and is safe to read from, the user memory we are writing to could be pager-backed.
  // This means that when we attempt to write to it as part of the VmObjectPaged::ReadUser call, we
  // cannot be holding locks. So we need to obtain the lock to set up the copy, drop the lock, do
  // the copy, then grab the lock again to make sure everything went well.
  //
  // During the copy, we'd need to prevent:
  //   1) The sampler from writing new data
  //   2) The buffers being destroyed due to the read handle being zx_handle_close'd
  //   3) A new sampler from being created.
  //
  // We do this by:
  //    1) Setting our state to SamplingState::Reading which disallows starting a new session (and
  //       thus destroying the old one).
  //    2) If OnPeerZeroHandlesLocked is triggered while in `Reading` mode, we delay actually
  //       destroying the buffers and destroy them after the copy is completed instead.

  fbl::RefPtr<ThreadSamplerDispatcher> sampler_ref;
  ktl::optional<sampler::ReadToken> token;
  {
    Guard<Mutex> guard(ThreadSamplerLock::Get());
    // For now, as we don't yet support concurrent reads and writes, we must not be in an
    // active session when we are reading data.
    if (gThreadSampler_.dispatcher() == nullptr ||
        gThreadSampler_.dispatcher()->State() !=
            ThreadSamplerDispatcher::SamplingState::Configured) {
      return {ZX_ERR_BAD_STATE, 0};
    }

    if (disp->get_koid() != gThreadSampler_.dispatcher()->get_related_koid()) {
      return {ZX_ERR_BAD_HANDLE, 0};
    }
    sampler_ref = gThreadSampler_.dispatcher();
    zx::result<sampler::ReadToken> prepare_result = sampler_ref->PrepareRead();
    if (prepare_result.is_error()) {
      return {prepare_result.error_value(), 0};
    }
    token.emplace(ktl::move(prepare_result.value()));
  }

  auto [status, read] = sampler_ref->ReadUserImpl(token.value(), ptr, len);
  {
    // We now need to ensure that the user side handle hasn't been dropped. If it has been, then we
    // need to clean it up.
    Guard<Mutex> guard(ThreadSamplerLock::Get());
    DEBUG_ASSERT(gThreadSampler_.dispatcher() != nullptr);
    gThreadSampler_.dispatcher()->FinishRead(ktl::move(token.value()));
  }

  return {status, read};
}

zx::result<sampler::ReadToken> ThreadSamplerDispatcher::PrepareRead() {
  Guard<CriticalMutex> guard(get_lock());
  if (state_ != SamplingState::Configured) {
    return zx::error(ZX_ERR_BAD_STATE);
  }
  state_ = SamplingState::Reading;
  return zx::ok(sampler::ReadToken{});
}

void ThreadSamplerDispatcher::FinishRead(sampler::ReadToken&& token) {
  Guard<CriticalMutex> guard(get_lock());
  DEBUG_ASSERT(state_ == SamplingState::Reading || state_ == SamplingState::Destroying);
  if (state_ == SamplingState::Destroying) {
    // Our peer was destroyed while we dropped to the lock to do the read. We were reading, so we
    // delayed destroying the buffers as to not corrupt the read. However, now we're responsible for
    // the remaining buffer clean up.
    per_cpu_state_.reset();
    state_ = SamplingState::Destroyed;
  } else {
    // No additional action is needed.
    state_ = SamplingState::Configured;
  }
  token.disarmed = true;
}

ktl::pair<zx_status_t, size_t> ThreadSamplerDispatcher::ReadUserImpl(const sampler::ReadToken&,
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
