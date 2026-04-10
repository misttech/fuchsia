// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/thread_sampler/thread_sampler.h>

#include <object/sampler_dispatcher.h>

void SamplerDispatcher::on_zero_handles() {
  if (zx::result res = sampler::gThreadSampler.Destroy(); res.is_error()) {
    dprintf(ALWAYS, "Failed to cleanly destroy sampler: %d\n", res.status_value());
  }
}

zx::result<KernelHandle<SamplerDispatcher>> SamplerDispatcher::Create(
    const zx_sampler_config_t& config) {
  // Set up the global sampler if it hasn't been set up yet.
  zx::result res = sampler::gThreadSampler.SetUp(config);
  if (res.is_error()) {
    return res.take_error();
  }

  fbl::AllocChecker ac;
  KernelHandle sampler(fbl::AdoptRef(new (&ac) SamplerDispatcher));
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  return zx::ok(ktl::move(sampler));
}

zx::result<> SamplerDispatcher::Stop() { return sampler::gThreadSampler.Stop(); }

zx::result<> SamplerDispatcher::Start() { return sampler::gThreadSampler.Start(); }

zx::result<> SamplerDispatcher::SampleThread(zx_koid_t pid, zx_koid_t tid, GeneralRegsSource source,
                                             const void* gregs) {
  return sampler::gThreadSampler.SampleThread(pid, tid, source, gregs);
}

ktl::pair<zx_status_t, size_t> SamplerDispatcher::ReadUser(user_out_ptr<void> ptr, size_t len) {
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
  //    2) If on_zero_handles is triggered while in `Reading` mode, we delay actually
  //       destroying the buffers and destroy them after the copy is completed instead.
  zx::result<sampler::ReadToken> token = sampler::gThreadSampler.PrepareRead();
  if (token.is_error()) {
    return {token.error_value(), 0};
  }

  auto [status, read] = sampler::gThreadSampler.ReadUser(*token, ptr, len);

  // We now need to ensure that the user side handle hasn't been dropped. If it has been, then
  // we need to clean it up.
  sampler::gThreadSampler.FinishRead(ktl::move(*token));
  return {status, read};
}
