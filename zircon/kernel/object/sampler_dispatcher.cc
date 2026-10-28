// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/thread_sampler/thread_sampler.h>

#include <object/sampler_dispatcher.h>

extern "C" {
void rust_sampler_dispatcher_state_init(void* state, void* disp);
void rust_sampler_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_sampler_dispatcher_state_get_lock(const void* state);
void rust_sampler_dispatcher_on_zero_handles(const SamplerDispatcher* disp);
}  // extern "C"

SamplerDispatcher::SamplerDispatcher() : Dispatcher(0) {
  DISPATCHER_VERIFY_OFFSET(SamplerDispatcher, kSamplerDispatcherStateOffset);
  rust_sampler_dispatcher_state_init(&opaque_storage_, this);
}

IMPLEMENT_DISPATCHER_RUST_STATE(SamplerDispatcher, rust_sampler_dispatcher_state_get_lock,
                                rust_sampler_dispatcher_state_destroy)

void SamplerDispatcher::on_zero_handles() { rust_sampler_dispatcher_on_zero_handles(this); }

zx::result<> SamplerDispatcher::SampleThread(zx_koid_t pid, zx_koid_t tid, GeneralRegsSource source,
                                             const void* gregs, uint64_t session_id) {
  return sampler::ThreadSampler::SampleThread(pid, tid, source, gregs, session_id);
}

zx_status_t SamplerDispatcher::user_signal_self(uint32_t clear_mask, uint32_t set_mask) {
  return UserSignalSelfSolo(this, clear_mask, set_mask, 0);
}
