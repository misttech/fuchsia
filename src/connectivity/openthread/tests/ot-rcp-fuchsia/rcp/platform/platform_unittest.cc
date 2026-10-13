// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Unit tests for the ot-rcp-fuchsia OpenThread platform (platform.cc).
//
// The OpenThread core is not linked: the core functions the platform calls back into
// (otTaskletsProcess, otPlatAlarmMilliFired, otInstanceIsInitialized) are faked below, and time is
// driven by an async::TestLoop.

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/platform/platform.h"

#include <lib/async-testing/test_loop.h>
#include <lib/fit/function.h>
#include <lib/zx/time.h>

#include <cstdint>
#include <limits>
#include <string>

#include <gtest/gtest.h>
#include <openthread/instance.h>
#include <openthread/platform/alarm-milli.h>
#include <openthread/platform/entropy.h>
#include <openthread/platform/misc.h>
#include <openthread/platform/time.h>
#include <openthread/tasklet.h>

namespace {

// Fake OpenThread implementation.

// Records calls from the platform into the (faked) OpenThread core.
struct FakeCore {
  bool instance_initialized = true;  // Returned by otInstanceIsInitialized().
  int tasklet_process_calls = 0;
  int alarm_fired_calls = 0;
  // Optional actions run from within the callbacks, to exercise re-entry into the platform the
  // way OpenThread does.
  fit::closure on_tasklet_process;
  fit::closure on_alarm_fired;
};

// The fakes are plain C functions with OpenThread's signatures (no context pointer).
FakeCore& Core() {
  static FakeCore core;
  return core;
}

}  // namespace

extern "C" {

bool otInstanceIsInitialized(otInstance* aInstance) {
  EXPECT_NE(aInstance, nullptr);
  return Core().instance_initialized;
}

void otTaskletsProcess(otInstance* aInstance) {
  EXPECT_NE(aInstance, nullptr);
  Core().tasklet_process_calls++;
  if (Core().on_tasklet_process) {
    Core().on_tasklet_process();
  }
}

void otPlatAlarmMilliFired(otInstance* aInstance) {
  EXPECT_NE(aInstance, nullptr);
  Core().alarm_fired_calls++;
  if (Core().on_alarm_fired) {
    Core().on_alarm_fired();
  }
}

}  // extern "C"

namespace ot_rcp {
namespace {

// Node id passed to Platform::Create(). Only used by the platform as a log tag.
constexpr uint32_t kNodeId = 7;
// A different node id, used to check that re-Create() takes effect.
constexpr uint32_t kOtherNodeId = 42;

// `otInstance` is opaque; the platform only passes the pointer back to the core.
otInstance* FakeInstance() {
  static uint64_t storage;
  return reinterpret_cast<otInstance*>(&storage);
}

class PlatformTest : public ::testing::Test {
 protected:
  void SetUp() override {
    Core() = FakeCore();
    Platform::Create(loop_.dispatcher(), kNodeId);
  }

  void TearDown() override {
    // `loop_` belongs to this fixture and is destroyed right after TearDown(). Destroy the platform
    // state first so its TaskClosures cancel any task still posted to `loop_`.
    Platform::Destroy();
  }

  // Advances fake time so that `otPlatAlarmMilliGetNow()` returns `now_ms`.
  void AdvanceMilliCounterTo(uint32_t now_ms) {
    const uint32_t delta = now_ms - otPlatAlarmMilliGetNow();
    loop_.RunFor(zx::msec(delta));
    EXPECT_EQ(otPlatAlarmMilliGetNow(), now_ms);
  }

