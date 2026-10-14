// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.power/cpp/fidl.h>
#include <fidl/fuchsia.power.broker/cpp/fidl.h>
#include <lib/driver/incoming/cpp/namespace.h>
#include <lib/driver/power/cpp/suspend.h>
#include <lib/driver/testing/cpp/scoped_global_logger.h>
#include <lib/fit/function.h>
#include <zircon/errors.h>

#include <cstddef>
#include <cstdint>
#include <optional>
#include <string_view>
#include <utility>

#include <gtest/gtest.h>
#include <src/lib/testing/loop_fixture/real_loop_fixture.h>

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)

namespace power_lib_test {
namespace {

class FakeDriverBase {
 public:
  using InitMethodCallback =
      fit::function<zx::result<>(async_dispatcher_t*, fdf::Namespace&, std::string_view)>;

  template <typename T>
  void RegisterInitMethods(T&& callback) {
    registered_init_method_ = std::forward<T>(callback);
  }

  zx::result<> RunRegisteredInitMethod(async_dispatcher_t* dispatcher, fdf::Namespace& incoming,
                                       std::string_view name) {
    return registered_init_method_(dispatcher, incoming, name);
  }

  bool has_registered_init_method() const { return static_cast<bool>(registered_init_method_); }

 private:
  InitMethodCallback registered_init_method_;
};

// FakeDriverBase must precede fdf_power::Suspendable in the base class list so that FakeDriverBase
// (and specifically registered_init_method_) is fully constructed before Suspendable's constructor
// calls static_cast<Driver*>(this)->RegisterInitMethods. This mirrors real drivers where
// fdf::DriverBase is listed before fdf_power::Suspendable<MyDriver>.
class FakeSuspendableDriver : public FakeDriverBase,
                              public fdf_power::Suspendable<FakeSuspendableDriver> {
 public:
  explicit FakeSuspendableDriver(
      bool suspend_enabled = true,
      std::optional<fidl::ServerEnd<fuchsia_power_broker::ElementRunner>> runner = std::nullopt,
      bool defer_completers = false)
      : suspend_enabled_(suspend_enabled),
        runner_(std::move(runner)),
        defer_completers_(defer_completers) {}

  std::optional<fidl::ServerEnd<fuchsia_power_broker::ElementRunner>> take_power_element_runner() {
    return std::exchange(runner_, std::nullopt);
  }

  void Suspend(fdf_power::SuspendCompleter completer) override {
    suspend_count_++;
    if (defer_completers_) {
      pending_suspend_completer_.emplace(std::move(completer));
    } else {
      completer();
    }
  }

  void Resume(fdf_power::ResumeCompleter completer) override {
    resume_count_++;
    if (defer_completers_) {
      pending_resume_completer_.emplace(std::move(completer));
    } else {
      completer();
    }
  }

  bool SuspendEnabled() override { return suspend_enabled_; }

  size_t suspend_count() const { return suspend_count_; }
  size_t resume_count() const { return resume_count_; }

  bool has_pending_suspend_completer() const { return pending_suspend_completer_.has_value(); }
  bool has_pending_resume_completer() const { return pending_resume_completer_.has_value(); }

  void CompletePendingSuspend() {
    fdf_power::SuspendCompleter completer = std::move(pending_suspend_completer_.value());
    pending_suspend_completer_.reset();
    completer();
  }

  void CompletePendingResume() {
    fdf_power::ResumeCompleter completer = std::move(pending_resume_completer_.value());
    pending_resume_completer_.reset();
    completer();
  }

 private:
  bool suspend_enabled_;
  std::optional<fidl::ServerEnd<fuchsia_power_broker::ElementRunner>> runner_;
  bool defer_completers_;
  std::optional<fdf_power::SuspendCompleter> pending_suspend_completer_;
  std::optional<fdf_power::ResumeCompleter> pending_resume_completer_;
  size_t suspend_count_ = 0;
  size_t resume_count_ = 0;
};

class SuspendTests : public gtest::RealLoopFixture {
 protected:
  void SendSetLevelRaw(fidl::Client<fuchsia_power_broker::ElementRunner>& client, uint8_t level) {
    bool completed = false;
    client->SetLevel(level).ThenExactlyOnce(
        [&completed](fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel>& result) {
          EXPECT_TRUE(result.is_ok());
          completed = true;
        });
    RunLoopUntil([&completed] { return completed; });
  }

