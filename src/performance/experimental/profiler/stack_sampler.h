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
};

}  // namespace profiler

#endif  // SRC_PERFORMANCE_EXPERIMENTAL_PROFILER_STACK_SAMPLER_H_
