// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "poisson_sampler.h"

#include <algorithm>
#include <cmath>
#include <cstdint>
#include <random>

namespace {
class NonDeterministicSampleIntervalGenerator final
    : public memory_sampler::PoissonSampler::SampleIntervalGenerator {
 public:
  // We sample with a Poisson process, with constant average sampling
  // interval. This follows the exponential probability distribution with
  // parameter λ = 1/interval where |interval| is the average number of bytes
  // between samples.
  // Let u be a uniformly distributed random number (0,1], then
  // next_sample = -ln(u) / λ
  //
  // Note: This formula is derived from inverse transform sampling, by
  // computing the inverse cumulative distribution to get a random
  // number following an arbitrary distribution from a uniform random
  // number generator.
  size_t GetNextSampleInterval(uint64_t mean_interval) override {
    // UniformRandomDouble returns numbers [0,1). We use 1-uniform to
    // correct it to avoid a possible floating point exception from
    // taking the log of 0.
    double uniform = UniformRandomDouble();
    double value = -std::log(1 - uniform) * static_cast<double>(mean_interval);
    double min_value = 1.0;
    // We limit the upper bound of a sample interval to make sure we don't have
    // huge gaps in the sampling stream. Probability of the upper bound gets hit
    // is exp(-20) ~ 2e-9, so it should not skew the distribution.
    //
    // Note: a mean interval of 0 means "sample every non-zero allocation";
    // clamping the upper bound up to `min_value` keeps it well-formed, since
    // `std::clamp` requires its lower bound not to exceed its upper bound.
    double max_value = std::max(min_value, static_cast<double>(mean_interval) * 20.0);
    return static_cast<size_t>(std::clamp(value, min_value, max_value));
  }

 private:
  // Random engine used to run the uniform real distribution.
  //
  // Note: We use a hardware RNG only for seeding; we expect a PRNG to
  // be good enough for the lifetime of the recorder.
  std::default_random_engine generator_{std::random_device{}()};
  std::uniform_real_distribution<double> distribution_{0, 1.0};
  // Draw a uniform random double from [0, 1).
  double UniformRandomDouble() { return distribution_(generator_); }
};

}  // namespace

namespace memory_sampler {
PoissonSampler::PoissonSampler(MeanIntervalProvider get_mean_interval)
    : get_mean_interval_(get_mean_interval),
      sample_interval_generator_(std::make_unique<NonDeterministicSampleIntervalGenerator>()) {}

// The overall strategy to implement our Poisson process goes like
// this:
//
//   * Compute a sample interval.
//
//   * Keep track of the number of allocated bytes so far.
//
//   * Once this total has crossed the threshold defined by the
//   interval (which means that the "sampled byte" was part of this
//   allocation), compute the new threshold by getting a new interval,
//   then record this allocation.
std::optional<uint64_t> PoissonSampler::ShouldSampleAllocation(size_t size) {
  if (!sampling_interval_initialized_) {
    sampling_interval_initialized_ = true;
    interval_used_for_next_sample_ = get_mean_interval_();
    bytes_before_next_sample_ =
        sample_interval_generator_->GetNextSampleInterval(interval_used_for_next_sample_);
  }

  if (bytes_before_next_sample_ > size) {
    bytes_before_next_sample_ -= size;
    return std::nullopt;
  }

  uint64_t triggered_interval = interval_used_for_next_sample_;
  size_t spillover_bytes = size - bytes_before_next_sample_;
  size_t new_interval = 0;
  uint64_t current_mean = get_mean_interval_();
  interval_used_for_next_sample_ = current_mean;
  do {
    new_interval += sample_interval_generator_->GetNextSampleInterval(current_mean);
  } while (new_interval < spillover_bytes);

  bytes_before_next_sample_ = new_interval - spillover_bytes;
  return triggered_interval;
}

}  // namespace memory_sampler