  void SendSetLevel(fidl::Client<fuchsia_power_broker::ElementRunner>& client,
                    fuchsia_hardware_power::FrameworkElementLevels level) {
    SendSetLevelRaw(client, static_cast<uint8_t>(level));
  }

  fdf_testing::ScopedGlobalLogger logger_;
};

TEST_F(SuspendTests, SkipsInitialResumeDirectlyOnFirstActivation) {
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
  FakeSuspendableDriver driver(/*suspend_enabled=*/true, std::move(server_end));
  fdf::Namespace incoming;

  ASSERT_TRUE(driver.has_registered_init_method());
  ASSERT_EQ(driver.RunRegisteredInitMethod(dispatcher(), incoming, "test-driver").status_value(),
            ZX_OK);
  EXPECT_TRUE(driver.SuspendActive());

  fidl::Client<fuchsia_power_broker::ElementRunner> runner_client(std::move(client_end),
                                                                  dispatcher());

  // When SetLevel(kOn) is the very first call at driver startup, it should be skipped without
  // calling Resume().
  SendSetLevel(runner_client, fuchsia_hardware_power::FrameworkElementLevels::kOn);
  EXPECT_EQ(driver.suspend_count(), 0u);
  EXPECT_EQ(driver.resume_count(), 0u);

  // Subsequent SetLevel(kOff) should trigger Suspend().
  SendSetLevel(runner_client, fuchsia_hardware_power::FrameworkElementLevels::kOff);
  EXPECT_EQ(driver.suspend_count(), 1u);
  EXPECT_EQ(driver.resume_count(), 0u);

  // Subsequent SetLevel(kOn) should trigger Resume().
  SendSetLevel(runner_client, fuchsia_hardware_power::FrameworkElementLevels::kOn);
  EXPECT_EQ(driver.suspend_count(), 1u);
  EXPECT_EQ(driver.resume_count(), 1u);
}

TEST_F(SuspendTests, SkipsInitialResumeAndSpuriousInitialSuspend) {
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
  FakeSuspendableDriver driver(/*suspend_enabled=*/true, std::move(server_end));
  fdf::Namespace incoming;

  ASSERT_EQ(driver.InitializeSuspend(dispatcher(), incoming, "test-driver").status_value(), ZX_OK);
  EXPECT_TRUE(driver.SuspendActive());

  fidl::Client<fuchsia_power_broker::ElementRunner> runner_client(std::move(client_end),
                                                                  dispatcher());

  // Spurious SetLevel(kOff) before the first activation should be ignored.
  SendSetLevel(runner_client, fuchsia_hardware_power::FrameworkElementLevels::kOff);
  EXPECT_EQ(driver.suspend_count(), 0u);
  EXPECT_EQ(driver.resume_count(), 0u);

  // The initial activation SetLevel(kOn) at driver startup should be skipped without calling
  // Resume().
  SendSetLevel(runner_client, fuchsia_hardware_power::FrameworkElementLevels::kOn);
  EXPECT_EQ(driver.suspend_count(), 0u);
  EXPECT_EQ(driver.resume_count(), 0u);

  // Subsequent SetLevel(kOff) should trigger Suspend().
  SendSetLevel(runner_client, fuchsia_hardware_power::FrameworkElementLevels::kOff);
  EXPECT_EQ(driver.suspend_count(), 1u);
  EXPECT_EQ(driver.resume_count(), 0u);

  // Subsequent SetLevel(kOn) should trigger Resume().
  SendSetLevel(runner_client, fuchsia_hardware_power::FrameworkElementLevels::kOn);
  EXPECT_EQ(driver.suspend_count(), 1u);
  EXPECT_EQ(driver.resume_count(), 1u);

  // Another suspend/resume cycle should trigger Suspend() and Resume() again.
  SendSetLevel(runner_client, fuchsia_hardware_power::FrameworkElementLevels::kOff);
  EXPECT_EQ(driver.suspend_count(), 2u);
  EXPECT_EQ(driver.resume_count(), 1u);

  SendSetLevel(runner_client, fuchsia_hardware_power::FrameworkElementLevels::kOn);
  EXPECT_EQ(driver.suspend_count(), 2u);
  EXPECT_EQ(driver.resume_count(), 2u);
}

TEST_F(SuspendTests, MapsUnexpectedNonZeroLevelToActive) {
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
  FakeSuspendableDriver driver(/*suspend_enabled=*/true, std::move(server_end));
  fdf::Namespace incoming;

  ASSERT_EQ(driver.InitializeSuspend(dispatcher(), incoming, "test-driver").status_value(), ZX_OK);
  fidl::Client<fuchsia_power_broker::ElementRunner> runner_client(std::move(client_end),
                                                                  dispatcher());

  // An unexpected non-zero level (e.g., 2) on initial activation should be mapped to active and
  // skipped without calling Resume().
  SendSetLevelRaw(runner_client, 2);
  EXPECT_EQ(driver.suspend_count(), 0u);
  EXPECT_EQ(driver.resume_count(), 0u);

  SendSetLevel(runner_client, fuchsia_hardware_power::FrameworkElementLevels::kOff);
  EXPECT_EQ(driver.suspend_count(), 1u);
  EXPECT_EQ(driver.resume_count(), 0u);

  // A subsequent unexpected non-zero level after Suspend() should map to active and call Resume().
  SendSetLevelRaw(runner_client, 2);
  EXPECT_EQ(driver.suspend_count(), 1u);
  EXPECT_EQ(driver.resume_count(), 1u);
}

TEST_F(SuspendTests, AsyncSuspendAndResumeCompletersDelayReplyUntilInvoked) {
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
  FakeSuspendableDriver driver(/*suspend_enabled=*/true, std::move(server_end),
                               /*defer_completers=*/true);
  fdf::Namespace incoming;

  ASSERT_EQ(driver.InitializeSuspend(dispatcher(), incoming, "test-driver").status_value(), ZX_OK);
  fidl::Client<fuchsia_power_broker::ElementRunner> runner_client(std::move(client_end),
                                                                  dispatcher());

  // Initial activation replies immediately without calling Resume().
  SendSetLevel(runner_client, fuchsia_hardware_power::FrameworkElementLevels::kOn);
  EXPECT_EQ(driver.suspend_count(), 0u);
  EXPECT_EQ(driver.resume_count(), 0u);

  // Send SetLevel(kOff) and verify it does not reply until CompletePendingSuspend() is called.
  bool suspend_replied = false;
  runner_client
      ->SetLevel(static_cast<uint8_t>(fuchsia_hardware_power::FrameworkElementLevels::kOff))
      .ThenExactlyOnce(
          [&suspend_replied](fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel>& result) {
            EXPECT_TRUE(result.is_ok());
            suspend_replied = true;
          });
  RunLoopUntil([&driver] { return driver.has_pending_suspend_completer(); });
  RunLoopUntilIdle();
  EXPECT_EQ(driver.suspend_count(), 1u);
  EXPECT_FALSE(suspend_replied);

  driver.CompletePendingSuspend();
  RunLoopUntil([&suspend_replied] { return suspend_replied; });
  EXPECT_TRUE(suspend_replied);

  // Send SetLevel(kOn) and verify it does not reply until CompletePendingResume() is called.
  bool resume_replied = false;
  runner_client->SetLevel(static_cast<uint8_t>(fuchsia_hardware_power::FrameworkElementLevels::kOn))
      .ThenExactlyOnce(
          [&resume_replied](fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel>& result) {
            EXPECT_TRUE(result.is_ok());
            resume_replied = true;
          });
  RunLoopUntil([&driver] { return driver.has_pending_resume_completer(); });
  RunLoopUntilIdle();
  EXPECT_EQ(driver.resume_count(), 1u);
  EXPECT_FALSE(resume_replied);

  driver.CompletePendingResume();
  RunLoopUntil([&resume_replied] { return resume_replied; });
  EXPECT_TRUE(resume_replied);
}

TEST_F(SuspendTests, SuspendDisabled) {
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
  FakeSuspendableDriver driver(/*suspend_enabled=*/false, std::move(server_end));
  fdf::Namespace incoming;

  ASSERT_EQ(driver.InitializeSuspend(dispatcher(), incoming, "test-driver").status_value(), ZX_OK);
  EXPECT_FALSE(driver.SuspendActive());
}

TEST_F(SuspendTests, MissingPowerElementRunnerReturnsUnavailable) {
  FakeSuspendableDriver driver(/*suspend_enabled=*/true, std::nullopt);
  fdf::Namespace incoming;

  ASSERT_EQ(driver.InitializeSuspend(dispatcher(), incoming, "test-driver").status_value(),
            ZX_ERR_UNAVAILABLE);
  EXPECT_FALSE(driver.SuspendActive());
}

}  // namespace
}  // namespace power_lib_test

#endif  // FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
