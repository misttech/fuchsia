// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/core-test-utils.h>
#include <lib/sync/completion.h>
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
TEST(ClockTest, MultithreadedContentionClockMonotonic) {
  // TODO(https://fxbug.dev/498597096): Skip this test on Cavium for now.
  // Cavium hardware seems to show non-monotonic multithreaded system timer
  // behavior which is suspected to be related to its multi-socket nature (and
  // the fact that it is an abandoned beta-quality ARM server platform).
  //
  // Unless we can someday get to a root cause and a fix, we simply cannot trust
  // Cavium HW when it comes to clocks, and need to skip this test.
  if (std::optional<std::string_view> skip = core_test_utils::SkipBug363254896(); skip) {
    ZXTEST_SKIP(*skip);
  }

  // Parameters comparable to those used in threadtest.c from Linux kselftest.
  constexpr int kNumThreads = 8;
  constexpr int kRuntimeSeconds = 5;
  constexpr int kListSize = 100;

  // Shared context for each of the test threads.  Each thread is going to:
  //
  // 1) Lock.
  // 2) Make a clock observation.
  // 3) Add that observation to a vector of observations.
  // 4) If the vector is full, validate the vector.
  // 5) Unlock.
  //
  // A done flag will be observed inside of the lock looking for the signal from
  // the control thread to stop making observations.  If a failure is detected,
  // a libsync completion is used to signal the control thread that the test
  // should shutdown early.
  //
  struct {
    std::mutex mut;
    sync_completion_t failed;
    TA_GUARDED(mut) std::vector<zx::time> timestamps;
    TA_GUARDED(mut) bool done = false;

    void TestThread() {
      while (true) {
        std::lock_guard<std::mutex> lock(mut);

        if (done) {
          break;
        }

        timestamps.push_back(zx::clock::get_monotonic());
        // Once we collect enough values, check the values, clear the list and repeat.
        // Note that the last batch of timestamps is not checked, but that might
        // not be a big deal for detection.
        //
        // TODO(johngro): The purpose of this array is not clear to me.  It
        // seems simpler to simply maintain a "last observation" field and
        // constantly be checking to make sure that the current observation is
        // >= to the last observation.  The purpose of batching up a lot of
        // observations before verifying them is not entirely clear.
        //
        // When the test was originally written, one of the stated goals was to
        // maintain parity with a different test which came from somewhere else.
        // If maintaining this precise level of parity is not needed to
        // reproduce a failure, it would be a good idea to eliminate the vector
        // and simplify this test.
        //
        if (unlikely(timestamps.size() >= kListSize)) {
          static_assert(kListSize > 1);
          for (size_t i = 0; i < timestamps.size() - 1; ++i) {
            const zx::time sooner = timestamps[i];
            const zx::time later = timestamps[i + 1];
            EXPECT_LE(sooner.get(), later.get());
            if (CURRENT_TEST_HAS_FAILURES()) {
              sync_completion_signal(&failed);
              break;
            }
          }
          // Keep the last element so the next iterations could check monotonicity
          // across kListSize boundary.
          const zx::time last = timestamps.back();
          timestamps.clear();
          timestamps.push_back(last);
        }
      }  // while
    }

  } state;

  // Reserve space in our vector before starting the test threads.
  {
    std::lock_guard<std::mutex> lock(state.mut);
    state.timestamps.reserve(kListSize);
  }

  // Now create all of our threads and point them our test method.
  std::vector<std::thread> threads;
  threads.reserve(kNumThreads);
  for (int t = 0; t < kNumThreads; ++t) {
    threads.emplace_back([&state]() { state.TestThread(); });
  }

  // Wait until the test runtime has elapsed, or a failure is detected,
  // whichever comes first.  Then tell the threads to exit.
  sync_completion_wait(&state.failed, ZX_SEC(kRuntimeSeconds));
  {
    std::lock_guard<std::mutex> lock(state.mut);
    state.done = true;
  }

  // Wait for the test threads to finish exiting and we are done.
  for (auto& thread : threads) {
    thread.join();
  }
}

}  // namespace
