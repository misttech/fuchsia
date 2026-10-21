// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <cstdint>

#include <gtest/gtest.h>
#include <src/performance/memory/sampler/instrumentation/poisson_sampler.h>

namespace memory_sampler {
namespace {
constexpr size_t kHighSamplingRateInterval = 1;
constexpr size_t kLowSamplingRateInterval = 1000;
constexpr size_t kSmallAllocation = 1;
constexpr size_t kLargeAllocation = 1000;
constexpr size_t kMediumAllocation = 100;

TEST(PoissonSamplerTest, SampleLargeAllocation) {
  class TestSampleIntervalGenerator : public PoissonSampler::SampleIntervalGenerator {
   public:
    size_t GetNextSampleInterval(uint64_t) override { return kHighSamplingRateInterval; }
  };

  // Create a sampler with a high sampling rate.
  PoissonSampler sampler{[] -> uint64_t { return kHighSamplingRateInterval; },
                         std::make_unique<TestSampleIntervalGenerator>()};

  // Query the sampler with a large allocation.
  EXPECT_TRUE(sampler.ShouldSampleAllocation(kLargeAllocation).has_value());
}

TEST(PoissonSamplerTest, DontSampleSmallAllocation) {
  class TestSampleIntervalGenerator : public PoissonSampler::SampleIntervalGenerator {
   public:
    size_t GetNextSampleInterval(uint64_t) override { return kLowSamplingRateInterval; }
  };
  // Create a sampler with a low sampling rate.
  PoissonSampler sampler{[] -> uint64_t { return kLowSamplingRateInterval; },
                         std::make_unique<TestSampleIntervalGenerator>()};

  // Query the sampler with a small allocation.
  EXPECT_FALSE(sampler.ShouldSampleAllocation(kSmallAllocation).has_value());
}

TEST(PoissonSamplerTest, SampleProbabilityIncreases) {
  class TestSampleIntervalGenerator : public PoissonSampler::SampleIntervalGenerator {
   public:
    size_t GetNextSampleInterval(uint64_t) override { return kLowSamplingRateInterval; }
  };
  // Create a sampler with a low sampling rate.
  PoissonSampler sampler{[] -> uint64_t { return kLowSamplingRateInterval; },
                         std::make_unique<TestSampleIntervalGenerator>()};

  // Query the sampler with small allocations until it samples.
  size_t allocated_bytes = 0;
  while (allocated_bytes < kLowSamplingRateInterval - kMediumAllocation) {
    EXPECT_FALSE(sampler.ShouldSampleAllocation(kMediumAllocation).has_value());
    allocated_bytes += kMediumAllocation;
  }

  EXPECT_TRUE(sampler.ShouldSampleAllocation(kMediumAllocation).has_value());
}

TEST(PoissonSamplerTest, ReturnActualMeanIntervalUsed) {
  class TestSampleIntervalGenerator : public PoissonSampler::SampleIntervalGenerator {
   public:
    size_t GetNextSampleInterval(uint64_t interval) override { return interval; }
  };

  // The provider must be captureless to convert to `MeanIntervalProvider`, so
  // the interval this test varies lives in a static. It is reassigned on entry
  // rather than only initialized, so the test does not depend on being run at
  // most once per process.
  thread_local uint64_t dynamic_interval;
  dynamic_interval = 500;
  PoissonSampler sampler{[] -> uint64_t { return dynamic_interval; },
                         std::make_unique<TestSampleIntervalGenerator>()};

  // Initialize sampler with dynamic_interval = 500.
  // 500 - 100 = 400 bytes remaining.
  EXPECT_FALSE(sampler.ShouldSampleAllocation(100).has_value());

  // Before next sample is triggered, change dynamic interval to 1000.
  dynamic_interval = 1000;

  // The allocation of 500 bytes will trigger the sample that was initialized with 500.
  auto result = sampler.ShouldSampleAllocation(500);
  ASSERT_TRUE(result.has_value());
  EXPECT_EQ(result.value(), 500u);

  // Spillover was 500 - 400 = 100 bytes.
  // Next interval generated was 1000.
  // Remaining bytes before next sample: 1000 - 100 = 900.
  // Change dynamic interval to 2000.
  dynamic_interval = 2000;

  // 1000 bytes will trigger the next sample (needs 900).
  result = sampler.ShouldSampleAllocation(1000);
  ASSERT_TRUE(result.has_value());
  EXPECT_EQ(result.value(), 1000u);
}
}  // namespace
}  // namespace memory_sampler
