// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/zircon-internal/thread_annotations.h>
#include <lib/zx/clock.h>
#include <lib/zx/time.h>

#include <array>
#include <chrono>
#include <cstddef>
#include <mutex>
#include <thread>
#include <vector>

#include <zxtest/zxtest.h>

namespace {

constexpr zx::duration k1MsDelay = zx::msec(1);

TEST(ClockTest, ClockMonotonic) {
  const zx::time zero;
  zx::time previous = zx::clock::get_monotonic();

  for (int idx = 0; idx < 100; ++idx) {
    zx::time current = zx::clock::get_monotonic();
    ASSERT_GT(current, zero, "monotonic time should be a positive number of nanoseconds");
    ASSERT_GE(current, previous, "monotonic time should only advance");
    // This calls zx_nanosleep directly rather than using
    // zx_deadline_after, which internally gets the monotonic
    // clock.
    zx::nanosleep(current + k1MsDelay);

    previous = current;
  }
}

TEST(ClockTest, DeadlineAfter) {
  constexpr std::array Offsets = {ZX_MSEC(0), ZX_MSEC(20)};

  // Make sure that zx_deadline_after always gives results which are consistent
  // with simply getting clock monotonic and adding our own offset.
  for (auto offset : Offsets) {
    zx_instant_mono_t before, after, deadline;

    before = zx_time_add_duration(zx_clock_get_monotonic(), offset);
    deadline = zx_deadline_after(offset);
    after = zx_time_add_duration(zx_clock_get_monotonic(), offset);

    ASSERT_GE(deadline, before);
    ASSERT_LE(deadline, after);
  }
}

// Have multiple threads compete over putting monotonic clock readings into a
// shared list in a very hot loop over a long period of time. This test is
// similar to a Linux kselftest which exercises the monotonic time readings.
//
// TODO(https://fxbug.dev/498597096): Test is disabled because of failures.  See
// bug for details.
TEST(ClockTest, DISABLED_MultithreadedContentionClockMonotonic) {
  // Parameters comparable to those used in threadtest.c from Linux kselftest.
  constexpr int kNumThreads = 8;
  constexpr int kRuntimeSeconds = 30;
  constexpr int kListSize = 100;

  struct {
    std::mutex mut;
    std::vector<zx::time> TA_GUARDED(mut) timestamps;
    bool TA_GUARDED(mut) failed = false;
    bool TA_GUARDED(mut) done = false;
  } shared_items;

  {
    std::lock_guard<std::mutex> lock(shared_items.mut);
    shared_items.timestamps.reserve(kListSize);
  }

  std::vector<std::thread> threads;
  threads.reserve(kNumThreads + 1);
  for (int t = 0; t < kNumThreads; ++t) {
    threads.emplace_back([&]() {
      while (true) {
        std::lock_guard<std::mutex> lock(shared_items.mut);

        if (shared_items.done || shared_items.failed) {
          break;
        }

        shared_items.timestamps.push_back(zx::clock::get_monotonic());
        // Once we collect enough values, check the values, clear the list and repeat.
        // Note that the last batch of timestamps is not checked, but that might
        // not be a big deal for detection.
        if (unlikely(shared_items.timestamps.size() >= kListSize)) {
          static_assert(kListSize > 1);
          for (size_t i = 0; i < shared_items.timestamps.size() - 1; ++i) {
            const int64_t& sooner = shared_items.timestamps[i].get();
            const int64_t& later = shared_items.timestamps[i + 1].get();
            EXPECT_LE(sooner, later);
            if (sooner > later) {
              shared_items.failed = true;
              break;
            }
          }
          // Keep the last element so the next iterations could check monotonicity
          // across kListSize boundary.
          const zx::time last = shared_items.timestamps.back();
          shared_items.timestamps.clear();
          shared_items.timestamps.push_back(last);
        }
      }  // while
    });
  }

  threads.emplace_back([&] {
    // This thread doesn't seem to be bothered by contention, so I leave the
    // `done` flag flip as is.
    std::this_thread::sleep_for(std::chrono::seconds(kRuntimeSeconds));
    std::lock_guard<std::mutex> lock(shared_items.mut);
    shared_items.done = true;
  });

  for (auto& thread : threads) {
    thread.join();
  }
}

}  // namespace
