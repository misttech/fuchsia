// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.power.system/cpp/fidl.h>
#include <fidl/fuchsia.power.system/cpp/test_base.h>
#include <lib/async/cpp/task.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/inspect/testing/cpp/inspect.h>

#include <fbl/ref_ptr.h>
#include <gtest/gtest.h>

#include "sdk/lib/driver/power/cpp/testing-common.h"
#include "src/graphics/drivers/msd-arm-mali/src/fuchsia_power_manager.h"
#include "src/lib/testing/loop_fixture/real_loop_fixture.h"
#include "src/storage/lib/vfs/cpp/pseudo_dir.h"
#include "src/storage/lib/vfs/cpp/service.h"
#include "src/storage/lib/vfs/cpp/synchronous_vfs.h"

namespace {

class FakePowerOwner : public FuchsiaPowerManager::Owner {
 public:
  void PostPowerStateChange(bool enabled,
                            FuchsiaPowerManager::Owner::PowerStateCallback completer) override {
    enabled_calls_.push_back(enabled);
    completer(enabled);
  }
  PowerManager* GetPowerManager() override { return nullptr; }

  const std::vector<bool>& enabled_calls() const { return enabled_calls_; }

 private:
  std::vector<bool> enabled_calls_;
};

class FuchsiaPowerManagerTest : public gtest::RealLoopFixture {
 public:
  FuchsiaPowerManagerTest() : fpm_(&owner_) {}

  void SetUp() override {
    sag_loop().StartThread();
    zx::event exec_opportunistic_dupe, wake_assertive_dupe;
    sag_ = std::make_unique<power_lib_test::SystemActivityGovernor>(
        std::move(exec_opportunistic_dupe), std::move(wake_assertive_dupe),
        sag_loop().dispatcher());

    fbl::RefPtr<fs::Service> sag = fbl::MakeRefCounted<fs::Service>(
        [this](fidl::ServerEnd<fuchsia_power_system::ActivityGovernor> chan) {
          bindings_.AddBinding(sag_loop().dispatcher(), std::move(chan), sag_.get(),
                               fidl::kIgnoreBindingClosure);
          return ZX_OK;
        });

    fbl::RefPtr<fs::PseudoDir> svcs_dir = fbl::MakeRefCounted<fs::PseudoDir>();
    svcs_dir->AddEntry("fuchsia.power.system.ActivityGovernor", sag);
    vfs_.emplace(sag_loop().dispatcher());

    fidl::Endpoints<fuchsia_io::Directory> dir_endpoints =
        fidl::Endpoints<fuchsia_io::Directory>::Create();
    vfs_->ServeDirectory(std::move(svcs_dir), std::move(dir_endpoints.server));
    std::vector<fuchsia_component_runner::ComponentNamespaceEntry> namespace_entries;
    namespace_entries.emplace_back(fuchsia_component_runner::ComponentNamespaceEntry{
        {.path = "/svc", .directory = std::move(dir_endpoints.client)}});
    ns_ = std::make_unique<fdf::Namespace>(fdf::Namespace::Create(namespace_entries).value());
    ASSERT_TRUE(fpm_.Initialize(ns_.get(), inspector_.GetRoot(), sag_loop().dispatcher()));
  }

  void TearDown() override {
    sag_loop().Shutdown();
    sag_loop().JoinThreads();
  }

  const inspect::Inspector& inspector() { return inspector_; }
  const FakePowerOwner& owner() const { return owner_; }
  FuchsiaPowerManager& fuchsia_power_manager() { return fpm_; }
  power_lib_test::SystemActivityGovernor& sag() const { return *sag_; }
  async::Loop& sag_loop() { return sag_loop_; }

 private:
  async::Loop sag_loop_ = async::Loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  fidl::ServerBindingGroup<fuchsia_power_system::ActivityGovernor> bindings_;
  std::unique_ptr<fdf::Namespace> ns_;
  std::unique_ptr<power_lib_test::SystemActivityGovernor> sag_;
  std::optional<fs::SynchronousVfs> vfs_;
  inspect::Inspector inspector_;
  FakePowerOwner owner_;
  FuchsiaPowerManager fpm_;
};

TEST_F(FuchsiaPowerManagerTest, InitializeFailsWithNullNamespace) {
  FakePowerOwner owner;
  FuchsiaPowerManager fpm(&owner);
  ASSERT_FALSE(fpm.Initialize(nullptr, inspector().GetRoot()));
}

TEST_F(FuchsiaPowerManagerTest, Initialize) {
  fpromise::result<inspect::Hierarchy> hierarchy =
      RunPromise(inspect::ReadFromInspector(inspector()));
  ASSERT_TRUE(hierarchy.is_ok());

  auto* is_system_suspending = hierarchy.value().node().get_property<inspect::BoolPropertyValue>(
      FuchsiaPowerManager::kIsSystemSuspendingInspectNode);
  ASSERT_TRUE(is_system_suspending);
  ASSERT_FALSE(is_system_suspending->value());

  auto* powered_on = hierarchy.value().node().get_property<inspect::BoolPropertyValue>(
      FuchsiaPowerManager::kPoweredOnInspectNode);
  ASSERT_TRUE(powered_on);
  ASSERT_FALSE(powered_on->value());

  auto* power_up_requested = hierarchy.value().node().get_property<inspect::BoolPropertyValue>(
      FuchsiaPowerManager::kPowerOnAfterSuspendInspectNode);
  ASSERT_TRUE(power_up_requested);
  ASSERT_FALSE(power_up_requested->value());
}

TEST_F(FuchsiaPowerManagerTest, EnablePower) {
  fuchsia_power_manager().EnablePower();

  ASSERT_EQ(1u, owner().enabled_calls().size());
  ASSERT_TRUE(owner().enabled_calls()[0]);
}

TEST_F(FuchsiaPowerManagerTest, DisablePower) {
  fuchsia_power_manager().EnablePower();
  fuchsia_power_manager().DisablePower();

  ASSERT_EQ(2u, owner().enabled_calls().size());
  ASSERT_TRUE(owner().enabled_calls()[0]);
  ASSERT_FALSE(owner().enabled_calls()[1]);
}

TEST_F(FuchsiaPowerManagerTest, PowerUpIsDelayedUntilResume) {
  fuchsia_power_manager().EnablePower();

  // Discard the power up transition.
  ASSERT_EQ(1u, owner().enabled_calls().size());

  async::PostTask(sag_loop().dispatcher(), [this]() { sag().SendBeforeSuspend(); });
  RunLoopUntil([&]() { return owner().enabled_calls().size() == 2; });
  fuchsia_power_manager().EnablePower();

  // Even though power up was requested, suspension blocks the request.
  ASSERT_EQ(2u, owner().enabled_calls().size());
  ASSERT_FALSE(owner().enabled_calls()[1]);

  async::PostTask(sag_loop().dispatcher(), [this]() { sag().SendAfterResume(); });
  RunLoopUntil([&]() { return owner().enabled_calls().size() == 3; });

  // Now that the system has resumed, power up should occur.
  ASSERT_EQ(3u, owner().enabled_calls().size());
  ASSERT_TRUE(owner().enabled_calls()[2]);
}

}  // namespace
