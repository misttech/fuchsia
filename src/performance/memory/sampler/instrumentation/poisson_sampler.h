// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_PERFORMANCE_MEMORY_SAMPLER_INSTRUMENTATION_POISSON_SAMPLER_H_
#define SRC_PERFORMANCE_MEMORY_SAMPLER_INSTRUMENTATION_POISSON_SAMPLER_H_

#include <cstddef>
#include <cstdint>
#include <memory>
#include <optional>
#include <utility>

namespace memory_sampler {
// Helper class to implement a Poisson point process.
//
// The Poisson point process is a commonly used sampling strategy:
// we consider the line of allocated bytes over time, and we select
// bytes following a Poisson distribution. When a byte has been
// selected, we record its entire allocation (and the corresponding
// deallocation).
//
// This strategy has desirable properties:
//
//   * It skews profiles towards large allocations, which are more
//   likely to be problematic or interesting.
//
//   * It introduces some amount of randomness, to prevent aliasing
//   issues (i.e. an allocation pattern that significantly distorts
//   the view presented by the profile).
//
//   * It is memoryless (in the sense that computing sampled bytes
//   does not rely on the history of previously sampled bytes).
//
//   * We can define a sampling rate in terms of average allocated
//   memory between samples.
//
//   * There is a rather straightforward way of generating
//   inter-arrival intervals via the inverse CDF method, making it
//   rather straightforward to implement.
class PoissonSampler {
 public:
  class SampleIntervalGenerator {
   public:
    // Compute a suitable interval of seen bytes between samples.
    virtual size_t GetNextSampleInterval(uint64_t mean_interval) = 0;
    virtual ~SampleIntervalGenerator() = default;
  };

  // Provides the mean number of bytes between samples, which may vary over
  // time.
  using MeanIntervalProvider = uint64_t (*)();

  // Provide custom sample interval generator.
  explicit PoissonSampler(MeanIntervalProvider get_mean_interval,
                          std::unique_ptr<SampleIntervalGenerator> sample_interval_generator)
      : get_mean_interval_(get_mean_interval),
        sample_interval_generator_(std::move(sample_interval_generator)) {}

  // Use provided default Poisson point process appropriate sample
  // interval generator.
  explicit PoissonSampler(MeanIntervalProvider get_mean_interval);

  // Given the size of an allocation, and the current state of the sampler,
  // decide whether to capture a sample. Returns the sampling interval if the
  // allocation should be sampled; returns `std::nullopt` otherwise.
  //
  // Note: calling this function progresses the Poisson point process,
  // which is effectful.
  std::optional<uint64_t> ShouldSampleAllocation(size_t size);

 private:
  size_t bytes_before_next_sample_ = 0;
  bool sampling_interval_initialized_ = false;
  uint64_t interval_used_for_next_sample_ = 0;
  MeanIntervalProvider get_mean_interval_;
  std::unique_ptr<SampleIntervalGenerator> sample_interval_generator_;
};
}  // namespace memory_sampler

#endif  // SRC_PERFORMANCE_MEMORY_SAMPLER_INSTRUMENTATION_POISSON_SAMPLER_H_