  async::TestLoop loop_;
};

// Tasklets

TEST_F(PlatformTest, SignalPendingOnce) {
  otTaskletsSignalPending(FakeInstance());
  EXPECT_EQ(Core().tasklet_process_calls, 0);

  loop_.RunUntilIdle();
  EXPECT_EQ(Core().tasklet_process_calls, 1);
}

// `otTaskletsProcess()` runs all pending tasklets at once and clears signals.
TEST_F(PlatformTest, TaskletSignalsAreCoalesced) {
  otTaskletsSignalPending(FakeInstance());
  otTaskletsSignalPending(FakeInstance());
  otTaskletsSignalPending(FakeInstance());
  loop_.RunUntilIdle();
  EXPECT_EQ(Core().tasklet_process_calls, 1);

  otTaskletsSignalPending(FakeInstance());
  loop_.RunUntilIdle();
  EXPECT_EQ(Core().tasklet_process_calls, 2);
}

// A tasklet commonly posts another tasklet while it runs.
TEST_F(PlatformTest, SignalFromWithinProcessingSchedulesAnotherRun) {
  Core().on_tasklet_process = [] {
    if (Core().tasklet_process_calls == 1) {
      otTaskletsSignalPending(FakeInstance());
    }
  };
  otTaskletsSignalPending(FakeInstance());

  loop_.RunUntilIdle();
  EXPECT_EQ(Core().tasklet_process_calls, 2);
}

// Tasklets of an instance that is not (or no longer) initialized must not run.
TEST_F(PlatformTest, TaskletsSkippedWhileInstanceNotInitialized) {
  Core().instance_initialized = false;
  otTaskletsSignalPending(FakeInstance());
  loop_.RunUntilIdle();
  EXPECT_EQ(Core().tasklet_process_calls, 0);
}

TEST_F(PlatformTest, DestroyCancelsPendingTaskletsAndRecreateIsUsable) {
  otTaskletsSignalPending(FakeInstance());
  Platform::Destroy();
  loop_.RunUntilIdle();
  EXPECT_EQ(Core().tasklet_process_calls, 0);

  Platform::Create(loop_.dispatcher(), kNodeId);
  otTaskletsSignalPending(FakeInstance());
  loop_.RunUntilIdle();
  EXPECT_EQ(Core().tasklet_process_calls, 1);
}

// Time related testing.

TEST_F(PlatformTest, AlarmFiresOnceAtT0PlusDt) {
  otPlatAlarmMilliStartAt(FakeInstance(), otPlatAlarmMilliGetNow(), 10);

  loop_.RunFor(zx::msec(9));
  EXPECT_EQ(Core().alarm_fired_calls, 0);

  loop_.RunFor(zx::msec(1));
  EXPECT_EQ(Core().alarm_fired_calls, 1);

  loop_.RunFor(zx::sec(1));
  EXPECT_EQ(Core().alarm_fired_calls, 1);
}

// A deadline that has already passed fires right away instead of being lost.
TEST_F(PlatformTest, ExpiredAlarmFiresImmediately) {
  AdvanceMilliCounterTo(1000);
  otPlatAlarmMilliStartAt(FakeInstance(), 900, 10);  // Was due at 910.

  loop_.RunUntilIdle();
  EXPECT_EQ(Core().alarm_fired_calls, 1);
}

// The counter wraps after ~49.7 days (fake time: the jump is instant). A deadline past the wrap
// must not be mistaken for an expired one.
TEST_F(PlatformTest, AlarmDeadlinePastCounterWrapAround) {
  constexpr uint32_t kT0 = std::numeric_limits<uint32_t>::max() - 2;
  AdvanceMilliCounterTo(kT0);
  otPlatAlarmMilliStartAt(FakeInstance(), kT0, 10);  // Due at 7, after the wrap.

  loop_.RunUntilIdle();
  EXPECT_EQ(Core().alarm_fired_calls, 0);

  loop_.RunFor(zx::msec(9));
  EXPECT_EQ(Core().alarm_fired_calls, 0);
  loop_.RunFor(zx::msec(1));
  EXPECT_EQ(Core().alarm_fired_calls, 1);
  EXPECT_EQ(otPlatAlarmMilliGetNow(), 7u);
}

// t0 is read 10 ms before the counter wraps, but the alarm is only started 5 ms after the wrap.
TEST_F(PlatformTest, AlarmWithT0BeforeWrapAroundStartedAfterIt) {
  AdvanceMilliCounterTo(std::numeric_limits<uint32_t>::max() - 9);  // 10 ms before the wrap.
  const uint32_t t0 = otPlatAlarmMilliGetNow();
  constexpr uint32_t kDt = 30;  // Due 20 ms after the wrap.

  loop_.RunFor(zx::msec(15));  // Counter wraps max -> 0 and reaches 5.
  ASSERT_EQ(otPlatAlarmMilliGetNow(), 5u);
  otPlatAlarmMilliStartAt(FakeInstance(), t0, kDt);  // Due at 20; now is 5 -> fires in 15 ms.

  loop_.RunFor(zx::msec(14));
  EXPECT_EQ(Core().alarm_fired_calls, 0);
  loop_.RunFor(zx::msec(1));
  EXPECT_EQ(Core().alarm_fired_calls, 1);
  EXPECT_EQ(otPlatAlarmMilliGetNow(), 20u);
}

// There is a single millisecond alarm: starting it again replaces the pending deadline.
TEST_F(PlatformTest, RestartingAlarmReplacesPendingDeadline) {
  const uint32_t now = otPlatAlarmMilliGetNow();
  otPlatAlarmMilliStartAt(FakeInstance(), now, 10);
  otPlatAlarmMilliStartAt(FakeInstance(), now, 20);

  loop_.RunFor(zx::msec(19));
  EXPECT_EQ(Core().alarm_fired_calls, 0);
  loop_.RunFor(zx::msec(1));
  EXPECT_EQ(Core().alarm_fired_calls, 1);
  loop_.RunFor(zx::sec(1));
  EXPECT_EQ(Core().alarm_fired_calls, 1);
}

// Periodic timers restart themselves from their handler, i.e. otPlatAlarmMilliStartAt() is called
// from within otPlatAlarmMilliFired(). The new deadline must be honoured.
TEST_F(PlatformTest, AlarmRestartedFromWithinFiredHandlerFiresAgain) {
  Core().on_alarm_fired = [] {
    if (Core().alarm_fired_calls == 1) {
      otPlatAlarmMilliStartAt(FakeInstance(), otPlatAlarmMilliGetNow(), 10);
    }
  };
  otPlatAlarmMilliStartAt(FakeInstance(), otPlatAlarmMilliGetNow(), 10);

  loop_.RunFor(zx::msec(10));
  EXPECT_EQ(Core().alarm_fired_calls, 1);
  loop_.RunFor(zx::msec(9));
  EXPECT_EQ(Core().alarm_fired_calls, 1);
  loop_.RunFor(zx::msec(1));
  EXPECT_EQ(Core().alarm_fired_calls, 2);
  loop_.RunFor(zx::sec(1));
  EXPECT_EQ(Core().alarm_fired_calls, 2);
}

TEST_F(PlatformTest, StopCancelsAlarm) {
  otPlatAlarmMilliStartAt(FakeInstance(), otPlatAlarmMilliGetNow(), 10);
  otPlatAlarmMilliStop(FakeInstance());

  loop_.RunFor(zx::sec(1));
  EXPECT_EQ(Core().alarm_fired_calls, 0);
}

// When Platform::Destroy() is called, the old instance's alarm must not fire, and after Create()
// the new instance's alarm must.
TEST_F(PlatformTest, DestroyCancelsAlarmAndRecreateIsUsable) {
  otPlatAlarmMilliStartAt(FakeInstance(), otPlatAlarmMilliGetNow(), 10);
  Platform::Destroy();

  loop_.RunFor(zx::sec(1));
  EXPECT_EQ(Core().alarm_fired_calls, 0);

  Platform::Create(loop_.dispatcher(), kNodeId);
  otPlatAlarmMilliStartAt(FakeInstance(), otPlatAlarmMilliGetNow(), 10);
  loop_.RunFor(zx::msec(10));
  EXPECT_EQ(Core().alarm_fired_calls, 1);
}

// The alarm of an instance that is not (or no longer) initialized must not fire.
TEST_F(PlatformTest, AlarmSkippedWhileInstanceNotInitialized) {
  Core().instance_initialized = false;
  otPlatAlarmMilliStartAt(FakeInstance(), otPlatAlarmMilliGetNow(), 10);

  loop_.RunFor(zx::sec(1));
  EXPECT_EQ(Core().alarm_fired_calls, 0);
}

// Reset reason

TEST_F(PlatformTest, ResetReasonDefaultsToPowerOnAndIsSettable) {
  EXPECT_EQ(otPlatGetResetReason(FakeInstance()), OT_PLAT_RESET_REASON_POWER_ON);
  Platform::Get().set_reset_reason(OT_PLAT_RESET_REASON_SOFTWARE);
  EXPECT_EQ(otPlatGetResetReason(FakeInstance()), OT_PLAT_RESET_REASON_SOFTWARE);
}

// Node id and log tag

// The log tag belongs to the platform state, so it follows the node id of the latest Create().
TEST_F(PlatformTest, NodeIdAndLogTagFollowCreate) {
  EXPECT_EQ(Platform::Get().node_id(), kNodeId);
  EXPECT_EQ(Platform::Get().log_tag(), "rcp-" + std::to_string(kNodeId));

  Platform::Destroy();
  Platform::Create(loop_.dispatcher(), kOtherNodeId);
  EXPECT_EQ(Platform::Get().node_id(), kOtherNodeId);
  EXPECT_EQ(Platform::Get().log_tag(), "rcp-" + std::to_string(kOtherNodeId));
}
}  // namespace
}  // namespace ot_rcp
