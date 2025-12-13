// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_PERFORMANCE_EXPERIMENTAL_PROFILER_STACK_SAMPLER_H_
#define SRC_PERFORMANCE_EXPERIMENTAL_PROFILER_STACK_SAMPLER_H_

#include "sampler.h"

namespace profiler {

class StackSampler : public Sampler {
 public:
  explicit StackSampler(async_dispatcher_t* dispatcher, TargetTree&& targets,
                        std::vector<fuchsia_cpu_profiler::SamplingConfig> sampling_specs)
      : Sampler(dispatcher, std::move(targets), std::move(sampling_specs)) {}

  zx::result<> Start(size_t buffer_size_mb) override;
  zx::result<> Stop() override;

 private:
  void AddThread(std::vector<zx_koid_t> job_path, zx_koid_t pid, zx_koid_t tid,
                 zx::thread t) override;
  void CollectSamples(async_dispatcher_t* dispatcher, async::TaskBase* task, zx_status_t status);
  async::TaskMethod<profiler::StackSampler, &profiler::StackSampler::CollectSamples> sample_task_{
      this};
};

}  // namespace profiler

#endif  // SRC_PERFORMANCE_EXPERIMENTAL_PROFILER_STACK_SAMPLER_H_
