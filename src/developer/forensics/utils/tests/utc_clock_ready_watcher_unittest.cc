// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/utils/utc_clock_ready_watcher.h"

#include <fuchsia/time/cpp/fidl.h>

#include <memory>

#include <gtest/gtest.h>

#include "src/developer/forensics/testing/unit_test_fixture.h"
#include "src/lib/timekeeper/test_clock.h"

namespace forensics {
namespace {

constexpr timekeeper::time_utc kTime((zx::hour(7) + zx::min(14) + zx::sec(52)).get());

class UtcClockReadyWatcherTest : public UnitTestFixture {
 public:
  UtcClockReadyWatcherTest() {
    clock_.SetUtc(kTime);

    zx_clock_create_args_v1_t clock_args{.backstop_time = 0};
    FX_CHECK(zx::clock::create(0u, &clock_args, &clock_handle_) == ZX_OK);

    utc_clock_ready_watcher_ = std::make_unique<UtcClockReadyWatcher>(
        dispatcher(), zx::unowned_clock(clock_handle_.get_handle()));
  }

 protected:
  void SignalLoggingQualityClock() {
    if (const zx_status_t status =
            clock_handle_.signal(/*clear_mask=*/0,
                                 /*set_mask=*/fuchsia::time::SIGNAL_UTC_CLOCK_LOGGING_QUALITY);
        status != ZX_OK) {
      FX_PLOGS(FATAL, status) << "Failed to achieve logging quality clock";
    }
  }

  timekeeper::TestClock clock_;
  zx::clock clock_handle_;

  std::unique_ptr<UtcClockReadyWatcher> utc_clock_ready_watcher_;
};

TEST_F(UtcClockReadyWatcherTest, Check_LoggingQualityClock) {
  bool logging_quality_clock{false};

  utc_clock_ready_watcher_->OnClockReady([&] { logging_quality_clock = true; });
  ASSERT_FALSE(logging_quality_clock);

  SignalLoggingQualityClock();
  RunLoopUntilIdle();

  ASSERT_TRUE(logging_quality_clock);
}

TEST_F(UtcClockReadyWatcherTest, Check_ClockPreviouslyLoggingQuality) {
  bool logging_quality_clock{false};
  SignalLoggingQualityClock();
  RunLoopUntilIdle();

  utc_clock_ready_watcher_->OnClockReady([&] { logging_quality_clock = true; });
  ASSERT_TRUE(logging_quality_clock);
}

TEST_F(UtcClockReadyWatcherTest, Check_ClockNeverLoggingQuality) {
  bool logging_quality_clock{false};

  utc_clock_ready_watcher_->OnClockReady([&] { logging_quality_clock = true; });
  ASSERT_FALSE(logging_quality_clock);

  for (size_t i = 0; i < 100; ++i) {
    RunLoopFor(zx::hour(23));
    EXPECT_FALSE(logging_quality_clock);
  }
}

TEST_F(UtcClockReadyWatcherTest, Check_NotReadyOnClockStart) {
  bool clock_started{false};

  utc_clock_ready_watcher_->OnClockReady([&] { clock_started = true; });
  ASSERT_FALSE(clock_started);

  ASSERT_EQ(
      clock_handle_.update(zx::clock::update_args().set_value(zx::time_monotonic(kTime.get()))),
      ZX_OK);
  RunLoopUntilIdle();

  EXPECT_FALSE(clock_started);
}

TEST_F(UtcClockReadyWatcherTest, Check_NotReadyOnClockSynchronized) {
  bool clock_synced{false};

  utc_clock_ready_watcher_->OnClockReady([&] { clock_synced = true; });
  ASSERT_FALSE(clock_synced);

  ASSERT_EQ(clock_handle_.signal(
                /*clear_mask=*/0, /*set_mask=*/fuchsia::time::SIGNAL_UTC_CLOCK_SYNCHRONIZED),
            ZX_OK);
  RunLoopUntilIdle();

  EXPECT_FALSE(clock_synced);
}

}  // namespace
}  // namespace forensics
