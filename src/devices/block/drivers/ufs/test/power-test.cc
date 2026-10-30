// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.interconnect/cpp/wire.h>
#include <fidl/fuchsia.hardware.platform.device/cpp/wire_test_base.h>
#include <fidl/fuchsia.hardware.ufs.phy/cpp/wire.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/fit/defer.h>
#include <lib/fpromise/single_threaded_executor.h>
#include <lib/sync/cpp/completion.h>

#include <atomic>
#include <future>
#include <mutex>
#include <optional>
#include <thread>
#include <tuple>
#include <utility>
#include <vector>

#include "src/devices/block/drivers/ufs/ufs_pdev.h"
#include "src/storage/lib/block_client/cpp/remote_block_device.h"
#include "unit-lib.h"

namespace ufs {
using namespace ufs_mock_device;

namespace {

// Reads the uint property |name| from the driver's "ufs" inspect node.
uint64_t ReadUfsInspectUint(Ufs& ufs, const char* name) {
  fpromise::result<inspect::Hierarchy> hierarchy =
      fpromise::run_single_threaded(inspect::ReadFromInspector(ufs.inspect()));
  if (hierarchy.is_error()) {
    ADD_FAILURE() << "Failed to read inspect";
    return 0;
  }
  const auto* node = hierarchy.value().GetByPath({"ufs"});
  if (node == nullptr) {
    ADD_FAILURE() << "No ufs inspect node";
    return 0;
  }
  const auto* property = node->node().get_property<inspect::UintPropertyValue>(name);
  if (property == nullptr) {
    ADD_FAILURE() << "No inspect property " << name;
    return 0;
  }
  return property->value();
}

// Reads the bool property |name| from the driver's "ufs" inspect node.
bool ReadUfsInspectBool(Ufs& ufs, const char* name) {
  fpromise::result<inspect::Hierarchy> hierarchy =
      fpromise::run_single_threaded(inspect::ReadFromInspector(ufs.inspect()));
  if (hierarchy.is_error()) {
    ADD_FAILURE() << "Failed to read inspect";
    return false;
  }
  const auto* node = hierarchy.value().GetByPath({"ufs"});
  if (node == nullptr) {
    ADD_FAILURE() << "No ufs inspect node";
    return false;
  }
  const auto* property = node->node().get_property<inspect::BoolPropertyValue>(name);
  if (property == nullptr) {
    ADD_FAILURE() << "No inspect property " << name;
    return false;
  }
  return property->value();
}

// Returns the total number of samples recorded by the exponential histogram |name| of the driver's
// "ufs" inspect node.
uint64_t ReadUfsInspectHistogramSampleCount(Ufs& ufs, const char* name) {
  fpromise::result<inspect::Hierarchy> hierarchy =
      fpromise::run_single_threaded(inspect::ReadFromInspector(ufs.inspect()));
  if (hierarchy.is_error()) {
    ADD_FAILURE() << "Failed to read inspect";
    return 0;
  }
  const auto* node = hierarchy.value().GetByPath({"ufs"});
  if (node == nullptr) {
    ADD_FAILURE() << "No ufs inspect node";
    return 0;
  }
  const auto* property = node->node().get_property<inspect::UintArrayValue>(name);
  if (property == nullptr) {
    ADD_FAILURE() << "No inspect histogram " << name;
    return 0;
  }
  uint64_t samples = 0;
  for (const auto& bucket : property->GetBuckets()) {
    samples += bucket.count;
  }
  return samples;
}

class PowerTest : public UfsTest {
 public:
  void SetUp() override {
    InitMockDevice();
    CapabilityReg::Get()
        .ReadFrom(mock_device_.GetRegisters())
        .set_auto_hibernation_support(true)
        .WriteTo(mock_device_.GetRegisters());
  }

  // Issues ElementRunner::SetLevel(|level|) from the fake Power Broker and waits for the reply.
  // Issues SetLevel(level) without waiting for the reply; the returned completion is signaled when
  // the driver answers it.
  std::shared_ptr<libsync::Completion> SetPowerLevelAsync(fuchsia_power_broker::PowerLevel level) {
    auto replied = std::make_shared<libsync::Completion>();
    driver_test().RunInEnvironmentTypeContext([&](Environment& env) {
      env.power_broker().hardware_power_element_runner_client_->SetLevel({level}).ThenExactlyOnce(
          [replied](fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel> result) {
            EXPECT_TRUE(result.is_ok());
            replied->Signal();
          });
    });
    return replied;
  }

  // The link-idle vote is recorded after the PHY call returns, so poll for the resulting state.
  void WaitForLinkIdle(bool idle) {
    const zx::time deadline = zx::deadline_after(zx::sec(5));
    while (dut_->IsLinkIdle() != idle && zx::clock::get_monotonic() < deadline) {
      zx::nanosleep(zx::deadline_after(zx::msec(1)));
    }
    EXPECT_EQ(dut_->IsLinkIdle(), idle);
  }

  // Simulates the controller having parked the link in Auto-Hibern8: the mock never does this on
  // its own, and HandleLinkIdleTimer() only votes LINK_IDLE once HCS shows DP / UTRLRDY clear.
  void ParkLinkInAutoHibernate() {
    HostControllerStatusReg::Get()
        .ReadFrom(mock_device_.GetRegisters())
        .set_device_present(false)
        .set_utp_transfer_request_list_ready(false)
        .WriteTo(mock_device_.GetRegisters());
  }

  void SetPowerLevelSync(fuchsia_power_broker::PowerLevel level) {
    std::shared_ptr<libsync::Completion> replied = SetPowerLevelAsync(level);
    EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { replied->Wait(); }));
  }

  // Power Broker sends SetLevel(off) as soon as the framework-provided element exists (no lease is
  // held yet); fdf_power::Suspendable acknowledges it without calling Ufs::Suspend() until it has
  // seen the first "on". Verify that the hardware stayed active, then perform the first real
  // suspend.
  void VerifyInitialOffIgnoredAndSuspend() {
    ASSERT_TRUE(dut_->IsResumed());
    ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);
    SetPowerLevelSync(Ufs::kPowerLevelOn);
    ASSERT_TRUE(dut_->IsResumed());
    SetPowerLevelSync(Ufs::kPowerLevelOff);
  }

  void WaitForInspectUint(const char* name, uint64_t expected) {
    const zx::time deadline = zx::deadline_after(zx::sec(10));
    while (ReadUfsInspectUint(*dut_, name) < expected && zx::clock::get_monotonic() < deadline) {
      driver_test().runtime().RunUntilIdle();
      zx::nanosleep(zx::deadline_after(zx::msec(2)));
    }
    EXPECT_EQ(ReadUfsInspectUint(*dut_, name), expected);
  }
};

TEST_F(PowerTest, PowerSuspendResume) {
  libsync::Completion sleep_complete;
  libsync::Completion awake_complete;
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateEnter,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        mock_device_.GetUicCmdProcessor().DefaultDmeHibernateEnterHandler(mock_device, ucmdarg1,
                                                                          ucmdarg2, ucmdarg3);
        sleep_complete.Signal();
      });
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateExit,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        mock_device_.GetUicCmdProcessor().DefaultDmeHibernateExitHandler(mock_device, ucmdarg1,
                                                                         ucmdarg2, ucmdarg3);
        awake_complete.Signal();
      });

  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));

  scsi::BlockDevice* block_device = nullptr;
  while (true) {
    {
      std::lock_guard<std::mutex> lock(dut_->lock());
      if (!dut_->block_devs().empty() && dut_->block_devs().contains(0) &&
          !dut_->block_devs().at(0).empty()) {
        block_device = dut_->block_devs().at(0).at(0).get();
        break;
      }
    }
    zx::nanosleep(zx::deadline_after(zx::msec(1)));
  }
  std::unique_ptr<block_client::RemoteBlockDevice> client;
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() {
    auto client_end = driver_test().Connect<fuchsia_hardware_block_volume::Service::Volume>(
        block_device->DeviceName().c_str());
    ASSERT_OK(client_end);
    auto remote_device_result =
        block_client::RemoteBlockDevice::Create(std::move(client_end.value()));
    ASSERT_OK(remote_device_result);
    client = std::move(remote_device_result.value());
  }));

  // 1. The initial SetLevel(off) is ignored; suspend after the first "on".
  ASSERT_NO_FATAL_FAILURE(VerifyInitialOffIgnoredAndSuspend());
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { sleep_complete.Wait(); }));

  // TODO(https://fxbug.dev/42075643): Check if suspend is enabled with inspect
  ASSERT_FALSE(dut_->IsResumed());
  UfsPowerMode power_mode = UfsPowerMode::kSleep;
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), power_mode);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerCondition(),
            GetPowerModeInfo(power_mode).power_condition);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(),
            GetPowerModeInfo(power_mode).link_state);

  const zx::vmo inspect_vmo = dut_->inspect().DuplicateVmo();
  ASSERT_TRUE(inspect_vmo.is_valid());

  fpromise::result<inspect::Hierarchy> hierarchy =
      fpromise::run_single_threaded(inspect::ReadFromInspector(dut_->inspect()));
  ASSERT_TRUE(hierarchy.is_ok());
  const auto* ufs = hierarchy.value().GetByPath({"ufs"});
  ASSERT_NE(ufs, nullptr);

  const auto* power_suspended =
      ufs->node().get_property<inspect::BoolPropertyValue>("power_suspended");
  ASSERT_NE(power_suspended, nullptr);
  EXPECT_TRUE(power_suspended->value());

  // 2. Issue request while power is suspended.
  awake_complete.Reset();
  sleep_complete.Reset();

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(ufs_mock_device::kMockBlockSize, 0, &vmo));
  zx_vaddr_t vaddr;
  ASSERT_OK(zx::vmar::root_self()->map(ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, 0, vmo, 0,
                                       ufs_mock_device::kMockBlockSize, &vaddr));
  char* mapped_vaddr = reinterpret_cast<char*>(vaddr);
  std::strncpy(mapped_vaddr, "test", ufs_mock_device::kMockBlockSize);

  storage::Vmoid owned_vmoid;
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync(
      [&]() { ASSERT_OK(client->BlockAttachVmo(vmo, &owned_vmoid)); }));

  BlockFifoRequest request = {
      .command = {.opcode = BLOCK_OPCODE_WRITE},
      .vmoid = owned_vmoid.get(),
      .length = 1,
      .vmo_offset = 0,
      .dev_offset = 0,
  };

  std::future<zx_status_t> tx_future =
      std::async(std::launch::async, [&]() { return client->FifoTransaction(&request, 1); });

  // The driver should stay suspended, and the block operation incomplete.
  zx_status_t status;
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync(
      [&]() { status = awake_complete.Wait(zx::msec(100)); }));
  EXPECT_EQ(status, ZX_ERR_TIMED_OUT);
  EXPECT_EQ(tx_future.wait_for(std::chrono::milliseconds(100)), std::future_status::timeout);

  // Wake up the driver.
  driver_test().RunInEnvironmentTypeContext([&](Environment& env) {
    env.power_broker()
        .hardware_power_element_runner_client_->SetLevel({Ufs::kPowerLevelOn})
        .ThenExactlyOnce([&](fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel> result) {
          EXPECT_TRUE(result.is_ok());
        });
  });

  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { awake_complete.Wait(); }));
  while (tx_future.wait_for(std::chrono::milliseconds(10)) != std::future_status::ready) {
    driver_test().runtime().RunUntilIdle();
  }
  ASSERT_OK(tx_future.get());
  ASSERT_TRUE(dut_->IsResumed());
  ASSERT_OK(zx::vmar::root_self()->unmap(vaddr, ufs_mock_device::kMockBlockSize));
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync(
      [&]() { ASSERT_OK(client->BlockDetachVmo(std::move(owned_vmoid))); }));

  // Return the driver to the suspended state.
  driver_test().RunInEnvironmentTypeContext([&](Environment& env) {
    env.power_broker()
        .hardware_power_element_runner_client_->SetLevel({Ufs::kPowerLevelOff})
        .ThenExactlyOnce([&](fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel> result) {
          EXPECT_TRUE(result.is_ok());
        });
  });

  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { sleep_complete.Wait(); }));

  ASSERT_FALSE(dut_->IsResumed());
  power_mode = UfsPowerMode::kSleep;
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), power_mode);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerCondition(),
            GetPowerModeInfo(power_mode).power_condition);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(),
            GetPowerModeInfo(power_mode).link_state);

  hierarchy = fpromise::run_single_threaded(inspect::ReadFromInspector(dut_->inspect()));
  ASSERT_TRUE(hierarchy.is_ok());
  ufs = hierarchy.value().GetByPath({"ufs"});
  ASSERT_NE(ufs, nullptr);

  power_suspended = ufs->node().get_property<inspect::BoolPropertyValue>("power_suspended");
  ASSERT_NE(power_suspended, nullptr);
  EXPECT_TRUE(power_suspended->value());

  // 3. Trigger power level change to kPowerLevelOn.
  awake_complete.Reset();
  driver_test().RunInEnvironmentTypeContext([&](Environment& env) {
    env.power_broker()
        .hardware_power_element_runner_client_->SetLevel({Ufs::kPowerLevelOn})
        .ThenExactlyOnce([&](fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel> result) {
          EXPECT_TRUE(result.is_ok());
        });
  });

  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { awake_complete.Wait(); }));

  ASSERT_TRUE(dut_->IsResumed());
  power_mode = UfsPowerMode::kActive;
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), power_mode);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerCondition(),
            GetPowerModeInfo(power_mode).power_condition);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(),
            GetPowerModeInfo(power_mode).link_state);

  hierarchy = fpromise::run_single_threaded(inspect::ReadFromInspector(dut_->inspect()));
  ASSERT_TRUE(hierarchy.is_ok());
  ufs = hierarchy.value().GetByPath({"ufs"});
  ASSERT_NE(ufs, nullptr);

  power_suspended = ufs->node().get_property<inspect::BoolPropertyValue>("power_suspended");
  ASSERT_NE(power_suspended, nullptr);
  EXPECT_FALSE(power_suspended->value());

  // 4. Trigger power level change to kPowerLevelOff.
  sleep_complete.Reset();
  driver_test().RunInEnvironmentTypeContext([&](Environment& env) {
    env.power_broker()
        .hardware_power_element_runner_client_->SetLevel({Ufs::kPowerLevelOff})
        .ThenExactlyOnce([&](fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel> result) {
          EXPECT_TRUE(result.is_ok());
        });
  });

  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { sleep_complete.Wait(); }));

  ASSERT_FALSE(dut_->IsResumed());
  power_mode = UfsPowerMode::kSleep;
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), power_mode);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerCondition(),
            GetPowerModeInfo(power_mode).power_condition);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(),
            GetPowerModeInfo(power_mode).link_state);

  hierarchy = fpromise::run_single_threaded(inspect::ReadFromInspector(dut_->inspect()));
  ASSERT_TRUE(hierarchy.is_ok());
  ufs = hierarchy.value().GetByPath({"ufs"});
  ASSERT_NE(ufs, nullptr);

  power_suspended = ufs->node().get_property<inspect::BoolPropertyValue>("power_suspended");
  ASSERT_NE(power_suspended, nullptr);
  EXPECT_TRUE(power_suspended->value());

  // 5. Restore driver to active state.
  libsync::Completion power_reply_1;
  awake_complete.Reset();
  driver_test().RunInEnvironmentTypeContext([&](Environment& env) {
    env.power_broker()
        .hardware_power_element_runner_client_->SetLevel({Ufs::kPowerLevelOn})
        .ThenExactlyOnce([&, &reply = power_reply_1](
                             fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel> result) {
          EXPECT_TRUE(result.is_ok());
          reply.Signal();
        });
  });
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { awake_complete.Wait(); }));
  power_reply_1.Wait();
  ASSERT_TRUE(dut_->IsResumed());
  mock_device_.GetUicCmdProcessor().Reset();
}

TEST_F(PowerTest, BackgroundOperations) {
  libsync::Completion sleep_complete;
  libsync::Completion awake_complete;
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateEnter,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        mock_device_.GetUicCmdProcessor().DefaultDmeHibernateEnterHandler(mock_device, ucmdarg1,
                                                                          ucmdarg2, ucmdarg3);
        sleep_complete.Signal();
      });
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateExit,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        mock_device_.GetUicCmdProcessor().DefaultDmeHibernateExitHandler(mock_device, ucmdarg1,
                                                                         ucmdarg2, ucmdarg3);
        awake_complete.Signal();
      });

  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));

  // 1. Background Operation is disabled at power off (the initial SetLevel(off) is ignored, so
  //    drive the first suspend explicitly).
  ASSERT_NO_FATAL_FAILURE(VerifyInitialOffIgnoredAndSuspend());
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { sleep_complete.Wait(); }));

  ASSERT_FALSE(dut_->IsResumed());
  UfsPowerMode power_mode = UfsPowerMode::kSleep;
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), power_mode);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerCondition(),
            GetPowerModeInfo(power_mode).power_condition);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(),
            GetPowerModeInfo(power_mode).link_state);

  const zx::vmo inspect_vmo = dut_->inspect().DuplicateVmo();
  ASSERT_TRUE(inspect_vmo.is_valid());

  fpromise::result<inspect::Hierarchy> hierarchy =
      fpromise::run_single_threaded(inspect::ReadFromInspector(dut_->inspect()));
  ASSERT_TRUE(hierarchy.is_ok());
  const auto* ufs = hierarchy.value().GetByPath({"ufs"});
  ASSERT_NE(ufs, nullptr);

  const auto* power_suspended =
      ufs->node().get_property<inspect::BoolPropertyValue>("power_suspended");
  ASSERT_NE(power_suspended, nullptr);
  EXPECT_TRUE(power_suspended->value());

  const auto* bkop_node = ufs->GetByPath({"controller"})->GetByPath({"background_operations"});
  const auto* is_background_op_enabled =
      bkop_node->node().get_property<inspect::BoolPropertyValue>("is_background_op_enabled");
  ASSERT_NE(is_background_op_enabled, nullptr);
  EXPECT_FALSE(is_background_op_enabled->value());

  // 2. Background Operation is enabled at power on
  awake_complete.Reset();
  driver_test().RunInEnvironmentTypeContext([&](Environment& env) {
    env.power_broker()
        .hardware_power_element_runner_client_->SetLevel({Ufs::kPowerLevelOn})
        .ThenExactlyOnce([&](fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel> result) {
          EXPECT_TRUE(result.is_ok());
        });
  });
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { awake_complete.Wait(); }));

  ASSERT_TRUE(dut_->IsResumed());
  power_mode = UfsPowerMode::kActive;
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), power_mode);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerCondition(),
            GetPowerModeInfo(power_mode).power_condition);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(),
            GetPowerModeInfo(power_mode).link_state);

  hierarchy = fpromise::run_single_threaded(inspect::ReadFromInspector(dut_->inspect()));
  ASSERT_TRUE(hierarchy.is_ok());
  ufs = hierarchy.value().GetByPath({"ufs"});
  ASSERT_NE(ufs, nullptr);

  power_suspended = ufs->node().get_property<inspect::BoolPropertyValue>("power_suspended");
  EXPECT_FALSE(power_suspended->value());

  bkop_node = ufs->GetByPath({"controller"})->GetByPath({"background_operations"});
  is_background_op_enabled =
      bkop_node->node().get_property<inspect::BoolPropertyValue>("is_background_op_enabled");
  EXPECT_TRUE(is_background_op_enabled->value());

  // 3. Background operations change from disabled to enabled when an Urgent Background Operation
  // Exception Event occurs.
  ASSERT_OK(DisableBackgroundOp());

  hierarchy = fpromise::run_single_threaded(inspect::ReadFromInspector(dut_->inspect()));
  ASSERT_TRUE(hierarchy.is_ok());
  ufs = hierarchy.value().GetByPath({"ufs"});
  ASSERT_NE(ufs, nullptr);

  bkop_node = ufs->GetByPath({"controller"})->GetByPath({"background_operations"});
  is_background_op_enabled =
      bkop_node->node().get_property<inspect::BoolPropertyValue>("is_background_op_enabled");
  EXPECT_FALSE(is_background_op_enabled->value());

  // Using Exception Event to trigger Urgent Background Operations.
  mock_device_.SetExceptionEventAlert(true);
  ExceptionEventStatus ee_status = {0};
  ee_status.set_urgent_bkops(true);
  mock_device_.SetAttribute(Attributes::wExceptionEventStatus,
                            static_cast<uint32_t>(ee_status.value));
  mock_device_.SetAttribute(Attributes::bBackgroundOpStatus,
                            static_cast<uint32_t>(BackgroundOpStatus::kCritical));

  // To check for an Exception Event, send a command.
  auto attribute = ReadAttribute(Attributes::bBackgroundOpStatus);
  EXPECT_OK(attribute);

  // Wait for exception event completion
  auto wait_for = [&]() -> bool {
    // Check that Background Operations is enabled
    hierarchy = fpromise::run_single_threaded(inspect::ReadFromInspector(dut_->inspect()));
    ufs = hierarchy.value().GetByPath({"ufs"});

    bkop_node = ufs->GetByPath({"controller"})->GetByPath({"background_operations"});
    is_background_op_enabled =
        bkop_node->node().get_property<inspect::BoolPropertyValue>("is_background_op_enabled");
    return is_background_op_enabled->value();
  };
  fbl::String timeout_message = "Timeout waiting for enabling Background Op";
  constexpr uint32_t kTimeoutUs = 1000000;
  ASSERT_OK(dut_->WaitWithTimeout(wait_for, zx::usec(kTimeoutUs), timeout_message));

  // Clean up
  mock_device_.SetExceptionEventAlert(false);

  // 4. Background Operation is disabled at power off
  sleep_complete.Reset();
  driver_test().RunInEnvironmentTypeContext([&](Environment& env) {
    env.power_broker()
        .hardware_power_element_runner_client_->SetLevel({Ufs::kPowerLevelOff})
        .ThenExactlyOnce([&](fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel> result) {
          EXPECT_TRUE(result.is_ok());
        });
  });
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { sleep_complete.Wait(); }));

  ASSERT_FALSE(dut_->IsResumed());
  power_mode = UfsPowerMode::kSleep;
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), power_mode);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerCondition(),
            GetPowerModeInfo(power_mode).power_condition);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(),
            GetPowerModeInfo(power_mode).link_state);

  hierarchy = fpromise::run_single_threaded(inspect::ReadFromInspector(dut_->inspect()));
  ASSERT_TRUE(hierarchy.is_ok());
  ufs = hierarchy.value().GetByPath({"ufs"});
  ASSERT_NE(ufs, nullptr);

  power_suspended = ufs->node().get_property<inspect::BoolPropertyValue>("power_suspended");
  EXPECT_TRUE(power_suspended->value());

  bkop_node = ufs->GetByPath({"controller"})->GetByPath({"background_operations"});
  is_background_op_enabled =
      bkop_node->node().get_property<inspect::BoolPropertyValue>("is_background_op_enabled");
  EXPECT_FALSE(is_background_op_enabled->value());

  // Restore driver to active state.
  libsync::Completion power_reply;
  awake_complete.Reset();
  driver_test().RunInEnvironmentTypeContext([&](Environment& env) {
    env.power_broker()
        .hardware_power_element_runner_client_->SetLevel({Ufs::kPowerLevelOn})
        .ThenExactlyOnce([&, &reply = power_reply](
                             fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel> result) {
          EXPECT_TRUE(result.is_ok());
          reply.Signal();
        });
  });
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { awake_complete.Wait(); }));
  power_reply.Wait();
  ASSERT_TRUE(dut_->IsResumed());
  mock_device_.GetUicCmdProcessor().Reset();
}

TEST_F(PowerTest, PowerDownSuspendResume) {
  TestUfs::SetSupportsPowerDownSuspend(true);
  CapabilityReg::Get()
      .ReadFrom(mock_device_.GetRegisters())
      .set_auto_hibernation_support(true)
      .WriteTo(mock_device_.GetRegisters());

  libsync::Completion sleep_complete;
  std::atomic<uint32_t> link_startup_count{0};
  std::atomic<uint32_t> post_suspend_count{0};
  std::atomic<uint32_t> pre_resume_count{0};
  std::atomic<uint32_t> pre_link_startup_count{0};
  std::atomic<uint32_t> post_resume_count{0};

  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateEnter,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        mock_device_.GetUicCmdProcessor().DefaultDmeHibernateEnterHandler(mock_device, ucmdarg1,
                                                                          ucmdarg2, ucmdarg3);
        sleep_complete.Signal();
      });
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeLinkStartUp,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        mock_device_.GetUicCmdProcessor().DefaultDmeLinkStartUpHandler(mock_device, ucmdarg1,
                                                                       ucmdarg2, ucmdarg3);
        link_startup_count.fetch_add(1);
      });

  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));

  // 1. The initial SetLevel(off) must NOT power the device down (that would add a full
  //    PowerDown + link re-initialization cycle to every boot). The first real "off" after "on"
  //    triggers the PowerDown suspend.
  ASSERT_NO_FATAL_FAILURE(VerifyInitialOffIgnoredAndSuspend());
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { sleep_complete.Wait(); }));

  ASSERT_FALSE(dut_->IsResumed());
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kPowerDown);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerCondition(),
            GetPowerModeInfo(UfsPowerMode::kPowerDown).power_condition);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(), LinkState::kOff);
  EXPECT_FALSE(HostControllerEnableReg::Get()
                   .ReadFrom(mock_device_.GetRegisters())
                   .host_controller_enable());
  EXPECT_EQ(AutoHibernateIdleTimerReg::Get().ReadFrom(mock_device_.GetRegisters()).reg_value(), 0u);
  // Exactly one link startup (boot): had the initial "off" powered the device down, the "on" in
  // VerifyInitialOffIgnoredAndSuspend() would have re-initialized the link a second time.
  EXPECT_EQ(link_startup_count.load(), 1u);

  // A power-cycled UFS device reports UNIT ATTENTION (POWER ON / RESET) on the first SCSI command
  // to each LU (observed on hardware). Emulate it when the PHY is powered back up and verify that
  // the resume path clears it with TEST UNIT READY instead of failing START STOP UNIT.
  std::atomic<uint32_t> unit_attention_reported{0};
  mock_device_.GetScsiCommandProcessor().SetHook(
      scsi::Opcode::TEST_UNIT_READY,
      [&](UfsMockDevice& mock_device, CommandUpiuData& command_upiu,
          ResponseUpiuData& response_upiu,
          cpp20::span<PhysicalRegionDescriptionTableEntry>& prdt_upius) {
        const bool had_unit_attention = mock_device.GetUnitAttention();
        auto result = mock_device_.GetScsiCommandProcessor().DefaultTestUnitReadyHandler(
            mock_device, command_upiu, response_upiu, prdt_upius);
        if (had_unit_attention) {
          unit_attention_reported.fetch_add(1);
        }
        return result;
      });

  dut_->SetHostControllerCallback([&](NotifyEvent event, uint64_t data) -> zx::result<> {
    switch (event) {
      case NotifyEvent::kPostSuspend:
        post_suspend_count.fetch_add(1);
        break;
      case NotifyEvent::kPreResume:
        pre_resume_count.fetch_add(1);
        mock_device_.SetUnitAttention(true);
        break;
      case NotifyEvent::kPreLinkStartup:
        pre_link_startup_count.fetch_add(1);
        break;
      case NotifyEvent::kPostResume:
        post_resume_count.fetch_add(1);
        break;
      default:
        break;
    }
    return Ufs::NotifyEventCallback(event, data);
  });

  // 2. Wake up the driver from PowerDown; verify full controller + link reinitialization.
  libsync::Completion resume_reply;
  driver_test().RunInEnvironmentTypeContext([&](Environment& env) {
    env.power_broker()
        .hardware_power_element_runner_client_->SetLevel({Ufs::kPowerLevelOn})
        .ThenExactlyOnce([&](fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel> result) {
          EXPECT_TRUE(result.is_ok());
          resume_reply.Signal();
        });
  });
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { resume_reply.Wait(); }));

  ASSERT_TRUE(dut_->IsResumed());
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerCondition(), scsi::PowerCondition::kActive);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(), LinkState::kActive);
  EXPECT_TRUE(HostControllerEnableReg::Get()
                  .ReadFrom(mock_device_.GetRegisters())
                  .host_controller_enable());
  EXPECT_EQ(pre_resume_count.load(), 1u);
  EXPECT_EQ(pre_link_startup_count.load(), 1u);
  EXPECT_EQ(post_resume_count.load(), 1u);
  EXPECT_EQ(link_startup_count.load(), 2u);
  EXPECT_NE(AutoHibernateIdleTimerReg::Get().ReadFrom(mock_device_.GetRegisters()).reg_value(), 0u);
  EXPECT_EQ(unit_attention_reported.load(), 1u);
  EXPECT_FALSE(mock_device_.GetUnitAttention());

  // 3. Suspend again to PowerDown; verify kPostSuspend callback, HCE disabled, and AH8 disabled.
  sleep_complete.Reset();
  libsync::Completion suspend_reply;
  driver_test().RunInEnvironmentTypeContext([&](Environment& env) {
    env.power_broker()
        .hardware_power_element_runner_client_->SetLevel({Ufs::kPowerLevelOff})
        .ThenExactlyOnce([&](fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel> result) {
          EXPECT_TRUE(result.is_ok());
          suspend_reply.Signal();
        });
  });
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() {
    sleep_complete.Wait();
    suspend_reply.Wait();
  }));

  ASSERT_FALSE(dut_->IsResumed());
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kPowerDown);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(), LinkState::kOff);
  EXPECT_EQ(post_suspend_count.load(), 1u);
  EXPECT_FALSE(HostControllerEnableReg::Get()
                   .ReadFrom(mock_device_.GetRegisters())
                   .host_controller_enable());
  EXPECT_EQ(AutoHibernateIdleTimerReg::Get().ReadFrom(mock_device_.GetRegisters()).reg_value(), 0u);

  // 4. Restore driver to active state before TearDown.
  libsync::Completion final_resume_reply;
  driver_test().RunInEnvironmentTypeContext([&](Environment& env) {
    env.power_broker()
        .hardware_power_element_runner_client_->SetLevel({Ufs::kPowerLevelOn})
        .ThenExactlyOnce([&](fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel> result) {
          EXPECT_TRUE(result.is_ok());
          final_resume_reply.Signal();
        });
  });
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { final_resume_reply.Wait(); }));
  ASSERT_TRUE(dut_->IsResumed());
  EXPECT_EQ(link_startup_count.load(), 3u);
  mock_device_.GetScsiCommandProcessor().Reset();
  mock_device_.GetUicCmdProcessor().Reset();
}

// A PowerDown resume whose re-initialization fails after DME_LINKSTARTUP has already succeeded
// must (a) leave the SetLevel unanswered (Power Broker must not report the level as reached while
// the device is still down) and keep retrying in the background, (b) leave the device recorded as
// {PowerDown, link off} with the host controller stopped, and (c) succeed, and only then answer the
// SetLevel, once the fault is gone.
TEST_F(PowerTest, PowerDownResumeRetriesAfterReinitFailure) {
  TestUfs::SetSupportsPowerDownSuspend(true);

  libsync::Completion sleep_complete;
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateEnter,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        mock_device_.GetUicCmdProcessor().DefaultDmeHibernateEnterHandler(mock_device, ucmdarg1,
                                                                          ucmdarg2, ucmdarg3);
        sleep_complete.Signal();
      });

  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));
  ASSERT_NO_FATAL_FAILURE(VerifyInitialOffIgnoredAndSuspend());
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { sleep_complete.Wait(); }));
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kPowerDown);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(), LinkState::kOff);

  std::atomic<bool> fail_reinit{true};
  std::atomic<uint32_t> pre_resume_count{0};
  std::atomic<uint32_t> post_suspend_count{0};
  std::atomic<uint32_t> device_init_done_count{0};
  dut_->SetHostControllerCallback([&](NotifyEvent event, uint64_t data) -> zx::result<> {
    switch (event) {
      case NotifyEvent::kPreResume:
        pre_resume_count.fetch_add(1);
        break;
      case NotifyEvent::kPostSuspend:
        post_suspend_count.fetch_add(1);
        break;
      case NotifyEvent::kDeviceInitDone:
        // Reached only after link startup and device init succeeded, i.e. with
        // current_link_state_ already recorded as kActive.
        device_init_done_count.fetch_add(1);
        if (fail_reinit.load()) {
          return zx::error(ZX_ERR_IO);
        }
        break;
      default:
        break;
    }
    return Ufs::NotifyEventCallback(event, data);
  });

  // 1. Every synchronous attempt fails late in ReinitAfterPowerDown(). The SetLevel must stay
  //    unanswered while the driver retries in the background (a closed ElementRunner channel is
  //    never re-bound by Power Broker, and an early reply would let dependants come up against a
  //    device that is still down).
  constexpr auto kAttempts = static_cast<uint32_t>(kMaxPowerDownResumeAttempts);
  std::shared_ptr<libsync::Completion> replied = SetPowerLevelAsync(Ufs::kPowerLevelOn);
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() {
    const zx::time deadline = zx::deadline_after(zx::sec(10));
    while (device_init_done_count.load() < kAttempts && zx::clock::get_monotonic() < deadline) {
      zx::nanosleep(zx::deadline_after(zx::msec(20)));
    }
    EXPECT_EQ(replied->Wait(zx::msec(200)), ZX_ERR_TIMED_OUT);
  }));
  // Every failed ResumePower() round makes kMaxPowerDownResumeAttempts attempts; a background
  // retry round may already have started by now.
  EXPECT_GE(pre_resume_count.load(), kAttempts);
  EXPECT_GE(post_suspend_count.load(), kAttempts);
  EXPECT_GE(device_init_done_count.load(), kAttempts);
  EXPECT_FALSE(dut_->IsResumed());
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kPowerDown);
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(), LinkState::kOff);
  EXPECT_FALSE(HostControllerEnableReg::Get()
                   .ReadFrom(mock_device_.GetRegisters())
                   .host_controller_enable());
  {
    fpromise::result<inspect::Hierarchy> hierarchy =
        fpromise::run_single_threaded(inspect::ReadFromInspector(dut_->inspect()));
    ASSERT_TRUE(hierarchy.is_ok());
    const auto* ufs = hierarchy.value().GetByPath({"ufs"});
    ASSERT_NE(ufs, nullptr);
    const auto* resume_failures =
        ufs->node().get_property<inspect::UintPropertyValue>("resume_failures");
    ASSERT_NE(resume_failures, nullptr);
    // At least the SetLevel attempt; background retries may already have added to it.
    EXPECT_GE(resume_failures->value(), 1u);
  }

  // 2. Clear the fault: the next background retry starts over from kPreResume, succeeds, and only
  //    then is the SetLevel answered.
  fail_reinit.store(false);
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync(
      [&]() { EXPECT_OK(replied->Wait(kResumeRetryMaxDelay + zx::sec(10))); }));
  EXPECT_GE(pre_resume_count.load(), kAttempts + 1);
  EXPECT_TRUE(dut_->IsResumed());
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(), LinkState::kActive);
  EXPECT_TRUE(HostControllerEnableReg::Get()
                  .ReadFrom(mock_device_.GetRegisters())
                  .host_controller_enable());

  mock_device_.GetUicCmdProcessor().Reset();
}

// Regular LUs are not necessarily contiguous. After a PowerDown resume, UNIT ATTENTION clearing
// must target the LUs that were actually bound (here 0 and 2) and skip the gap, otherwise a TEST
// UNIT READY to the non-existent LU 1 fails the whole resume.
TEST_F(PowerTest, PowerDownResumeClearsUnitAttentionOnBoundLunsOnly) {
  TestUfs::SetSupportsPowerDownSuspend(true);
  ASSERT_OK(mock_device_.AddLun(2));

  libsync::Completion sleep_complete;
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateEnter,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        mock_device_.GetUicCmdProcessor().DefaultDmeHibernateEnterHandler(mock_device, ucmdarg1,
                                                                          ucmdarg2, ucmdarg3);
        sleep_complete.Signal();
      });

  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));
  ASSERT_EQ(dut_->GetLogicalUnitCount(), 2u);
  ASSERT_NO_FATAL_FAILURE(VerifyInitialOffIgnoredAndSuspend());
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { sleep_complete.Wait(); }));
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kPowerDown);

  std::mutex tur_luns_lock;
  std::vector<uint8_t> tur_luns;
  mock_device_.GetScsiCommandProcessor().SetHook(
      scsi::Opcode::TEST_UNIT_READY,
      [&](UfsMockDevice& mock_device, CommandUpiuData& command_upiu,
          ResponseUpiuData& response_upiu,
          cpp20::span<PhysicalRegionDescriptionTableEntry>& prdt_upius) {
        {
          std::lock_guard<std::mutex> lock(tur_luns_lock);
          tur_luns.push_back(command_upiu.header.lun);
        }
        return mock_device_.GetScsiCommandProcessor().DefaultTestUnitReadyHandler(
            mock_device, command_upiu, response_upiu, prdt_upius);
      });

  SetPowerLevelSync(Ufs::kPowerLevelOn);
  ASSERT_TRUE(dut_->IsResumed());

  std::lock_guard<std::mutex> lock(tur_luns_lock);
  // The command UPIU header carries the UFS LUN (not the SCSI-translated one).
  const uint8_t device_wlun = static_cast<uint8_t>(WellKnownLuns::kUfsDevice);
  EXPECT_NE(std::find(tur_luns.begin(), tur_luns.end(), device_wlun), tur_luns.end());
  EXPECT_NE(std::find(tur_luns.begin(), tur_luns.end(), 0), tur_luns.end());
  EXPECT_NE(std::find(tur_luns.begin(), tur_luns.end(), 2), tur_luns.end());
  EXPECT_EQ(std::find(tur_luns.begin(), tur_luns.end(), 1), tur_luns.end());

  mock_device_.GetScsiCommandProcessor().Reset();
  mock_device_.GetUicCmdProcessor().Reset();
}

// Power Broker only calls SetLevel() when the required level changes, so once the synchronous
// resume attempts are exhausted the driver must keep retrying on its own while "on" is still
// required, and resume as soon as the fault clears.
TEST_F(PowerTest, PowerDownResumeRetriedInBackground) {
  TestUfs::SetSupportsPowerDownSuspend(true);

  libsync::Completion sleep_complete;
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateEnter,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        mock_device_.GetUicCmdProcessor().DefaultDmeHibernateEnterHandler(mock_device, ucmdarg1,
                                                                          ucmdarg2, ucmdarg3);
        sleep_complete.Signal();
      });

  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));
  ASSERT_NO_FATAL_FAILURE(VerifyInitialOffIgnoredAndSuspend());
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { sleep_complete.Wait(); }));
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kPowerDown);

  std::atomic<bool> fail_resume{true};
  std::atomic<uint32_t> pre_resume_count{0};
  std::atomic<uint32_t> post_suspend_count{0};
  dut_->SetHostControllerCallback([&](NotifyEvent event, uint64_t data) -> zx::result<> {
    switch (event) {
      case NotifyEvent::kPreResume:
        pre_resume_count.fetch_add(1);
        if (fail_resume.load()) {
          // A PHY that fails to power up; the driver must not touch the (still gated) registers.
          return zx::error(ZX_ERR_IO);
        }
        break;
      case NotifyEvent::kPostSuspend:
        post_suspend_count.fetch_add(1);
        break;
      default:
        break;
    }
    return Ufs::NotifyEventCallback(event, data);
  });

  // The synchronous attempts all fail; the SetLevel stays unanswered while the driver retries.
  std::shared_ptr<libsync::Completion> replied = SetPowerLevelAsync(Ufs::kPowerLevelOn);
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() {
    const zx::time deadline = zx::deadline_after(zx::sec(10));
    while (pre_resume_count.load() < static_cast<uint32_t>(kMaxPowerDownResumeAttempts) &&
           zx::clock::get_monotonic() < deadline) {
      zx::nanosleep(zx::deadline_after(zx::msec(20)));
    }
    EXPECT_EQ(replied->Wait(zx::msec(200)), ZX_ERR_TIMED_OUT);
  }));
  EXPECT_GE(pre_resume_count.load(), static_cast<uint32_t>(kMaxPowerDownResumeAttempts));
  EXPECT_EQ(post_suspend_count.load(), pre_resume_count.load());
  EXPECT_FALSE(dut_->IsResumed());

  // Clear the fault; the deferred retry (kResumeRetryInitialDelay later) must bring the device
  // back and answer the original SetLevel, without any further SetLevel from Power Broker. The
  // driver dispatcher is foreground in this fixture, so wait from a background dispatcher to let
  // it run the delayed task.
  fail_resume.store(false);
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync(
      [&]() { EXPECT_OK(replied->Wait(kResumeRetryMaxDelay + zx::sec(10))); }));
  EXPECT_TRUE(dut_->IsResumed());
  const uint32_t pre_resume_total = pre_resume_count.load();
  EXPECT_GE(pre_resume_total, static_cast<uint32_t>(kMaxPowerDownResumeAttempts) + 1);
  // Every attempt but the successful last one powered the PHY back down.
  EXPECT_EQ(post_suspend_count.load(), pre_resume_total - 1);
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);
  {
    fpromise::result<inspect::Hierarchy> hierarchy =
        fpromise::run_single_threaded(inspect::ReadFromInspector(dut_->inspect()));
    ASSERT_TRUE(hierarchy.is_ok());
    const auto* ufs = hierarchy.value().GetByPath({"ufs"});
    ASSERT_NE(ufs, nullptr);
    const auto* resume_failures =
        ufs->node().get_property<inspect::UintPropertyValue>("resume_failures");
    ASSERT_NE(resume_failures, nullptr);
    // The SetLevel attempt and any background retries that ran before the fault was cleared; the
    // successful retry is not counted.
    EXPECT_GE(resume_failures->value(), 1u);
    EXPECT_EQ(resume_failures->value(),
              (pre_resume_total - 1) / static_cast<uint32_t>(kMaxPowerDownResumeAttempts));
  }

  // A regular suspend/resume cycle still works afterwards.
  sleep_complete.Reset();
  SetPowerLevelSync(Ufs::kPowerLevelOff);
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { sleep_complete.Wait(); }));
  EXPECT_FALSE(dut_->IsResumed());
  SetPowerLevelSync(Ufs::kPowerLevelOn);
  EXPECT_TRUE(dut_->IsResumed());

  mock_device_.GetUicCmdProcessor().Reset();
}

// A power-on write-protected LU would silently become writable across a PowerDown (VCC removal
// clears fPowerOnWPEn), so the driver must fall back to Sleep: link in Hibern8, host controller
// still enabled, PHY never powered down.
TEST_F(PowerTest, PowerOnWriteProtectForcesSleepInsteadOfPowerDown) {
  TestUfs::SetSupportsPowerDownSuspend(true);

  libsync::Completion sleep_complete;
  libsync::Completion awake_complete;
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateEnter,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        mock_device_.GetUicCmdProcessor().DefaultDmeHibernateEnterHandler(mock_device, ucmdarg1,
                                                                          ucmdarg2, ucmdarg3);
        sleep_complete.Signal();
      });
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateExit,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        mock_device_.GetUicCmdProcessor().DefaultDmeHibernateExitHandler(mock_device, ucmdarg1,
                                                                         ucmdarg2, ucmdarg3);
        awake_complete.Signal();
      });

  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));
  dut_->GetDeviceManager().SetLogicalLunPowerOnWriteProtect(true);

  std::atomic<uint32_t> post_suspend_count{0};
  std::atomic<uint32_t> pre_resume_count{0};
  dut_->SetHostControllerCallback([&](NotifyEvent event, uint64_t data) -> zx::result<> {
    switch (event) {
      case NotifyEvent::kPostSuspend:
        post_suspend_count.fetch_add(1);
        break;
      case NotifyEvent::kPreResume:
        pre_resume_count.fetch_add(1);
        break;
      default:
        break;
    }
    return Ufs::NotifyEventCallback(event, data);
  });

  ASSERT_NO_FATAL_FAILURE(VerifyInitialOffIgnoredAndSuspend());
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { sleep_complete.Wait(); }));
  EXPECT_FALSE(dut_->IsResumed());
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kSleep);
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(), LinkState::kHibernate);
  EXPECT_TRUE(HostControllerEnableReg::Get()
                  .ReadFrom(mock_device_.GetRegisters())
                  .host_controller_enable());
  EXPECT_EQ(post_suspend_count.load(), 0u);

  SetPowerLevelSync(Ufs::kPowerLevelOn);
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { awake_complete.Wait(); }));
  EXPECT_TRUE(dut_->IsResumed());
  EXPECT_EQ(pre_resume_count.load(), 0u);
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);

  mock_device_.GetUicCmdProcessor().Reset();
}

// Once the host controller has been stopped there is no way back short of a full
// re-initialization, so a failing PHY power-down is still recorded as PowerDown (and counted); the
// next "on" takes the normal ReinitAfterPowerDown() path.
TEST_F(PowerTest, PowerDownSuspendWithPhyFailureStillRecordsPowerDown) {
  TestUfs::SetSupportsPowerDownSuspend(true);

  libsync::Completion sleep_complete;
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateEnter,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        mock_device_.GetUicCmdProcessor().DefaultDmeHibernateEnterHandler(mock_device, ucmdarg1,
                                                                          ucmdarg2, ucmdarg3);
        sleep_complete.Signal();
      });

  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));

  std::atomic<bool> fail_post_suspend{true};
  std::atomic<uint32_t> pre_resume_count{0};
  dut_->SetHostControllerCallback([&](NotifyEvent event, uint64_t data) -> zx::result<> {
    switch (event) {
      case NotifyEvent::kPostSuspend:
        if (fail_post_suspend.exchange(false)) {
          return zx::error(ZX_ERR_IO);
        }
        break;
      case NotifyEvent::kPreResume:
        pre_resume_count.fetch_add(1);
        break;
      default:
        break;
    }
    return Ufs::NotifyEventCallback(event, data);
  });

  ASSERT_NO_FATAL_FAILURE(VerifyInitialOffIgnoredAndSuspend());
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { sleep_complete.Wait(); }));
  EXPECT_FALSE(dut_->IsResumed());
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kPowerDown);
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(), LinkState::kOff);
  EXPECT_FALSE(HostControllerEnableReg::Get()
                   .ReadFrom(mock_device_.GetRegisters())
                   .host_controller_enable());
  {
    fpromise::result<inspect::Hierarchy> hierarchy =
        fpromise::run_single_threaded(inspect::ReadFromInspector(dut_->inspect()));
    ASSERT_TRUE(hierarchy.is_ok());
    const auto* ufs = hierarchy.value().GetByPath({"ufs"});
    ASSERT_NE(ufs, nullptr);
    const auto* suspend_failures =
        ufs->node().get_property<inspect::UintPropertyValue>("suspend_failures");
    ASSERT_NE(suspend_failures, nullptr);
    EXPECT_EQ(suspend_failures->value(), 1u);
  }

  SetPowerLevelSync(Ufs::kPowerLevelOn);
  EXPECT_TRUE(dut_->IsResumed());
  EXPECT_EQ(pre_resume_count.load(), 1u);
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);
  EXPECT_TRUE(HostControllerEnableReg::Get()
                  .ReadFrom(mock_device_.GetRegisters())
                  .host_controller_enable());

  mock_device_.GetUicCmdProcessor().Reset();
}

// Resuming from PowerDown re-initializes the link and the device, but skips work whose result
// cannot have changed across the power cycle: the persistent bRefClkFreq is only rewritten when the
// device reports a different frequency, and the UniPro attributes that only feed inspect are not
// re-read. The UIC power mode, which the link startup reset, is still re-applied.
TEST_F(PowerTest, PowerDownResumeSkipsRedundantReconfiguration) {
  TestUfs::SetSupportsPowerDownSuspend(true);

  libsync::Completion sleep_complete;
  std::atomic<uint32_t> reference_clock_reads{0};
  std::atomic<uint32_t> reference_clock_writes{0};
  std::atomic<uint32_t> unipro_attribute_reads{0};
  std::atomic<uint32_t> power_mode_changes{0};
  auto reset_hooks = fit::defer([&] {
    mock_device_.GetQueryRequestProcessor().Reset();
    mock_device_.GetUicCmdProcessor().Reset();
  });

  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateEnter,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        mock_device_.GetUicCmdProcessor().DefaultDmeHibernateEnterHandler(mock_device, ucmdarg1,
                                                                          ucmdarg2, ucmdarg3);
        sleep_complete.Signal();
      });
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeGet,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        switch (UicCommandArgument1Reg::Get().FromValue(ucmdarg1).mib_attribute()) {
          case PA_RemoteVerInfo:
          case PA_LocalVerInfo:
          case PA_TActivate:
          case PA_Granularity:
          case PA_AvailTxDataLanes:
          case PA_AvailRxDataLanes:
          case PA_MaxRxHSGear:
            unipro_attribute_reads.fetch_add(1);
            break;
          default:
            break;
        }
        mock_device_.GetUicCmdProcessor().DefaultDmeGetHandler(mock_device, ucmdarg1, ucmdarg2,
                                                               ucmdarg3);
      });
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmePeerGet,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        unipro_attribute_reads.fetch_add(1);
        mock_device_.GetUicCmdProcessor().DefaultDmePeerGetHandler(mock_device, ucmdarg1, ucmdarg2,
                                                                   ucmdarg3);
      });
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeSet,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        if (UicCommandArgument1Reg::Get().FromValue(ucmdarg1).mib_attribute() == PA_PWRMode) {
          power_mode_changes.fetch_add(1);
        }
        mock_device_.GetUicCmdProcessor().DefaultDmeSetHandler(mock_device, ucmdarg1, ucmdarg2,
                                                               ucmdarg3);
      });
  mock_device_.GetQueryRequestProcessor().SetHook(
      QueryOpcode::kReadAttribute, [&](UfsMockDevice& mock_device, QueryRequestUpiuData& req_upiu,
                                       QueryResponseUpiuData& rsp_upiu) {
        if (req_upiu.idn == static_cast<uint8_t>(Attributes::bRefClkFreq)) {
          reference_clock_reads.fetch_add(1);
        }
        return mock_device_.GetQueryRequestProcessor().DefaultReadAttributeHandler(
            mock_device, req_upiu, rsp_upiu);
      });
  mock_device_.GetQueryRequestProcessor().SetHook(
      QueryOpcode::kWriteAttribute, [&](UfsMockDevice& mock_device, QueryRequestUpiuData& req_upiu,
                                        QueryResponseUpiuData& rsp_upiu) {
        if (req_upiu.idn == static_cast<uint8_t>(Attributes::bRefClkFreq)) {
          reference_clock_writes.fetch_add(1);
        }
        return mock_device_.GetQueryRequestProcessor().DefaultWriteAttributeHandler(
            mock_device, req_upiu, rsp_upiu);
      });

  constexpr uint32_t kRefClk19_2MHz = static_cast<uint32_t>(AttributeReferenceClock::k19_2MHz);
  constexpr uint32_t kRefClk38_4MHz = static_cast<uint32_t>(AttributeReferenceClock::k38_4MHz);
  mock_device_.SetAttribute(Attributes::bRefClkFreq, kRefClk19_2MHz);

  // 1. Driver initialization configures everything.
  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));
  EXPECT_EQ(reference_clock_writes.load(), 1u);
  EXPECT_EQ(mock_device_.GetAttribute(Attributes::bRefClkFreq), kRefClk38_4MHz);
  EXPECT_GT(unipro_attribute_reads.load(), 0u);
  EXPECT_EQ(power_mode_changes.load(), 1u);

  ASSERT_NO_FATAL_FAILURE(VerifyInitialOffIgnoredAndSuspend());
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { sleep_complete.Wait(); }));
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kPowerDown);

  // 2. Resume while the device still reports the expected reference clock.
  reference_clock_reads.store(0);
  reference_clock_writes.store(0);
  unipro_attribute_reads.store(0);
  power_mode_changes.store(0);
  SetPowerLevelSync(Ufs::kPowerLevelOn);
  ASSERT_TRUE(dut_->IsResumed());
  EXPECT_EQ(reference_clock_reads.load(), 1u);
  EXPECT_EQ(reference_clock_writes.load(), 0u);
  EXPECT_EQ(unipro_attribute_reads.load(), 0u);
  EXPECT_EQ(power_mode_changes.load(), 1u);

  // 3. Resume while the device reports a different reference clock: it is rewritten.
  sleep_complete.Reset();
  SetPowerLevelSync(Ufs::kPowerLevelOff);
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { sleep_complete.Wait(); }));
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kPowerDown);
  mock_device_.SetAttribute(Attributes::bRefClkFreq, kRefClk19_2MHz);
  reference_clock_writes.store(0);
  SetPowerLevelSync(Ufs::kPowerLevelOn);
  ASSERT_TRUE(dut_->IsResumed());
  EXPECT_EQ(reference_clock_writes.load(), 1u);
  EXPECT_EQ(mock_device_.GetAttribute(Attributes::bRefClkFreq), kRefClk38_4MHz);
}

TEST_F(PowerTest, PowerDownSuspendFallsBackToSleepWithCryptoKeys) {
  TestUfs::SetSupportsPowerDownSuspend(true);

  libsync::Completion sleep_complete;
  std::atomic<uint32_t> link_startup_count{0};
  std::atomic<uint32_t> post_suspend_count{0};
  std::atomic<uint32_t> pre_resume_count{0};

  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateEnter,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        mock_device_.GetUicCmdProcessor().DefaultDmeHibernateEnterHandler(mock_device, ucmdarg1,
                                                                          ucmdarg2, ucmdarg3);
        sleep_complete.Signal();
      });
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeLinkStartUp,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        mock_device_.GetUicCmdProcessor().DefaultDmeLinkStartUpHandler(mock_device, ucmdarg1,
                                                                       ucmdarg2, ucmdarg3);
        link_startup_count.fetch_add(1);
      });

  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));

  // 1. With no keys programmed, the first real "off" is a PowerDown suspend.
  ASSERT_NO_FATAL_FAILURE(VerifyInitialOffIgnoredAndSuspend());
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { sleep_complete.Wait(); }));
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kPowerDown);
  EXPECT_FALSE(dut_->HasProgrammedCryptoKeys());

  dut_->SetHostControllerCallback([&](NotifyEvent event, uint64_t data) -> zx::result<> {
    switch (event) {
      case NotifyEvent::kPostSuspend:
        post_suspend_count.fetch_add(1);
        break;
      case NotifyEvent::kPreResume:
        pre_resume_count.fetch_add(1);
        break;
      default:
        break;
    }
    return Ufs::NotifyEventCallback(event, data);
  });

  SetPowerLevelSync(Ufs::kPowerLevelOn);
  ASSERT_TRUE(dut_->IsResumed());
  EXPECT_EQ(pre_resume_count.load(), 1u);
  EXPECT_EQ(link_startup_count.load(), 2u);

  // 2. Program an inline encryption key slot and verify CRYPTOCFG was written.
  constexpr uint8_t kSlot = 3;
  constexpr uint32_t kDataUnitSize = 4096;
  constexpr uint32_t kCryptoCfgDword16 = 0x500 + kSlot * 0x80 + 0x40;
  constexpr uint32_t kExpectedCryptoCfg = (1U << 31) | (kDataUnitSize / 512);
  ASSERT_OK(dut_->ProgramCryptoSlotForTest(kSlot, kDataUnitSize));
  EXPECT_TRUE(dut_->HasProgrammedCryptoKeys());
  EXPECT_EQ(mock_device_.GetRegisters()->Read<uint32_t>(kCryptoCfgDword16), kExpectedCryptoCfg);

  // 3. Suspend again. Because a key is live, the driver must stay in Sleep (link hibernate) rather
  //    than PowerDown so that the host controller key table and CRYPTOCFG are retained.
  sleep_complete.Reset();
  SetPowerLevelSync(Ufs::kPowerLevelOff);
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&]() { sleep_complete.Wait(); }));

  ASSERT_FALSE(dut_->IsResumed());
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kSleep);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerCondition(), scsi::PowerCondition::kIdle);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(), LinkState::kHibernate);
  EXPECT_EQ(post_suspend_count.load(), 0u);
  EXPECT_TRUE(HostControllerEnableReg::Get()
                  .ReadFrom(mock_device_.GetRegisters())
                  .host_controller_enable());
  EXPECT_EQ(mock_device_.GetRegisters()->Read<uint32_t>(kCryptoCfgDword16), kExpectedCryptoCfg);

  // 4. Resume from Sleep; verify the controller and link were not re-initialized.
  SetPowerLevelSync(Ufs::kPowerLevelOn);
  ASSERT_TRUE(dut_->IsResumed());
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);
  EXPECT_EQ(pre_resume_count.load(), 1u);
  EXPECT_EQ(link_startup_count.load(), 2u);
  EXPECT_EQ(mock_device_.GetRegisters()->Read<uint32_t>(kCryptoCfgDword16), kExpectedCryptoCfg);
  mock_device_.GetUicCmdProcessor().Reset();
}

// Verifies that ScopedAutoHibernateDisable clears AHIT and masks UHES/UHXS during DME accesses and
// restores both when the outermost scope exits.
TEST_F(PowerTest, ScopedAutoHibernateDisableProtectsDmeAccess) {
  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));

  const uint32_t configured_ahit =
      AutoHibernateIdleTimerReg::Get().ReadFrom(mock_device_.GetRegisters()).reg_value();
  ASSERT_NE(configured_ahit, 0u);

  uint32_t ahit_during_dme_get = 0xFFFFFFFF;
  bool uhes_enabled_during_dme_get = true;
  bool uhxs_enabled_during_dme_get = true;
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeGet,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        ahit_during_dme_get =
            AutoHibernateIdleTimerReg::Get().ReadFrom(mock_device.GetRegisters()).reg_value();
        auto ie = InterruptEnableReg::Get().ReadFrom(mock_device.GetRegisters());
        uhes_enabled_during_dme_get = ie.uic_hibernate_enter_status_enable();
        uhxs_enabled_during_dme_get = ie.uic_hibernate_exit_status_enable();
        UicCmdProcessor::DefaultDmeGetHandler(mock_device, ucmdarg1, ucmdarg2, ucmdarg3);
      });

  DmeGetUicCommand dme_get(*dut_, PA_MaxRxHSGear, 0);
  ASSERT_OK(dme_get.SendCommand());
  EXPECT_EQ(ahit_during_dme_get, 0u);
  EXPECT_FALSE(uhes_enabled_during_dme_get);
  EXPECT_FALSE(uhxs_enabled_during_dme_get);

  EXPECT_EQ(AutoHibernateIdleTimerReg::Get().ReadFrom(mock_device_.GetRegisters()).reg_value(),
            configured_ahit);
  auto ie_after = InterruptEnableReg::Get().ReadFrom(mock_device_.GetRegisters());
  EXPECT_TRUE(ie_after.uic_hibernate_enter_status_enable());
  EXPECT_TRUE(ie_after.uic_hibernate_exit_status_enable());

  {
    Ufs::ScopedAutoHibernateDisable outer_guard(*dut_);
    EXPECT_EQ(AutoHibernateIdleTimerReg::Get().ReadFrom(mock_device_.GetRegisters()).reg_value(),
              0u);
    {
      Ufs::ScopedAutoHibernateDisable inner_guard(*dut_);
      EXPECT_EQ(AutoHibernateIdleTimerReg::Get().ReadFrom(mock_device_.GetRegisters()).reg_value(),
                0u);
    }
    EXPECT_EQ(AutoHibernateIdleTimerReg::Get().ReadFrom(mock_device_.GetRegisters()).reg_value(),
              0u);
  }
  EXPECT_EQ(AutoHibernateIdleTimerReg::Get().ReadFrom(mock_device_.GetRegisters()).reg_value(),
            configured_ahit);

  mock_device_.GetUicCmdProcessor().Reset();
}

// Verifies that UIC error interrupts (UECN, UECPA, and UECDL including PA_INIT) clear IS.UE and
// increment uic_error_count without resetting the host controller or incrementing
// fatal_error_count.
TEST_F(PowerTest, UicErrorInterruptCountsErrorsWithoutResettingController) {
  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));
  ASSERT_EQ(ReadUfsInspectUint(*dut_, "uic_error_count"), 0u);
  ASSERT_EQ(ReadUfsInspectUint(*dut_, "fatal_error_count"), 0u);

  auto inject_uic_error = [&](auto&& program_error_register) {
    program_error_register();
    InterruptStatusReg::Get()
        .ReadFrom(mock_device_.GetRegisters())
        .set_uic_error(true)
        .WriteTo(mock_device_.GetRegisters());
    mock_device_.TriggerInterrupt();
  };

  // A Network layer error: counted, IS.UE cleared, controller stays active.
  inject_uic_error([&] {
    HostUicErrorCodeNetworkLayerReg::Get()
        .FromValue(0)
        .set_uic_network_layer_error(true)
        .set_uic_network_layer_error_code(0x1)
        .WriteTo(mock_device_.GetRegisters());
  });
  WaitForInspectUint("uic_error_count", 1u);
  EXPECT_FALSE(InterruptStatusReg::Get().ReadFrom(mock_device_.GetRegisters()).uic_error());
  EXPECT_EQ(ReadUfsInspectUint(*dut_, "fatal_error_count"), 0u);
  EXPECT_TRUE(HostControllerEnableReg::Get()
                  .ReadFrom(mock_device_.GetRegisters())
                  .host_controller_enable());
  EXPECT_TRUE(dut_->IsResumed());

  // A PHY Adapter layer error: counted, IS.UE cleared, controller stays active.
  inject_uic_error([&] {
    HostUicErrorCodePhyAdapterLayerReg::Get()
        .FromValue(0)
        .set_uic_phy_adapter_layer_error(true)
        .set_uic_phy_adapter_layer_error_code(0x1)
        .WriteTo(mock_device_.GetRegisters());
  });
  WaitForInspectUint("uic_error_count", 2u);
  EXPECT_FALSE(InterruptStatusReg::Get().ReadFrom(mock_device_.GetRegisters()).uic_error());
  EXPECT_EQ(ReadUfsInspectUint(*dut_, "fatal_error_count"), 0u);
  EXPECT_TRUE(HostControllerEnableReg::Get()
                  .ReadFrom(mock_device_.GetRegisters())
                  .host_controller_enable());
  EXPECT_TRUE(dut_->IsResumed());

  // A Data Link layer PA_INIT error: counted and dumped for diagnostics, controller stays active.
  inject_uic_error([&] {
    HostUicErrorCodeDataLinkLayerReg::Get()
        .FromValue(0)
        .set_uic_data_link_layer_error(true)
        .set_uic_data_link_layer_error_code(kUicDataLinkLayerErrorPaInit)
        .WriteTo(mock_device_.GetRegisters());
  });
  WaitForInspectUint("uic_error_count", 3u);
  EXPECT_FALSE(InterruptStatusReg::Get().ReadFrom(mock_device_.GetRegisters()).uic_error());
  EXPECT_EQ(ReadUfsInspectUint(*dut_, "fatal_error_count"), 0u);
  EXPECT_TRUE(HostControllerEnableReg::Get()
                  .ReadFrom(mock_device_.GetRegisters())
                  .host_controller_enable());
  EXPECT_TRUE(dut_->IsResumed());
  EXPECT_OK(dut_->TestUnitReady(0, 0));
}

// When the link is already parked in Auto-Hibern8 (HCS reports DP = 0 / UTRLRDY = 0), the
// outermost ScopedAutoHibernateDisable issues DME_HIBERNATE_EXIT so the caller's DME access does
// not race the wake-up; an active link needs no extra command.
TEST_F(PowerTest, ScopedAutoHibernateDisableWakesParkedLink) {
  CapabilityReg::Get()
      .ReadFrom(mock_device_.GetRegisters())
      .set_auto_hibernation_support(true)
      .WriteTo(mock_device_.GetRegisters());
  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));

  std::atomic<uint32_t> hibernate_exits{0};
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateExit,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        hibernate_exits.fetch_add(1);
        // The controller leaves Hibern8: DP / UTRLRDY come back.
        HostControllerStatusReg::Get()
            .ReadFrom(mock_device.GetRegisters())
            .set_device_present(true)
            .set_utp_transfer_request_list_ready(true)
            .WriteTo(mock_device.GetRegisters());
        UicCmdProcessor::DefaultDmeHibernateExitHandler(mock_device, ucmdarg1, ucmdarg2, ucmdarg3);
      });
  auto reset_hooks = fit::defer([&] { mock_device_.GetUicCmdProcessor().Reset(); });

  // Link active: no hibernate exit.
  {
    Ufs::ScopedAutoHibernateDisable guard(*dut_);
    EXPECT_EQ(hibernate_exits.load(), 0u);
  }

  // Link parked in AH8.
  HostControllerStatusReg::Get()
      .ReadFrom(mock_device_.GetRegisters())
      .set_device_present(false)
      .set_utp_transfer_request_list_ready(false)
      .WriteTo(mock_device_.GetRegisters());
  {
    Ufs::ScopedAutoHibernateDisable guard(*dut_);
    EXPECT_EQ(hibernate_exits.load(), 1u);
    auto hcs = HostControllerStatusReg::Get().ReadFrom(mock_device_.GetRegisters());
    EXPECT_TRUE(hcs.device_present());
    EXPECT_TRUE(hcs.utp_transfer_request_list_ready());
    // Nested guards do not re-issue the command.
    Ufs::ScopedAutoHibernateDisable inner_guard(*dut_);
    EXPECT_EQ(hibernate_exits.load(), 1u);
  }
  EXPECT_EQ(ReadUfsInspectUint(*dut_, "auto_hibern8_error_count"), 0u);
  EXPECT_EQ(ReadUfsInspectUint(*dut_, "fatal_error_count"), 0u);
}

// Verifies runtime link-idle platform voting (NotifyEvent::kLinkIdle / kLinkActive): the idle
// timer votes LINK_IDLE only once the link is parked in Auto-Hibern8 and nothing holds it active,
// and any guard, UIC command or doorbell restores ACTIVE before touching the controller.
TEST_F(PowerTest, RuntimeLinkIdleAndWakeVotes) {
  TestUfs::SetSupportsRuntimePowerManagement(true);
  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));

  std::atomic<uint32_t> link_idle_events{0};
  std::atomic<uint32_t> link_active_events{0};
  auto reset_callback = fit::defer([&] {
    libsync::Completion done;
    if (dut_->exception_event_dispatcher().get() != nullptr &&
        async::PostTask(dut_->exception_event_dispatcher().async_dispatcher(), [&] {
          dut_->SetHostControllerCallback(Ufs::NotifyEventCallback);
          done.Signal();
        }) == ZX_OK) {
      done.Wait();
    } else {
      dut_->SetHostControllerCallback(Ufs::NotifyEventCallback);
    }
  });
  dut_->SetHostControllerCallback([&](NotifyEvent event, uint64_t data) -> zx::result<> {
    if (event == NotifyEvent::kLinkIdle) {
      link_idle_events.fetch_add(1);
    } else if (event == NotifyEvent::kLinkActive) {
      link_active_events.fetch_add(1);
    }
    return Ufs::NotifyEventCallback(event, data);
  });
  // Leaving Auto-Hibern8 brings DP / UTRLRDY back.
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateExit,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        HostControllerStatusReg::Get()
            .ReadFrom(mock_device.GetRegisters())
            .set_device_present(true)
            .set_utp_transfer_request_list_ready(true)
            .WriteTo(mock_device.GetRegisters());
        UicCmdProcessor::DefaultDmeHibernateExitHandler(mock_device, ucmdarg1, ucmdarg2, ucmdarg3);
      });
  auto reset_hooks = fit::defer([&] {
    mock_device_.GetUicCmdProcessor().Reset();
    mock_device_.GetRegisterMmioProcessor().Reset();
  });

  // Configure a short link-idle timeout and long runtime-sleep timeout to isolate Tier 1.
  dut_->SetLinkIdleTimeoutForTesting(zx::msec(10));
  dut_->SetRuntimeSleepTimeoutForTesting(zx::sec(60));

  // 1. While a guard is held the timer never votes idle, even with the link parked.
  {
    Ufs::ScopedLinkActive guard(*dut_);
    ParkLinkInAutoHibernate();
    dut_->ScheduleRuntimeIdleTimers();
    zx::nanosleep(zx::deadline_after(zx::msec(60)));
    EXPECT_FALSE(dut_->IsLinkIdle());
    EXPECT_EQ(link_idle_events.load(), 0u);
  }

  // 2. Releasing the last guard re-arms the timer, which votes idle once it sees the link parked.
  WaitForLinkIdle(true);
  EXPECT_TRUE(dut_->IsLinkIdle());
  EXPECT_EQ(link_idle_events.load(), 1u);

  // 3. A DME access (ScopedAutoHibernateDisable) restores ACTIVE first, then wakes the link out of
  //    Auto-Hibern8 before the caller's command.
  {
    Ufs::ScopedAutoHibernateDisable guard(*dut_);
    EXPECT_FALSE(dut_->IsLinkIdle());
    EXPECT_EQ(link_active_events.load(), 1u);
    auto hcs = HostControllerStatusReg::Get().ReadFrom(mock_device_.GetRegisters());
    EXPECT_TRUE(hcs.device_present() && hcs.utp_transfer_request_list_ready());
  }

  // 4. The timer does not vote idle while the link is still active (not parked); after a bounded
  //    number of re-arms it waits for the next activity instead of polling forever.
  zx::nanosleep(zx::deadline_after(zx::msec(10) * (kMaxLinkIdleRearms + 3)));
  EXPECT_FALSE(dut_->IsLinkIdle());
  EXPECT_EQ(link_idle_events.load(), 1u);

  // 5. Block I/O issued while idle wakes the link before the doorbell is rung.
  scsi::BlockDevice* block_device = nullptr;
  {
    std::scoped_lock lock(dut_->lock());
    ASSERT_TRUE(dut_->block_devs().contains(0) && !dut_->block_devs().at(0).empty());
    block_device = dut_->block_devs().at(0).at(0).get();
  }
  std::unique_ptr<block_client::RemoteBlockDevice> client;
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&] {
    auto client_end = driver_test().Connect<fuchsia_hardware_block_volume::Service::Volume>(
        block_device->DeviceName().c_str());
    ASSERT_OK(client_end);
    auto remote_device_result =
        block_client::RemoteBlockDevice::Create(std::move(client_end.value()));
    ASSERT_OK(remote_device_result);
    client = std::move(remote_device_result.value());
  }));
  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(ufs_mock_device::kMockBlockSize, 0, &vmo));
  storage::Vmoid owned_vmoid;
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync(
      [&] { ASSERT_OK(client->BlockAttachVmo(vmo, &owned_vmoid)); }));
  auto detach = fit::defer([&] {
    EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync(
        [&] { ASSERT_OK(client->BlockDetachVmo(std::move(owned_vmoid))); }));
  });

  std::atomic<bool> doorbell_rung_while_idle{false};
  mock_device_.GetRegisterMmioProcessor().SetHook(
      RegisterMap::kUTRLDBR, [&](UfsMockDevice& mock_device, uint32_t value) {
        if (dut_->IsLinkIdle()) {
          doorbell_rung_while_idle = true;
        }
        RegisterMmioProcessor::DefaultUTRLDBRHandler(mock_device, value);
      });

  // Park again (the attach above may have re-armed the timer; the park makes the next firing
  // vote idle) and wait for the vote.
  ParkLinkInAutoHibernate();
  dut_->ScheduleRuntimeIdleTimers();
  WaitForLinkIdle(true);
  ASSERT_TRUE(dut_->IsLinkIdle());
  const uint32_t active_events_before_io = link_active_events.load();

  BlockFifoRequest request = {
      .command = {.opcode = BLOCK_OPCODE_READ},
      .vmoid = owned_vmoid.get(),
      .length = 1,
      .vmo_offset = 0,
      .dev_offset = 0,
  };
  std::future<zx_status_t> tx =
      std::async(std::launch::async, [&] { return client->FifoTransaction(&request, 1); });
  while (tx.wait_for(std::chrono::milliseconds(10)) != std::future_status::ready) {
    driver_test().runtime().RunUntilIdle();
  }
  EXPECT_OK(tx.get());
  EXPECT_FALSE(doorbell_rung_while_idle.load());
  EXPECT_EQ(link_active_events.load(), active_events_before_io + 1);
  EXPECT_FALSE(dut_->IsLinkIdle());

  // 6. Every transition above was timed: the idle vote (2, 5), the ACTIVE restore (3, 5) and the
  //    DME_HIBERNATE_EXIT out of the parked link (3).
  EXPECT_GE(ReadUfsInspectHistogramSampleCount(*dut_, "link_idle_latency_us"), 2u);
  EXPECT_GE(ReadUfsInspectHistogramSampleCount(*dut_, "link_active_latency_us"), 2u);
  EXPECT_GE(ReadUfsInspectHistogramSampleCount(*dut_, "ah8_exit_latency_us"), 1u);
  EXPECT_EQ(ReadUfsInspectUint(*dut_, "link_vote_error_count"), 0u);
}

// Verifies runtime device Sleep (`UFS_SLEEP_PWR_MODE` + link in Hibern8 + `kEnterSleep` platform
// vote) after long idle, urgent BKOPS hold-off, wake-on-first-request (`kExitSleep`), and clean
// transition from runtime Sleep to system PowerDown suspend.
TEST_F(PowerTest, RuntimeDeviceSleepWakeAndSystemSuspend) {
  TestUfs::SetSupportsPowerDownSuspend(true);
  TestUfs::SetSupportsRuntimePowerManagement(true);
  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));

  scsi::BlockDevice* block_device = nullptr;
  while (true) {
    {
      std::scoped_lock lock(dut_->lock());
      if (!dut_->block_devs().empty() && dut_->block_devs().contains(0) &&
          !dut_->block_devs().at(0).empty()) {
        block_device = dut_->block_devs().at(0).at(0).get();
        break;
      }
    }
    zx::nanosleep(zx::deadline_after(zx::msec(1)));
  }

  std::unique_ptr<block_client::RemoteBlockDevice> client;
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&] {
    auto client_end = driver_test().Connect<fuchsia_hardware_block_volume::Service::Volume>(
        block_device->DeviceName().c_str());
    ASSERT_OK(client_end);
    auto remote_device_result =
        block_client::RemoteBlockDevice::Create(std::move(client_end.value()));
    ASSERT_OK(remote_device_result);
    client = std::move(remote_device_result.value());
  }));

  libsync::Completion enter_sleep_called;
  libsync::Completion exit_sleep_called;
  std::atomic<uint32_t> enter_sleep_events{0};
  std::atomic<uint32_t> exit_sleep_events{0};
  auto reset_callback = fit::defer([&] {
    libsync::Completion done;
    if (dut_->exception_event_dispatcher().get() != nullptr &&
        async::PostTask(dut_->exception_event_dispatcher().async_dispatcher(), [&] {
          dut_->SetHostControllerCallback(Ufs::NotifyEventCallback);
          done.Signal();
        }) == ZX_OK) {
      done.Wait();
    } else {
      dut_->SetHostControllerCallback(Ufs::NotifyEventCallback);
    }
  });
  dut_->SetHostControllerCallback([&](NotifyEvent event, uint64_t data) -> zx::result<> {
    if (event == NotifyEvent::kEnterSleep) {
      enter_sleep_events.fetch_add(1);
      enter_sleep_called.Signal();
    } else if (event == NotifyEvent::kExitSleep) {
      exit_sleep_events.fetch_add(1);
      exit_sleep_called.Signal();
    }
    return Ufs::NotifyEventCallback(event, data);
  });

  // 1. Urgent BKOPS (`kRequiredPerformanceImpact`) must hold off runtime sleep and keep the device
  //    Active.
  mock_device_.SetAttribute(Attributes::bBackgroundOpStatus,
                            static_cast<uint32_t>(BackgroundOpStatus::kRequiredPerformanceImpact));
  dut_->SetLinkIdleTimeoutForTesting(zx::msec(5));
  dut_->SetRuntimeSleepTimeoutForTesting(zx::msec(20));
  ASSERT_OK(dut_->GetDeviceManager().EnterRuntimeSleep());
  EXPECT_FALSE(dut_->GetDeviceManager().IsRuntimeSleeping());
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);

  // 2. Clear urgent BKOPS; the runtime sleep timer must transition the device to kSleep with
  //    kEnterSleep notified while HCE remains enabled.
  mock_device_.SetAttribute(Attributes::bBackgroundOpStatus,
                            static_cast<uint32_t>(BackgroundOpStatus::kNotRequired));
  enter_sleep_called.Reset();
  dut_->ScheduleRuntimeIdleTimers();
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync(
      [&] { EXPECT_OK(enter_sleep_called.Wait(zx::sec(5))); }));
  // Wait briefly for EnterRuntimeSleep() to finish setting UfsPowerMode::kSleep after Notify.
  const zx::time sleep_deadline = zx::deadline_after(zx::sec(5));
  while (dut_->GetDeviceManager().GetCurrentPowerMode() != UfsPowerMode::kSleep &&
         zx::clock::get_monotonic() < sleep_deadline) {
    zx::nanosleep(zx::deadline_after(zx::msec(1)));
  }
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kSleep);
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(), LinkState::kHibernate);
  EXPECT_TRUE(HostControllerEnableReg::Get()
                  .ReadFrom(mock_device_.GetRegisters())
                  .host_controller_enable());
  EXPECT_GE(enter_sleep_events.load(), 1u);

  // 3. Issuing a block I/O request while in runtime Sleep must automatically wake the link and
  //    device (`kExitSleep` -> `DmeHibernateExit` -> `START STOP UNIT(kActive)`) and complete I/O.
  dut_->SetLinkIdleTimeoutForTesting(zx::sec(60));
  dut_->SetRuntimeSleepTimeoutForTesting(zx::sec(60));
  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(ufs_mock_device::kMockBlockSize, 0, &vmo));
  storage::Vmoid owned_vmoid;
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync(
      [&] { ASSERT_OK(client->BlockAttachVmo(vmo, &owned_vmoid)); }));

  BlockFifoRequest request = {
      .command = {.opcode = BLOCK_OPCODE_READ},
      .vmoid = owned_vmoid.get(),
      .length = 1,
      .vmo_offset = 0,
      .dev_offset = 0,
  };
  std::future<zx_status_t> tx_future =
      std::async(std::launch::async, [&] { return client->FifoTransaction(&request, 1); });
  while (tx_future.wait_for(std::chrono::milliseconds(10)) != std::future_status::ready) {
    driver_test().runtime().RunUntilIdle();
  }
  ASSERT_OK(tx_future.get());
  EXPECT_FALSE(dut_->GetDeviceManager().IsRuntimeSleeping());
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);
  EXPECT_GE(exit_sleep_events.load(), 1u);

  // 4. Re-enter runtime Sleep and verify that a system suspend (`SetLevel(off)`) cleanly wakes
  //    from runtime Sleep and transitions to `UfsPowerMode::kPowerDown`.
  // The Power Broker's initial SetLevel(off) is ignored until the first "on" has been seen (see
  // VerifyInitialOffIgnoredAndSuspend()); get that out of the way while the device is still Active,
  // since IsResumed() is false by design once runtime Sleep has been entered.
  SetPowerLevelSync(Ufs::kPowerLevelOn);
  ASSERT_TRUE(dut_->IsResumed());

  ASSERT_OK(dut_->GetDeviceManager().EnterRuntimeSleep());
  ASSERT_TRUE(dut_->GetDeviceManager().IsRuntimeSleeping());
  EXPECT_FALSE(dut_->IsResumed());

  SetPowerLevelSync(Ufs::kPowerLevelOff);
  EXPECT_FALSE(dut_->IsResumed());
  EXPECT_FALSE(dut_->GetDeviceManager().IsRuntimeSleeping());
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kPowerDown);
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(), LinkState::kOff);

  SetPowerLevelSync(Ufs::kPowerLevelOn);
  EXPECT_TRUE(dut_->IsResumed());
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);

  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync(
      [&] { ASSERT_OK(client->BlockDetachVmo(std::move(owned_vmoid))); }));
}

// Verifies Isr() handling of the hibernate status bits (consumed as hardware Auto-Hibern8 state
// while UHESE/UHXSE are enabled, left to the software poller while a guard masks them, counted in
// auto_hibern8_error_count on a fatal UPMCRS) and of the fatal controller/link status interrupts
// (each set bit counted in fatal_error_count without resetting the controller). UIC error
// counting is covered by UicErrorInterruptCountsErrorsWithoutResettingController.
TEST_F(PowerTest, IsrHibernateStatusAndFatalErrorCounts) {
  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));

  // Raises UHES+UHXS with the given UPMCRS, plus UTPES so the test can deterministically wait for
  // Isr() to have run (it always clears UTPES).
  auto raise_hibernate_status = [&](HostControllerStatusReg::PowerModeStatus upmcrs) {
    HostControllerStatusReg::Get()
        .ReadFrom(mock_device_.GetRegisters())
        .set_uic_power_mode_change_request_status(upmcrs)
        .WriteTo(mock_device_.GetRegisters());
    InterruptStatusReg::Get()
        .ReadFrom(mock_device_.GetRegisters())
        .set_uic_hibernate_enter_status(true)
        .set_uic_hibernate_exit_status(true)
        .set_utp_error_status(true)
        .WriteTo(mock_device_.GetRegisters());
    mock_device_.TriggerInterrupt();
    const zx::time deadline = zx::deadline_after(zx::sec(5));
    while (InterruptStatusReg::Get().ReadFrom(mock_device_.GetRegisters()).utp_error_status() &&
           zx::clock::get_monotonic() < deadline) {
      zx::nanosleep(zx::deadline_after(zx::msec(2)));
    }
    driver_test().runtime().RunUntilIdle();
    return InterruptStatusReg::Get().ReadFrom(mock_device_.GetRegisters());
  };

  // 1a. With Auto-Hibern8 enabled (UHESE/UHXSE set), a successful UHES/UHXS was raised by
  //     hardware: Isr() consumes it as the hardware-observed Hibern8 state without counting an
  //     error.
  {
    auto is = raise_hibernate_status(HostControllerStatusReg::PowerModeStatus::kPowerOk);
    EXPECT_FALSE(is.utp_error_status());
    EXPECT_FALSE(is.uic_hibernate_enter_status());
    EXPECT_FALSE(is.uic_hibernate_exit_status());
    EXPECT_EQ(ReadUfsInspectUint(*dut_, "auto_hibern8_error_count"), 0u);
    EXPECT_EQ(ReadUfsInspectUint(*dut_, "fatal_error_count"), 0u);
  }

  // 1b. While a ScopedAutoHibernateDisable guard masks UHESE/UHXSE, UHES/UHXS belong to a software
  //     DME_HIBERNATE_* poller: Isr() must leave them untouched and still not treat them as errors.
  {
    Ufs::ScopedAutoHibernateDisable guard(*dut_);
    auto is = raise_hibernate_status(HostControllerStatusReg::PowerModeStatus::kPowerOk);
    EXPECT_FALSE(is.utp_error_status());
    EXPECT_TRUE(is.uic_hibernate_enter_status());
    EXPECT_TRUE(is.uic_hibernate_exit_status());
    EXPECT_EQ(ReadUfsInspectUint(*dut_, "auto_hibern8_error_count"), 0u);
    EXPECT_EQ(ReadUfsInspectUint(*dut_, "fatal_error_count"), 0u);
    // Clear the simulated poller-owned bits before the next sub-case.
    InterruptStatusReg::Get()
        .ReadFrom(mock_device_.GetRegisters())
        .set_uic_hibernate_enter_status(false)
        .set_uic_hibernate_exit_status(false)
        .WriteTo(mock_device_.GetRegisters());
  }

  // 2. UHES with UPMCRS == kPowerFatalError increments auto_hibern8_error_count while leaving
  //    fatal_error_count at 0 and the controller active.
  HostControllerStatusReg::Get()
      .ReadFrom(mock_device_.GetRegisters())
      .set_uic_power_mode_change_request_status(
          HostControllerStatusReg::PowerModeStatus::kPowerFatalError)
      .WriteTo(mock_device_.GetRegisters());
  InterruptStatusReg::Get()
      .ReadFrom(mock_device_.GetRegisters())
      .set_uic_hibernate_enter_status(true)
      .WriteTo(mock_device_.GetRegisters());
  mock_device_.TriggerInterrupt();
  WaitForInspectUint("auto_hibern8_error_count", 1u);
  EXPECT_EQ(ReadUfsInspectUint(*dut_, "fatal_error_count"), 0u);
  EXPECT_TRUE(HostControllerEnableReg::Get()
                  .ReadFrom(mock_device_.GetRegisters())
                  .host_controller_enable());
  EXPECT_TRUE(dut_->IsResumed());

  // 3. Fatal link/controller status interrupts (ULLS, DFES, HCFES, SBFES, CEFES) clear W1C status
  //    and increment fatal_error_count once per set fatal bit without resetting the controller.
  InterruptStatusReg::Get()
      .ReadFrom(mock_device_.GetRegisters())
      .set_uic_link_lost_status(true)
      .WriteTo(mock_device_.GetRegisters());
  mock_device_.TriggerInterrupt();
  WaitForInspectUint("fatal_error_count", 1u);
  EXPECT_FALSE(
      InterruptStatusReg::Get().ReadFrom(mock_device_.GetRegisters()).uic_link_lost_status());

  InterruptStatusReg::Get()
      .ReadFrom(mock_device_.GetRegisters())
      .set_device_fatal_error_status(true)
      .WriteTo(mock_device_.GetRegisters());
  mock_device_.TriggerInterrupt();
  WaitForInspectUint("fatal_error_count", 2u);
  EXPECT_FALSE(
      InterruptStatusReg::Get().ReadFrom(mock_device_.GetRegisters()).device_fatal_error_status());

  InterruptStatusReg::Get()
      .ReadFrom(mock_device_.GetRegisters())
      .set_host_controller_fatal_error_status(true)
      .WriteTo(mock_device_.GetRegisters());
  mock_device_.TriggerInterrupt();
  WaitForInspectUint("fatal_error_count", 3u);
  EXPECT_FALSE(InterruptStatusReg::Get()
                   .ReadFrom(mock_device_.GetRegisters())
                   .host_controller_fatal_error_status());

  InterruptStatusReg::Get()
      .ReadFrom(mock_device_.GetRegisters())
      .set_system_bus_fatal_error_status(true)
      .WriteTo(mock_device_.GetRegisters());
  mock_device_.TriggerInterrupt();
  WaitForInspectUint("fatal_error_count", 4u);
  EXPECT_FALSE(InterruptStatusReg::Get()
                   .ReadFrom(mock_device_.GetRegisters())
                   .system_bus_fatal_error_status());

  InterruptStatusReg::Get()
      .ReadFrom(mock_device_.GetRegisters())
      .set_crypto_engine_fatal_error_status(true)
      .WriteTo(mock_device_.GetRegisters());
  mock_device_.TriggerInterrupt();
  WaitForInspectUint("fatal_error_count", 5u);
  EXPECT_FALSE(InterruptStatusReg::Get()
                   .ReadFrom(mock_device_.GetRegisters())
                   .crypto_engine_fatal_error_status());

  // Combined multi-bit fatal interrupt (DFES | HCFES) increments fatal_error_count by 2 -> 7.
  InterruptStatusReg::Get()
      .ReadFrom(mock_device_.GetRegisters())
      .set_device_fatal_error_status(true)
      .set_host_controller_fatal_error_status(true)
      .WriteTo(mock_device_.GetRegisters());
  mock_device_.TriggerInterrupt();
  WaitForInspectUint("fatal_error_count", 7u);
  auto is_after = InterruptStatusReg::Get().ReadFrom(mock_device_.GetRegisters());
  EXPECT_FALSE(is_after.device_fatal_error_status());
  EXPECT_FALSE(is_after.host_controller_fatal_error_status());
  EXPECT_TRUE(HostControllerEnableReg::Get()
                  .ReadFrom(mock_device_.GetRegisters())
                  .host_controller_enable());
  EXPECT_TRUE(dut_->IsResumed());
  EXPECT_OK(dut_->TestUnitReady(0, 0));
}

// Verifies software-initiated DmeHibernateEnter / DmeHibernateExit failure rollback and retry:
// 1. EnterRuntimeSleep() DmeHibernateEnter failure rolls back START STOP UNIT(Active), AH8, and
//    BKOPS without resetting the controller.
// 2. ExitRuntimeSleep() DmeHibernateExit or post-DmeHibernateExit START STOP UNIT(Active) failure
//    preserves runtime_sleeping so a subsequent WakeFromRuntimeSleep() retries cleanly (without
//    re-sending DmeHibernateExit once LinkState::kActive is already restored).
// 3. ResumePower() from kSleep suspend with a transient DmeHibernateExit failure preserves
//    system_suspended_ / power_suspended until Ufs::RetryResume() succeeds.
TEST_F(PowerTest, SoftwareHibernateFailureRollbackAndRetry) {
  TestUfs::SetSupportsRuntimePowerManagement(true);
  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));
  dut_->SetRuntimeSleepTimeoutForTesting(zx::sec(60));

  std::atomic<bool> fail_hibern8_enter{true};
  std::atomic<bool> fail_hibern8_exit{false};
  std::atomic<bool> fail_next_start_stop_unit{false};
  std::atomic<uint32_t> hibernate_exit_calls{0};
  libsync::Completion first_exit_failed;
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateEnter,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        if (fail_hibern8_enter.exchange(false)) {
          InterruptStatusReg::Get()
              .ReadFrom(mock_device.GetRegisters())
              .set_uic_hibernate_enter_status(true)
              .WriteTo(mock_device.GetRegisters());
          HostControllerStatusReg::Get()
              .ReadFrom(mock_device.GetRegisters())
              .set_uic_power_mode_change_request_status(
                  HostControllerStatusReg::PowerModeStatus::kPowerFatalError)
              .WriteTo(mock_device.GetRegisters());
          return;
        }
        UicCmdProcessor::DefaultDmeHibernateEnterHandler(mock_device, ucmdarg1, ucmdarg2, ucmdarg3);
      });
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateExit,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        hibernate_exit_calls.fetch_add(1);
        if (fail_hibern8_exit.exchange(false)) {
          InterruptStatusReg::Get()
              .ReadFrom(mock_device.GetRegisters())
              .set_uic_hibernate_exit_status(true)
              .WriteTo(mock_device.GetRegisters());
          HostControllerStatusReg::Get()
              .ReadFrom(mock_device.GetRegisters())
              .set_uic_power_mode_change_request_status(
                  HostControllerStatusReg::PowerModeStatus::kPowerFatalError)
              .WriteTo(mock_device.GetRegisters());
          first_exit_failed.Signal();
          return;
        }
        UicCmdProcessor::DefaultDmeHibernateExitHandler(mock_device, ucmdarg1, ucmdarg2, ucmdarg3);
      });
  mock_device_.GetScsiCommandProcessor().SetHook(
      scsi::Opcode::START_STOP_UNIT,
      [&](UfsMockDevice& mock_device, CommandUpiuData& command_upiu,
          ResponseUpiuData& response_upiu,
          cpp20::span<PhysicalRegionDescriptionTableEntry>& prdt_upius)
          -> zx::result<std::vector<uint8_t>> {
        if (fail_next_start_stop_unit.exchange(false)) {
          return zx::error(ZX_ERR_IO);
        }
        return ScsiCommandProcessor::DefaultStartStopUnitHandler(mock_device, command_upiu,
                                                                 response_upiu, prdt_upius);
      });
  auto reset_hooks = fit::defer([&] {
    mock_device_.GetUicCmdProcessor().Reset();
    mock_device_.GetScsiCommandProcessor().Reset();
  });

  // 1. Fail DmeHibernateEnter during EnterRuntimeSleep(): rollback restores Active, AH8, and BKOPS.
  zx::result<> enter_err = dut_->GetDeviceManager().EnterRuntimeSleep();
  EXPECT_TRUE(enter_err.is_error());
  EXPECT_FALSE(dut_->GetDeviceManager().IsRuntimeSleeping());
  EXPECT_FALSE(ReadUfsInspectBool(*dut_, "runtime_sleeping"));
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerCondition(), scsi::PowerCondition::kActive);
  EXPECT_NE(AutoHibernateIdleTimerReg::Get().ReadFrom(mock_device_.GetRegisters()).reg_value(), 0u);
  EXPECT_TRUE(mock_device_.GetFlag(Flags::fBackgroundOpsEn));
  EXPECT_EQ(ReadUfsInspectUint(*dut_, "auto_hibern8_error_count"), 1u);
  EXPECT_EQ(ReadUfsInspectUint(*dut_, "fatal_error_count"), 0u);
  EXPECT_TRUE(HostControllerEnableReg::Get()
                  .ReadFrom(mock_device_.GetRegisters())
                  .host_controller_enable());
  EXPECT_TRUE(dut_->IsResumed());
  EXPECT_OK(dut_->TestUnitReady(0, 0));

  // 2a. Enter runtime sleep normally, fail DmeHibernateExit once during ExitRuntimeSleep(), verify
  //     runtime_sleeping stays true, then retry via WakeFromRuntimeSleep() and succeed.
  ASSERT_OK(dut_->GetDeviceManager().EnterRuntimeSleep());
  ASSERT_TRUE(dut_->GetDeviceManager().IsRuntimeSleeping());
  ASSERT_TRUE(ReadUfsInspectBool(*dut_, "runtime_sleeping"));

  fail_hibern8_exit.store(true);
  zx::result<> exit_err = dut_->GetDeviceManager().ExitRuntimeSleep();
  EXPECT_TRUE(exit_err.is_error());
  EXPECT_EQ(ReadUfsInspectUint(*dut_, "auto_hibern8_error_count"), 2u);
  EXPECT_TRUE(dut_->GetDeviceManager().IsRuntimeSleeping());
  EXPECT_TRUE(ReadUfsInspectBool(*dut_, "runtime_sleeping"));
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(), LinkState::kHibernate);

  EXPECT_OK(dut_->WakeFromRuntimeSleep());
  EXPECT_FALSE(dut_->GetDeviceManager().IsRuntimeSleeping());
  EXPECT_FALSE(ReadUfsInspectBool(*dut_, "runtime_sleeping"));
  EXPECT_TRUE(dut_->IsResumed());
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerCondition(), scsi::PowerCondition::kActive);
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(), LinkState::kActive);

  // 2b. Fail START STOP UNIT(kActive) once after DmeHibernateExit succeeds during
  //     ExitRuntimeSleep(): LinkState::kActive and runtime_sleeping are preserved, and retrying via
  //     WakeFromRuntimeSleep() completes without re-sending DmeHibernateExit.
  ASSERT_OK(dut_->GetDeviceManager().EnterRuntimeSleep());
  ASSERT_TRUE(dut_->GetDeviceManager().IsRuntimeSleeping());
  hibernate_exit_calls.store(0);
  fail_next_start_stop_unit.store(true);

  exit_err = dut_->GetDeviceManager().ExitRuntimeSleep();
  EXPECT_TRUE(exit_err.is_error());
  EXPECT_EQ(hibernate_exit_calls.load(), 1u);
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(), LinkState::kActive);
  EXPECT_TRUE(dut_->GetDeviceManager().IsRuntimeSleeping());
  EXPECT_TRUE(ReadUfsInspectBool(*dut_, "runtime_sleeping"));

  EXPECT_OK(dut_->WakeFromRuntimeSleep());
  EXPECT_EQ(hibernate_exit_calls.load(), 1u);
  EXPECT_FALSE(dut_->GetDeviceManager().IsRuntimeSleeping());
  EXPECT_FALSE(ReadUfsInspectBool(*dut_, "runtime_sleeping"));
  EXPECT_TRUE(dut_->IsResumed());
  EXPECT_OK(dut_->TestUnitReady(0, 0));

  // 3. System suspend to kSleep (SupportsPowerDownSuspend() == false), then fail DmeHibernateExit
  //    once during ResumePower(): power_suspended stays true until Ufs::RetryResume() retries
  //    ResumePower() and completes SetLevel(On).
  TestUfs::SetSupportsPowerDownSuspend(false);
  ASSERT_NO_FATAL_FAILURE(VerifyInitialOffIgnoredAndSuspend());
  ASSERT_FALSE(dut_->IsResumed());
  ASSERT_TRUE(ReadUfsInspectBool(*dut_, "power_suspended"));

  first_exit_failed.Reset();
  fail_hibern8_exit.store(true);
  std::shared_ptr<libsync::Completion> resume_replied = SetPowerLevelAsync(Ufs::kPowerLevelOn);
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&] {
    EXPECT_OK(first_exit_failed.Wait(zx::sec(5)));
    EXPECT_EQ(resume_replied->Wait(zx::msec(20)), ZX_ERR_TIMED_OUT);
  }));
  EXPECT_TRUE(ReadUfsInspectBool(*dut_, "power_suspended"));
  EXPECT_FALSE(dut_->IsResumed());
  EXPECT_EQ(ReadUfsInspectUint(*dut_, "auto_hibern8_error_count"), 3u);
  EXPECT_EQ(ReadUfsInspectUint(*dut_, "resume_failures"), 1u);

  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync(
      [&] { EXPECT_OK(resume_replied->Wait(kResumeRetryMaxDelay + zx::sec(10))); }));
  EXPECT_FALSE(ReadUfsInspectBool(*dut_, "power_suspended"));
  EXPECT_EQ(ReadUfsInspectUint(*dut_, "auto_hibern8_error_count"), 3u);
  EXPECT_EQ(ReadUfsInspectUint(*dut_, "resume_failures"), 1u);
  EXPECT_TRUE(dut_->IsResumed());
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(), LinkState::kActive);
  EXPECT_OK(dut_->TestUnitReady(0, 0));
}

// Verifies WriteBooster buffer flush hold-off and buffer lifetime exhaustion handling during
// EnterRuntimeSleep() under kPreserveUserSpace.
TEST_F(PowerTest, RuntimeSleepWriteBoosterFlushAndLifetimeEdgeCases) {
  TestUfs::SetSupportsRuntimePowerManagement(true);
  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));
  // Keep the runtime-sleep timer out of the way: the sub-cases drive EnterRuntimeSleep()
  // directly and must not race a timer-initiated sleep.
  dut_->SetRuntimeSleepTimeoutForTesting(zx::sec(60));
  ASSERT_TRUE(dut_->GetDeviceManager().IsWriteBoosterEnabled());

  // 1. With kPreserveUserSpace (default), dCurrentWBBufferSize > 0 and bAvailableWBBufferSize < 4
  //    (< 40% available) requires a flush and must hold off runtime sleep while enabling
  //    fWBBufferFlushEn.
  mock_device_.SetAttribute(Attributes::dCurrentWBBufferSize, 1);
  mock_device_.SetAttribute(Attributes::bAvailableWBBufferSize, 3);
  ASSERT_OK(dut_->GetDeviceManager().EnterRuntimeSleep());
  EXPECT_FALSE(dut_->GetDeviceManager().IsRuntimeSleeping());
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);
  EXPECT_TRUE(mock_device_.GetFlag(Flags::fWBBufferFlushEn));

  // 2. When dCurrentWBBufferSize == 0, no flush is needed even if bAvailableWBBufferSize < 4;
  //    EnterRuntimeSleep() clears fWBBufferFlushEn and transitions to kSleep.
  mock_device_.SetAttribute(Attributes::dCurrentWBBufferSize, 0);
  mock_device_.SetAttribute(Attributes::bAvailableWBBufferSize, 2);
  ASSERT_OK(dut_->GetDeviceManager().EnterRuntimeSleep());
  EXPECT_TRUE(dut_->GetDeviceManager().IsRuntimeSleeping());
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kSleep);
  EXPECT_FALSE(mock_device_.GetFlag(Flags::fWBBufferFlushEn));
  ASSERT_OK(dut_->GetDeviceManager().ExitRuntimeSleep());
  EXPECT_FALSE(dut_->GetDeviceManager().IsRuntimeSleeping());

  // 3. When dCurrentWBBufferSize > 0 and bAvailableWBBufferSize >= 4 (>= 40% available), no flush
  //    is needed and EnterRuntimeSleep() transitions to kSleep.
  mock_device_.SetAttribute(Attributes::dCurrentWBBufferSize, 1);
  mock_device_.SetAttribute(Attributes::bAvailableWBBufferSize, 4);
  ASSERT_OK(dut_->GetDeviceManager().EnterRuntimeSleep());
  EXPECT_TRUE(dut_->GetDeviceManager().IsRuntimeSleeping());
  ASSERT_OK(dut_->GetDeviceManager().ExitRuntimeSleep());

  // 4. When bWBBufferLifeTimeEst reaches kExceededWriteBoosterBufferLifeTime (0x0B),
  //    QueryWriteBoosterFlushState() reports kLifetimeExhausted; EnterRuntimeSleep() disables
  //    WriteBooster and still proceeds to enter runtime sleep.
  mock_device_.SetAttribute(Attributes::bAvailableWBBufferSize, 1);
  mock_device_.SetAttribute(Attributes::bWBBufferLifeTimeEst, kExceededWriteBoosterBufferLifeTime);
  ASSERT_OK(dut_->GetDeviceManager().EnterRuntimeSleep());
  EXPECT_FALSE(dut_->GetDeviceManager().IsWriteBoosterEnabled());
  EXPECT_FALSE(mock_device_.GetFlag(Flags::fWriteBoosterEn));
  EXPECT_TRUE(dut_->GetDeviceManager().IsRuntimeSleeping());
  ASSERT_OK(dut_->GetDeviceManager().ExitRuntimeSleep());
}

// Verifies that UserSpaceConfigurationOption::kUserSpaceReduction uses the same unified < 40%
// (< kWriteBoosterFlushThreshold) WriteBooster flush hold-off threshold gated on
// dCurrentWBBufferSize > 0 before entering runtime Sleep.
TEST_F(PowerTest, RuntimeSleepWriteBoosterUserSpaceReductionThreshold) {
  TestUfs::SetSupportsRuntimePowerManagement(true);
  mock_device_.GetDeviceDesc().bWriteBoosterBufferPreserveUserSpaceEn =
      std::to_underlying(UserSpaceConfigurationOption::kUserSpaceReduction);
  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));
  // Keep the runtime-sleep timer out of the way: the sub-cases drive EnterRuntimeSleep()
  // directly and must not race a timer-initiated sleep.
  dut_->SetRuntimeSleepTimeoutForTesting(zx::sec(60));
  ASSERT_TRUE(dut_->GetDeviceManager().IsWriteBoosterEnabled());

  // 1. With dCurrentWBBufferSize > 0, bAvailableWBBufferSize == 3 (30% < 40% threshold) holds off
  //    runtime sleep and enables fWBBufferFlushEn.
  mock_device_.SetAttribute(Attributes::dCurrentWBBufferSize, 1);
  mock_device_.SetAttribute(Attributes::bAvailableWBBufferSize, 3);
  ASSERT_OK(dut_->GetDeviceManager().EnterRuntimeSleep());
  EXPECT_FALSE(dut_->GetDeviceManager().IsRuntimeSleeping());
  EXPECT_TRUE(mock_device_.GetFlag(Flags::fWBBufferFlushEn));

  // 2. bAvailableWBBufferSize == 4 (40% >= threshold) clears fWBBufferFlushEn and enters sleep.
  mock_device_.SetAttribute(Attributes::bAvailableWBBufferSize, 4);
  ASSERT_OK(dut_->GetDeviceManager().EnterRuntimeSleep());
  EXPECT_TRUE(dut_->GetDeviceManager().IsRuntimeSleeping());
  EXPECT_FALSE(mock_device_.GetFlag(Flags::fWBBufferFlushEn));
  ASSERT_OK(dut_->GetDeviceManager().ExitRuntimeSleep());

  // 3. When dCurrentWBBufferSize == 0, bAvailableWBBufferSize == 3 (< 40%) skips enabling
  //    fWBBufferFlushEn and enters runtime sleep cleanly.
  mock_device_.SetAttribute(Attributes::dCurrentWBBufferSize, 0);
  mock_device_.SetAttribute(Attributes::bAvailableWBBufferSize, 3);
  ASSERT_OK(dut_->GetDeviceManager().EnterRuntimeSleep());
  EXPECT_TRUE(dut_->GetDeviceManager().IsRuntimeSleeping());
  EXPECT_FALSE(mock_device_.GetFlag(Flags::fWBBufferFlushEn));
  ASSERT_OK(dut_->GetDeviceManager().ExitRuntimeSleep());
}

// Verifies that non-urgent BKOPS (kNotRequired, kRequiredNotCritical) do not block runtime sleep
// while performance-impacting or critical BKOPS (kRequiredPerformanceImpact, kCritical) hold off
// sleep, and that nested ScopedAutoHibernateDisable guards prevent the link-idle timer from voting
// kLinkIdle until the outermost guard is released.
TEST_F(PowerTest, RuntimeSleepBkopsThresholdAndNestedAh8Guard) {
  TestUfs::SetSupportsRuntimePowerManagement(true);
  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));
  // Keep the runtime-sleep timer out of the way: the sub-cases drive EnterRuntimeSleep()
  // directly and must not race a timer-initiated sleep.
  dut_->SetRuntimeSleepTimeoutForTesting(zx::sec(60));

  // 1. Non-urgent BKOPS (kNotRequired, kRequiredNotCritical) allow EnterRuntimeSleep().
  for (BackgroundOpStatus non_urgent_status :
       {BackgroundOpStatus::kNotRequired, BackgroundOpStatus::kRequiredNotCritical}) {
    mock_device_.SetAttribute(Attributes::bBackgroundOpStatus,
                              static_cast<uint32_t>(non_urgent_status));
    ASSERT_OK(dut_->GetDeviceManager().EnterRuntimeSleep());
    EXPECT_TRUE(dut_->GetDeviceManager().IsRuntimeSleeping());
    ASSERT_OK(dut_->GetDeviceManager().ExitRuntimeSleep());
  }

  // 2. Urgent BKOPS (kRequiredPerformanceImpact, kCritical) hold off EnterRuntimeSleep() and keep
  //    fBackgroundOpsEn set.
  for (BackgroundOpStatus urgent_status :
       {BackgroundOpStatus::kRequiredPerformanceImpact, BackgroundOpStatus::kCritical}) {
    mock_device_.SetAttribute(Attributes::bBackgroundOpStatus,
                              static_cast<uint32_t>(urgent_status));
    ASSERT_OK(dut_->GetDeviceManager().EnterRuntimeSleep());
    EXPECT_FALSE(dut_->GetDeviceManager().IsRuntimeSleeping());
    EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);
    EXPECT_TRUE(mock_device_.GetFlag(Flags::fBackgroundOpsEn));
  }
  mock_device_.SetAttribute(Attributes::bBackgroundOpStatus,
                            static_cast<uint32_t>(BackgroundOpStatus::kNotRequired));

  // 3. Nested ScopedAutoHibernateDisable guards prevent the link-idle timer from voting kLinkIdle
  //    until the outermost guard is destroyed.
  libsync::Completion link_idle_called;
  auto reset_callback = fit::defer([&] {
    libsync::Completion done;
    if (dut_->exception_event_dispatcher().get() != nullptr &&
        async::PostTask(dut_->exception_event_dispatcher().async_dispatcher(), [&] {
          dut_->SetHostControllerCallback(Ufs::NotifyEventCallback);
          done.Signal();
        }) == ZX_OK) {
      done.Wait();
    } else {
      dut_->SetHostControllerCallback(Ufs::NotifyEventCallback);
    }
  });
  dut_->SetHostControllerCallback([&](NotifyEvent event, uint64_t data) -> zx::result<> {
    if (event == NotifyEvent::kLinkIdle) {
      link_idle_called.Signal();
    }
    return Ufs::NotifyEventCallback(event, data);
  });

  dut_->SetLinkIdleTimeoutForTesting(zx::msec(10));
  {
    Ufs::ScopedAutoHibernateDisable outer_guard(*dut_);
    // Park the link now, after outer_guard performed its HCS check (so this does not trigger
    // ExitAutoHibernate()): from here on the only thing stopping an idle vote is the guard depth.
    ParkLinkInAutoHibernate();
    {
      Ufs::ScopedAutoHibernateDisable inner_guard(*dut_);
      dut_->ScheduleRuntimeIdleTimers();
    }
    // Inner guard released, but outer guard still active: neither the release nor an explicit
    // re-schedule may arm a timer that votes link-idle.
    dut_->ScheduleRuntimeIdleTimers();
    EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync(
        [&] { EXPECT_EQ(link_idle_called.Wait(zx::msec(30)), ZX_ERR_TIMED_OUT); }));
    EXPECT_FALSE(dut_->IsLinkIdle());
  }

  // Releasing the outer guard reschedules the idle timer and transitions to link-idle.
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync(
      [&] { EXPECT_OK(link_idle_called.Wait(zx::sec(5))); }));
  WaitForLinkIdle(true);
  EXPECT_TRUE(dut_->IsLinkIdle());
}

// Verifies that synchronous admin commands (ExecuteCommandSync) automatically wake the device from
// runtime sleep, and that system suspend to kSleep (when SupportsPowerDownSuspend() is false)
// adopts an already-sleeping runtime state without redundant link/device transitions.
TEST_F(PowerTest, RuntimeSleepAdminCommandWakeAndSleepSuspendAdoption) {
  TestUfs::SetSupportsPowerDownSuspend(false);
  TestUfs::SetSupportsRuntimePowerManagement(true);
  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));
  // Keep the runtime-sleep timer out of the way: the sub-cases drive EnterRuntimeSleep()
  // directly and must not race a timer-initiated sleep.
  dut_->SetRuntimeSleepTimeoutForTesting(zx::sec(60));

  // 1. Enter runtime sleep, then issue a synchronous admin command (TestUnitReady); verify that
  //    ExecuteCommandSync wakes the device and link before executing the command.
  ASSERT_OK(dut_->GetDeviceManager().EnterRuntimeSleep());
  ASSERT_TRUE(dut_->GetDeviceManager().IsRuntimeSleeping());
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kSleep);
  ASSERT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(), LinkState::kHibernate);

  ASSERT_OK(dut_->TestUnitReady(0, 0));
  EXPECT_FALSE(dut_->GetDeviceManager().IsRuntimeSleeping());
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(), LinkState::kActive);

  // 2. Re-enter runtime sleep and trigger a system suspend to kSleep (SupportsPowerDownSuspend() ==
  //    false). SuspendPower() must adopt the existing kSleep + kHibernate state without issuing
  //    another DmeHibernateEnter or DmeHibernateExit.
  SetPowerLevelSync(Ufs::kPowerLevelOn);
  ASSERT_OK(dut_->GetDeviceManager().EnterRuntimeSleep());
  ASSERT_TRUE(dut_->GetDeviceManager().IsRuntimeSleeping());

  std::atomic<uint32_t> hibernate_enter_calls{0};
  std::atomic<uint32_t> hibernate_exit_calls{0};
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateEnter,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        hibernate_enter_calls.fetch_add(1);
        UicCmdProcessor::DefaultDmeHibernateEnterHandler(mock_device, ucmdarg1, ucmdarg2, ucmdarg3);
      });
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateExit,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        hibernate_exit_calls.fetch_add(1);
        UicCmdProcessor::DefaultDmeHibernateExitHandler(mock_device, ucmdarg1, ucmdarg2, ucmdarg3);
      });

  SetPowerLevelSync(Ufs::kPowerLevelOff);
  EXPECT_FALSE(dut_->IsResumed());
  EXPECT_FALSE(dut_->GetDeviceManager().IsRuntimeSleeping());
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kSleep);
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(), LinkState::kHibernate);
  EXPECT_EQ(hibernate_enter_calls.load(), 0u);
  EXPECT_EQ(hibernate_exit_calls.load(), 0u);

  // Resume system power and verify normal wakeup from kSleep.
  SetPowerLevelSync(Ufs::kPowerLevelOn);
  EXPECT_TRUE(dut_->IsResumed());
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);
  EXPECT_EQ(dut_->GetDeviceManager().GetCurrentLinkState(), LinkState::kActive);
  EXPECT_EQ(hibernate_exit_calls.load(), 1u);

  mock_device_.GetUicCmdProcessor().Reset();
}

// Every runtime power transition is timed into an inspect histogram: the LINK_IDLE vote and the
// ACTIVE restore, leaving hardware Auto-Hibern8 for a DME access, and entering / leaving runtime
// device Sleep.
TEST_F(PowerTest, RuntimePowerTransitionLatencyHistograms) {
  TestUfs::SetSupportsRuntimePowerManagement(true);
  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));
  dut_->SetLinkIdleTimeoutForTesting(zx::msec(10));
  dut_->SetRuntimeSleepTimeoutForTesting(zx::sec(60));

  // Leaving Auto-Hibern8 brings DP / UTRLRDY back.
  mock_device_.GetUicCmdProcessor().SetHook(
      UicCommandOpcode::kDmeHibernateExit,
      [&](UfsMockDevice& mock_device, uint32_t ucmdarg1, uint32_t ucmdarg2, uint32_t ucmdarg3) {
        HostControllerStatusReg::Get()
            .ReadFrom(mock_device.GetRegisters())
            .set_device_present(true)
            .set_utp_transfer_request_list_ready(true)
            .WriteTo(mock_device.GetRegisters());
        UicCmdProcessor::DefaultDmeHibernateExitHandler(mock_device, ucmdarg1, ucmdarg2, ucmdarg3);
      });
  auto reset_hooks = fit::defer([&] { mock_device_.GetUicCmdProcessor().Reset(); });

  for (const char* name : {
           "link_idle_latency_us",
           "link_active_latency_us",
           "ah8_exit_latency_us",
           "runtime_sleep_enter_latency_us",
           "runtime_sleep_wake_latency_us",
       }) {
    EXPECT_EQ(ReadUfsInspectHistogramSampleCount(*dut_, name), 0u) << name;
  }

  // LINK_IDLE vote once the link is parked, ACTIVE restore plus DME_HIBERNATE_EXIT on the next
  // DME access.
  ParkLinkInAutoHibernate();
  dut_->ScheduleRuntimeIdleTimers();
  WaitForLinkIdle(true);
  {
    Ufs::ScopedAutoHibernateDisable guard(*dut_);
  }
  EXPECT_FALSE(dut_->IsLinkIdle());

  // Runtime device Sleep round trip.
  ASSERT_OK(dut_->GetDeviceManager().EnterRuntimeSleep());
  ASSERT_TRUE(dut_->GetDeviceManager().IsRuntimeSleeping());
  ASSERT_OK(dut_->GetDeviceManager().ExitRuntimeSleep());
  ASSERT_FALSE(dut_->GetDeviceManager().IsRuntimeSleeping());

  for (const char* name : {
           "link_idle_latency_us",
           "link_active_latency_us",
           "ah8_exit_latency_us",
           "runtime_sleep_enter_latency_us",
           "runtime_sleep_wake_latency_us",
       }) {
    EXPECT_GE(ReadUfsInspectHistogramSampleCount(*dut_, name), 1u) << name;
  }
  EXPECT_EQ(ReadUfsInspectUint(*dut_, "link_vote_error_count"), 0u);
}

// Verifies that block I/O arriving while EnterRuntimeSleep() has set suspend_in_progress_ (so
// ProcessIo() defers pending_commands_ to ScheduleRuntimeWake() on exception_event_dispatcher_) is
// kicked and completed without stalling when EnterRuntimeSleep() aborts early due to urgent BKOPS,
// WriteBooster flush hold-off, or a BlockSlotAllocation() drain timeout.
TEST_F(PowerTest, RuntimeSleepEarlyAbortWakesQueuedIo) {
  TestUfs::SetSupportsRuntimePowerManagement(true);
  ASSERT_NO_FATAL_FAILURE(StartDriver(/*supply_power_framework=*/true));
  // Keep the runtime-sleep timer out of the way: the sub-cases post EnterRuntimeSleep() directly
  // onto exception_event_dispatcher_ and must not race a timer-initiated sleep.
  dut_->SetRuntimeSleepTimeoutForTesting(zx::sec(60));
  ASSERT_TRUE(dut_->GetDeviceManager().IsWriteBoosterEnabled());

  scsi::BlockDevice* block_device = nullptr;
  const zx::time bind_deadline = zx::deadline_after(zx::sec(30));
  while (block_device == nullptr && zx::clock::get_monotonic() < bind_deadline) {
    {
      std::scoped_lock lock(dut_->lock());
      if (dut_->block_devs().contains(0) && !dut_->block_devs().at(0).empty()) {
        block_device = dut_->block_devs().at(0).at(0).get();
      }
    }
    if (block_device == nullptr) {
      zx::nanosleep(zx::deadline_after(zx::msec(1)));
    }
  }
  ASSERT_NE(block_device, nullptr);

  std::unique_ptr<block_client::RemoteBlockDevice> client;
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync([&] {
    auto client_end = driver_test().Connect<fuchsia_hardware_block_volume::Service::Volume>(
        block_device->DeviceName().c_str());
    ASSERT_OK(client_end);
    auto remote_device_result =
        block_client::RemoteBlockDevice::Create(std::move(client_end.value()));
    ASSERT_OK(remote_device_result);
    client = std::move(remote_device_result.value());
  }));
  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(ufs_mock_device::kMockBlockSize, 0, &vmo));
  storage::Vmoid owned_vmoid;
  EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync(
      [&] { ASSERT_OK(client->BlockAttachVmo(vmo, &owned_vmoid)); }));
  // Mark the target attribute read in QueryRequestProcessor (where SendRequestUsingSlot() holds
  // slot_lock_) and pause inside the completion ISR write to RegisterMap::kIS (after slot_lock_ is
  // released, before ProcessCompletionOfAdminRequests() wakes EnterRuntimeSleep()).
  std::atomic<uint8_t> pause_attr_idn{0xff};
  std::atomic<bool> pause_next_is_write{false};
  libsync::Completion query_paused;
  libsync::Completion allow_query_complete;
  auto cleanup = fit::defer([&] {
    allow_query_complete.Signal();
    EXPECT_OK(driver_test().RunOnBackgroundDispatcherSync(
        [&] { ASSERT_OK(client->BlockDetachVmo(std::move(owned_vmoid))); }));
    mock_device_.GetQueryRequestProcessor().Reset();
    mock_device_.GetRegisterMmioProcessor().Reset();
  });

  mock_device_.GetQueryRequestProcessor().SetHook(
      QueryOpcode::kReadAttribute,
      [&](UfsMockDevice& mock_device, QueryRequestUpiuData& req_upiu,
          QueryResponseUpiuData& rsp_upiu) -> zx_status_t {
        uint8_t expected_idn = req_upiu.idn;
        if (expected_idn != 0xff && pause_attr_idn.compare_exchange_strong(expected_idn, 0xff)) {
          pause_next_is_write.store(true);
        }
        return QueryRequestProcessor::DefaultReadAttributeHandler(mock_device, req_upiu, rsp_upiu);
      });
  mock_device_.GetRegisterMmioProcessor().SetHook(
      RegisterMap::kIS, [&](UfsMockDevice& mock_device, uint32_t value) {
        RegisterMmioProcessor::DefaultISHandler(mock_device, value);
        if (pause_next_is_write.exchange(false)) {
          query_paused.Signal();
          EXPECT_OK(allow_query_complete.Wait(zx::sec(10)));
        }
      });

  auto run_abort_case = [&](Attributes hold_attr, bool hold_transfer_slot) {
    query_paused.Reset();
    allow_query_complete.Reset();
    pause_next_is_write.store(false);
    pause_attr_idn.store(std::to_underlying(hold_attr));

    zx::result<> sleep_result = zx::error(ZX_ERR_INTERNAL);
    libsync::Completion sleep_done;
    ASSERT_OK(async::PostTask(dut_->exception_event_dispatcher().async_dispatcher(), [&] {
      sleep_result = dut_->GetDeviceManager().EnterRuntimeSleep();
      sleep_done.Signal();
    }));
    auto join_sleep = fit::defer([&] {
      allow_query_complete.Signal();
      std::ignore = sleep_done.Wait(zx::sec(10));
    });

    ASSERT_OK(query_paused.Wait(zx::sec(10)));
    EXPECT_FALSE(dut_->IsResumed());
    EXPECT_TRUE(dut_->GetDeviceManager().IsRuntimeSleeping());

    std::optional<uint8_t> held_slot;
    if (hold_transfer_slot) {
      zx::result<uint8_t> reserved = ReserveSlot<ufs::TransferRequestProcessor>();
      ASSERT_OK(reserved);
      held_slot = reserved.value();
    }
    auto release_held_slot = fit::defer([&] {
      if (held_slot.has_value()) {
        EXPECT_OK(ClearSlot(*held_slot));
        held_slot.reset();
      }
    });

    BlockFifoRequest request = {
        .command = {.opcode = BLOCK_OPCODE_READ},
        .vmoid = owned_vmoid.get(),
        .length = 1,
        .vmo_offset = 0,
        .dev_offset = 0,
    };
    std::future<zx_status_t> io_future =
        std::async(std::launch::async, [&] { return client->FifoTransaction(&request, 1); });
    const zx::time queue_deadline = zx::deadline_after(zx::sec(10));
    while (dut_->GetPendingCommandsCount() == 0 && zx::clock::get_monotonic() < queue_deadline) {
      driver_test().runtime().RunUntilIdle();
      zx::nanosleep(zx::deadline_after(zx::msec(1)));
    }
    ASSERT_EQ(dut_->GetPendingCommandsCount(), 1u);
    // Give io_worker time to observe !IsResumed() && IsRuntimeSleeping() and queue
    // ScheduleRuntimeWake() behind EnterRuntimeSleep() on exception_event_dispatcher_.
    EXPECT_EQ(io_future.wait_for(std::chrono::milliseconds(15)), std::future_status::timeout);

    allow_query_complete.Signal();
    ASSERT_OK(sleep_done.Wait(zx::sec(10)));
    EXPECT_OK(sleep_result);

    const zx::time io_deadline = zx::deadline_after(zx::sec(10));
    while (io_future.wait_for(std::chrono::milliseconds(10)) != std::future_status::ready &&
           zx::clock::get_monotonic() < io_deadline) {
      driver_test().runtime().RunUntilIdle();
    }
    const bool completed_in_time =
        io_future.wait_for(std::chrono::milliseconds(0)) == std::future_status::ready;
    EXPECT_TRUE(completed_in_time);
    if (!completed_in_time) {
      release_held_slot.call();
      dut_->GetTransferRequestProcessor().UnblockSlotAllocation();
      ASSERT_OK(async::PostTask(dut_->io_worker_dispatcher()->async_dispatcher(),
                                [this] { dut_->ProcessIoSubmissions(); }));
    }
    EXPECT_OK(io_future.get());
    release_held_slot.call();
    EXPECT_EQ(dut_->GetPendingCommandsCount(), 0u);
    EXPECT_TRUE(dut_->IsResumed());
    EXPECT_FALSE(dut_->GetDeviceManager().IsRuntimeSleeping());
    EXPECT_EQ(dut_->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);
  };

  // 1. Urgent BKOPS hold-off (bBackgroundOpStatus >= kRequiredPerformanceImpact).
  mock_device_.SetAttribute(Attributes::bBackgroundOpStatus,
                            static_cast<uint32_t>(BackgroundOpStatus::kRequiredPerformanceImpact));
  ASSERT_NO_FATAL_FAILURE(
      run_abort_case(Attributes::bBackgroundOpStatus, /*hold_transfer_slot=*/false));
  EXPECT_TRUE(mock_device_.GetFlag(Flags::fBackgroundOpsEn));
  mock_device_.SetAttribute(Attributes::bBackgroundOpStatus,
                            static_cast<uint32_t>(BackgroundOpStatus::kNotRequired));

  // 2. WriteBooster flush hold-off (dCurrentWBBufferSize > 0 && bAvailableWBBufferSize < 4).
  mock_device_.SetAttribute(Attributes::dCurrentWBBufferSize, 1);
  mock_device_.SetAttribute(Attributes::bAvailableWBBufferSize, 3);
  ASSERT_NO_FATAL_FAILURE(
      run_abort_case(Attributes::dCurrentWBBufferSize, /*hold_transfer_slot=*/false));
  EXPECT_TRUE(mock_device_.GetFlag(Flags::fWBBufferFlushEn));

  // 3. BlockSlotAllocation(100ms) drain timeout hold-off while an I/O slot remains allocated.
  mock_device_.SetAttribute(Attributes::bAvailableWBBufferSize, 10);
  ASSERT_NO_FATAL_FAILURE(
      run_abort_case(Attributes::bBackgroundOpStatus, /*hold_transfer_slot=*/true));
  EXPECT_FALSE(mock_device_.GetFlag(Flags::fWBBufferFlushEn));
}

using PhyPowerState = fuchsia_hardware_ufs_phy::wire::PowerState;

constexpr uint64_t kExpectedActiveBandwidthBps = 1'000'000'000;
constexpr uint64_t kExpectedIdleBandwidthBps = 0;
constexpr uint32_t kExpectedInterconnectTag = 'UFS ';

enum class PlatformEventKind : uint8_t {
  kPhyInit,
  kPhyPrePowerModeChange,
  kPhyGetProperties,
  kPhySetPowerState,
  kInterconnectSetBandwidth,
};

struct PlatformEvent {
  PlatformEventKind kind;
  PhyPowerState phy_state = PhyPowerState::kActive;
  uint64_t avg_bandwidth_bps = 0;
  uint64_t peak_bandwidth_bps = 0;
  uint32_t tag = 0;
  zx_status_t status = ZX_OK;
};

class PlatformEventLog final {
 public:
  void Record(const PlatformEvent& event) {
    std::scoped_lock lock(lock_);
    events_.push_back(event);
  }

  std::vector<PlatformEvent> TakeEvents() {
    std::scoped_lock lock(lock_);
    std::vector<PlatformEvent> out;
    out.swap(events_);
    return out;
  }

 private:
  mutable std::mutex lock_;
  std::vector<PlatformEvent> events_;
};

class FakeUfsPhy final : public fidl::WireServer<fuchsia_hardware_ufs_phy::UfsPhy> {
 public:
  explicit FakeUfsPhy(PlatformEventLog* log) : log_(log) {}

  fuchsia_hardware_ufs_phy::Service::InstanceHandler GetInstanceHandler() {
    return fuchsia_hardware_ufs_phy::Service::InstanceHandler({
        .phy = bindings_.CreateHandler(this, fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                       fidl::kIgnoreBindingClosure),
    });
  }

  void set_supports_suspend_resume(bool supports) {
    std::scoped_lock lock(lock_);
    supports_suspend_resume_ = supports;
  }

  void set_reject_runtime_low_power(bool reject) {
    std::scoped_lock lock(lock_);
    reject_runtime_low_power_ = reject;
  }

  void SetNextError(zx_status_t status) {
    std::scoped_lock lock(lock_);
    next_error_ = status;
  }

  PhyPowerState current_state() const {
    std::scoped_lock lock(lock_);
    return current_state_;
  }

  void Init(InitRequestView request, InitCompleter::Sync& completer) override {
    {
      std::scoped_lock lock(lock_);
      ufshci_client_ = std::move(request->ufs_hci);
    }
    log_->Record({.kind = PlatformEventKind::kPhyInit});
    completer.ReplySuccess();
  }

  void PrePowerModeChange(PrePowerModeChangeCompleter::Sync& completer) override {
    log_->Record({.kind = PlatformEventKind::kPhyPrePowerModeChange});
    completer.ReplySuccess();
  }

  void GetProperties(GetPropertiesCompleter::Sync& completer) override {
    bool supports = false;
    {
      std::scoped_lock lock(lock_);
      supports = supports_suspend_resume_;
    }
    log_->Record({.kind = PlatformEventKind::kPhyGetProperties});
    fidl::Arena arena;
    completer.Reply(fuchsia_hardware_ufs_phy::wire::UfsPhyGetPropertiesResponse::Builder(arena)
                        .supports_suspend_resume(supports)
                        .Build());
  }

  void SetPowerState(SetPowerStateRequestView request,
                     SetPowerStateCompleter::Sync& completer) override {
    zx_status_t status = ZX_OK;
    {
      std::scoped_lock lock(lock_);
      if (next_error_.has_value()) {
        status = *next_error_;
        next_error_.reset();
        // Per fuchsia.hardware.ufs.phy/UfsPhy.SetPowerState, POWER_DOWN attempts all power-down
        // steps and transitions to POWER_DOWN even when returning the first step error.
        if (request->state == PhyPowerState::kPowerDown) {
          current_state_ = PhyPowerState::kPowerDown;
        }
      } else if (reject_runtime_low_power_ && (request->state == PhyPowerState::kLinkIdle ||
                                               request->state == PhyPowerState::kHibernateSleep)) {
        status = ZX_ERR_NOT_SUPPORTED;
      } else {
        current_state_ = request->state;
      }
    }
    log_->Record({
        .kind = PlatformEventKind::kPhySetPowerState,
        .phy_state = request->state,
        .status = status,
    });
    if (status == ZX_OK) {
      completer.ReplySuccess();
    } else {
      completer.ReplyError(status);
    }
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_hardware_ufs_phy::UfsPhy> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

 private:
  mutable std::mutex lock_;
  PlatformEventLog* log_;
  fidl::ServerBindingGroup<fuchsia_hardware_ufs_phy::UfsPhy> bindings_;
  fidl::ClientEnd<fuchsia_hardware_ufs_phy::Ufshci> ufshci_client_;
  bool supports_suspend_resume_ = true;
  bool reject_runtime_low_power_ = false;
  std::optional<zx_status_t> next_error_;
  PhyPowerState current_state_ = PhyPowerState::kActive;
};

class FakeInterconnectPath final : public fidl::WireServer<fuchsia_hardware_interconnect::Path> {
 public:
  explicit FakeInterconnectPath(PlatformEventLog* log) : log_(log) {}

  fuchsia_hardware_interconnect::PathService::InstanceHandler GetInstanceHandler() {
    return fuchsia_hardware_interconnect::PathService::InstanceHandler({
        .path = bindings_.CreateHandler(this, fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                        fidl::kIgnoreBindingClosure),
    });
  }

  void SetNextError(zx_status_t status) {
    std::scoped_lock lock(lock_);
    next_error_ = status;
  }

  uint64_t current_avg_bandwidth_bps() const {
    std::scoped_lock lock(lock_);
    return current_avg_bandwidth_bps_;
  }

  void SetBandwidth(SetBandwidthRequestView request,
                    SetBandwidthCompleter::Sync& completer) override {
    const uint64_t avg =
        request->has_average_bandwidth_bps() ? request->average_bandwidth_bps() : 0;
    const uint64_t peak = request->has_peak_bandwidth_bps() ? request->peak_bandwidth_bps() : 0;
    const uint32_t tag = request->has_tag() ? request->tag() : 0;
    zx_status_t status = ZX_OK;
    {
      std::scoped_lock lock(lock_);
      if (next_error_.has_value()) {
        status = *next_error_;
        next_error_.reset();
      } else {
        current_avg_bandwidth_bps_ = avg;
      }
    }
    log_->Record({
        .kind = PlatformEventKind::kInterconnectSetBandwidth,
        .avg_bandwidth_bps = avg,
        .peak_bandwidth_bps = peak,
        .tag = tag,
        .status = status,
    });
    if (status == ZX_OK) {
      completer.ReplySuccess();
    } else {
      completer.ReplyError(status);
    }
  }

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_interconnect::Path> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {}

 private:
  mutable std::mutex lock_;
  PlatformEventLog* log_;
  fidl::ServerBindingGroup<fuchsia_hardware_interconnect::Path> bindings_;
  std::optional<zx_status_t> next_error_;
  uint64_t current_avg_bandwidth_bps_ = 0;
};

class TestUfsPdev final : public UfsPdev {
 public:
  using UfsPdev::UfsPdev;
  ~TestUfsPdev() override = default;

  static DriverRegistration GetDriverRegistration() {
    return FUCHSIA_DRIVER_REGISTRATION_V1(fdf_internal::DriverServer2<TestUfsPdev>::initialize,
                                          fdf_internal::DriverServer2<TestUfsPdev>::destroy);
  }

  static void SetMockDevice(ufs_mock_device::UfsMockDevice* mock_device) {
    mock_device_ = mock_device;
  }

 protected:
  zx::result<fdf::MmioBuffer> CreateMmioBuffer(zx_off_t offset, size_t size, zx::vmo vmo) override {
    ZX_ASSERT(mock_device_ != nullptr);
    return zx::ok(mock_device_->GetMmioBuffer(std::move(vmo)));
  }

 private:
  inline static ufs_mock_device::UfsMockDevice* mock_device_ = nullptr;
};

class FakePdev final
    : public fidl::testing::WireTestBase<fuchsia_hardware_platform_device::Device> {
 public:
  fuchsia_hardware_platform_device::Service::InstanceHandler GetInstanceHandler(
      async_dispatcher_t* dispatcher) {
    return fuchsia_hardware_platform_device::Service::InstanceHandler({
        .device = bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure),
    });
  }

  void SetMockDevice(ufs_mock_device::UfsMockDevice* mock_device) { mock_device_ = mock_device; }

  void GetMmioById(GetMmioByIdRequestView request, GetMmioByIdCompleter::Sync& completer) override {
    if (mock_device_ == nullptr || request->index != 0) {
      completer.ReplyError(ZX_ERR_NOT_FOUND);
      return;
    }
    fidl::Arena arena;
    completer.ReplySuccess(fuchsia_hardware_platform_device::wire::Mmio::Builder(arena)
                               .offset(0)
                               .size(RegisterMap::kRegisterSize)
                               .vmo(mock_device_->GetVmo())
                               .Build());
  }

  void GetBtiById(GetBtiByIdRequestView request, GetBtiByIdCompleter::Sync& completer) override {
    if (mock_device_ == nullptr || request->index != 0) {
      completer.ReplyError(ZX_ERR_NOT_FOUND);
      return;
    }
    completer.ReplySuccess(mock_device_->GetFakeBti());
  }

  void GetInterruptById(GetInterruptByIdRequestView request,
                        GetInterruptByIdCompleter::Sync& completer) override {
    if (mock_device_ == nullptr || request->index != 0) {
      completer.ReplyError(ZX_ERR_NOT_FOUND);
      return;
    }
    completer.ReplySuccess(mock_device_->GetIrq());
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    ADD_FAILURE() << name << " is not implemented";
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_platform_device::Device> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {}

 private:
  fidl::ServerBindingGroup<fuchsia_hardware_platform_device::Device> bindings_;
  ufs_mock_device::UfsMockDevice* mock_device_ = nullptr;
};

class UfsPdevEnvironment final : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();
    device_server_.Initialize(component::kDefaultInstance);
    device_server_.Serve(dispatcher, &to_driver_vfs);

    EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_platform_device::Service>(
        pdev_server_.GetInstanceHandler(dispatcher), "pdev"));
    EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_ufs_phy::Service>(phy_.GetInstanceHandler(),
                                                                          "phy"));
    EXPECT_OK(to_driver_vfs.AddService<fuchsia_hardware_interconnect::PathService>(
        interconnect_.GetInstanceHandler(), "ufs-interconnect"));
    EXPECT_OK(
        to_driver_vfs.component().AddUnmanagedProtocol<fuchsia_power_system::CpuElementManager>(
            cpu_element_manager_.CreateHandler()));
    return zx::ok();
  }

  void ConfigureMockDevice(ufs_mock_device::UfsMockDevice& mock_device) {
    pdev_server_.SetMockDevice(&mock_device);
  }

  PlatformEventLog& event_log() { return event_log_; }
  FakeUfsPhy& phy() { return phy_; }
  FakeInterconnectPath& interconnect() { return interconnect_; }
  FakeCpuElementManager& cpu_element_manager() { return cpu_element_manager_; }
  FakePowerBroker& power_broker() { return power_broker_; }

 private:
  PlatformEventLog event_log_;
  FakePdev pdev_server_;
  FakeUfsPhy phy_{&event_log_};
  FakeInterconnectPath interconnect_{&event_log_};
  compat::DeviceServer device_server_;
  FakeCpuElementManager cpu_element_manager_;
  FakePowerBroker power_broker_;
};

struct UfsPdevTestConfig final {
  using DriverType = TestUfsPdev;
  using EnvironmentType = UfsPdevEnvironment;
};

class UfsPdevPowerTest : public ::testing::Test {
 protected:
  void SetUp() override {
    auto irq = std::make_unique<zx::interrupt>();
    ASSERT_OK(zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, irq.get()));
    mock_device_.Init(std::move(irq));
    ASSERT_OK(mock_device_.AddLun(0));
    CapabilityReg::Get()
        .ReadFrom(mock_device_.GetRegisters())
        .set_auto_hibernation_support(true)
        .WriteTo(mock_device_.GetRegisters());
  }

  void TearDown() override {
    if (driver_started_ && !driver_stopped_) {
      ASSERT_OK(driver_test_.StopDriver());
    }
    TestUfsPdev::SetMockDevice(nullptr);
    mock_device_.GetRegisterMmioProcessor().Reset();
    mock_device_.GetUicCmdProcessor().Reset();
    mock_device_.GetTransferRequestProcessor().Reset();
    mock_device_.GetTaskManagementRequestProcessor().Reset();
    mock_device_.GetQueryRequestProcessor().Reset();
    mock_device_.GetScsiCommandProcessor().Reset();
    mock_device_.GetDmaHandler().Reset();
    mock_device_.GetRegisters()->Reset();
  }

  void StartPdevDriver(bool supply_power_framework = true) {
    driver_test_.RunInEnvironmentTypeContext(
        [this](UfsPdevEnvironment& env) { env.ConfigureMockDevice(mock_device_); });
    TestUfsPdev::SetMockDevice(&mock_device_);

    std::optional<fuchsia_driver_framework::PowerElementArgs> power_args;
    if (supply_power_framework) {
      auto [control_client, control_server] =
          fidl::Endpoints<fuchsia_power_broker::ElementControl>::Create();
      auto [runner_client, runner_server] =
          fidl::Endpoints<fuchsia_power_broker::ElementRunner>::Create();
      auto [lessor_client, lessor_server] = fidl::Endpoints<fuchsia_power_broker::Lessor>::Create();
      fuchsia_power_broker::DependencyToken token;
      ASSERT_OK(zx::event::create(0, &token));

      fuchsia_driver_framework::PowerElementArgs local_args;
      local_args.control_client() = std::move(control_client);
      local_args.runner_server() = std::move(runner_server);
      local_args.lessor_client() = std::move(lessor_client);
      local_args.token() = std::move(token);
      power_args = std::move(local_args);

      driver_test_.RunInEnvironmentTypeContext(
          [control = std::move(control_server), runner = std::move(runner_client),
           lessor = std::move(lessor_server)](UfsPdevEnvironment& env) mutable {
            env.power_broker().AddHardwarePowerElement(std::move(control), std::move(runner),
                                                       std::move(lessor));
          });
    }

    ASSERT_OK(driver_test_.StartDriverWithCustomStartArgs([&](fdf::DriverStartArgs& args) {
      ufs_config::Config config;
      config.enable_suspend() = supply_power_framework;
      config.storage_power_management_enabled() = supply_power_framework;
      config.enable_auto_hibernate() = true;
      config.auto_hibernate_timer_us() = 1000;
      config.link_idle_ahit_multiplier() = 2;
      args.config(config.ToVmo());
      args.power_element_args(std::move(power_args));
    }));

    driver_started_ = true;
    dut_ = driver_test_.driver();
    if (supply_power_framework) {
      driver_test_.RunInEnvironmentTypeContext([](UfsPdevEnvironment& env) {
        EXPECT_TRUE(env.cpu_element_manager().execution_state_dependency_added());
      });
    }
  }

  void StopPdevDriver() {
    ASSERT_TRUE(driver_started_);
    ASSERT_FALSE(driver_stopped_);
    ASSERT_OK(driver_test_.StopDriver());
    driver_stopped_ = true;
  }

  void SetPowerLevelSync(fuchsia_power_broker::PowerLevel level) {
    auto replied = std::make_shared<libsync::Completion>();
    driver_test_.RunInEnvironmentTypeContext([level, replied](UfsPdevEnvironment& env) {
      env.power_broker().hardware_power_element_runner_client_->SetLevel({level}).ThenExactlyOnce(
          [replied](fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel>& result) {
            EXPECT_TRUE(result.is_ok());
            replied->Signal();
          });
    });
    EXPECT_OK(driver_test_.RunOnBackgroundDispatcherSync([&] { replied->Wait(); }));
  }

  void ParkLinkInAutoHibernate() {
    HostControllerStatusReg::Get()
        .ReadFrom(mock_device_.GetRegisters())
        .set_device_present(false)
        .set_utp_transfer_request_list_ready(false)
        .WriteTo(mock_device_.GetRegisters());
  }

  void WaitForLinkIdle(bool idle) const {
    const zx::time deadline = zx::deadline_after(zx::sec(5));
    while (dut_->IsLinkIdle() != idle && zx::clock::get_monotonic() < deadline) {
      zx::nanosleep(zx::deadline_after(zx::msec(1)));
    }
    EXPECT_EQ(dut_->IsLinkIdle(), idle);
  }

  std::vector<PlatformEvent> TakeEvents() {
    return driver_test_.RunInEnvironmentTypeContext<std::vector<PlatformEvent>>(
        [](UfsPdevEnvironment& env) { return env.event_log().TakeEvents(); });
  }

  fdf_testing::ForegroundDriverTest<UfsPdevTestConfig>& driver_test() { return driver_test_; }
  TestUfsPdev* dut() const { return dut_; }

  static void ExpectBandwidthEvent(const PlatformEvent& ev, uint64_t expected_bps,
                                   zx_status_t expected_status = ZX_OK) {
    EXPECT_EQ(ev.kind, PlatformEventKind::kInterconnectSetBandwidth);
    EXPECT_EQ(ev.avg_bandwidth_bps, expected_bps);
    EXPECT_EQ(ev.peak_bandwidth_bps, expected_bps);
    EXPECT_EQ(ev.tag, kExpectedInterconnectTag);
    EXPECT_EQ(ev.status, expected_status);
  }

  static void ExpectPhyStateEvent(const PlatformEvent& ev, PhyPowerState expected_state,
                                  zx_status_t expected_status = ZX_OK) {
    EXPECT_EQ(ev.kind, PlatformEventKind::kPhySetPowerState);
    EXPECT_EQ(ev.phy_state, expected_state);
    EXPECT_EQ(ev.status, expected_status);
  }

 private:
  // mock_device_ MUST precede driver_test_ so driver_test_ shuts down before mock_device_ resets.
  ufs_mock_device::UfsMockDevice mock_device_;
  fdf_testing::ForegroundDriverTest<UfsPdevTestConfig> driver_test_;
  TestUfsPdev* dut_ = nullptr;
  bool driver_started_ = false;
  bool driver_stopped_ = false;
};

// Scenario 1 & 2: Boot InitResources vote, runtime kLinkIdle <-> kLinkActive ordering,
// redundant-vote deduplication, and composite latency histogram recording.
TEST_F(UfsPdevPowerTest, BootVoteAndRuntimeLinkIdleActiveBandwidthOrderingAndDedup) {
  ASSERT_NO_FATAL_FAILURE(StartPdevDriver(/*supply_power_framework=*/true));
  dut()->SetLinkIdleTimeoutForTesting(zx::msec(10));
  dut()->SetRuntimeSleepTimeoutForTesting(zx::sec(60));
  EXPECT_TRUE(dut()->SupportsPowerDownSuspend());
  EXPECT_TRUE(dut()->SupportsRuntimePowerManagement());

  // 1. Boot / InitResources must issue exactly one 1 GB/s SetBandwidth('UFS ') vote.
  const std::vector<PlatformEvent> boot_events = TakeEvents();
  size_t boot_bw_calls = 0;
  for (const auto& ev : boot_events) {
    if (ev.kind == PlatformEventKind::kInterconnectSetBandwidth) {
      ExpectBandwidthEvent(ev, kExpectedActiveBandwidthBps);
      ++boot_bw_calls;
    }
  }
  EXPECT_EQ(boot_bw_calls, 1u);

  // 2. Runtime link-idle timer: SetPowerState(kLinkIdle) MUST precede SetBandwidth(0).
  ParkLinkInAutoHibernate();
  dut()->ScheduleRuntimeIdleTimers();
  WaitForLinkIdle(true);
  std::vector<PlatformEvent> events = TakeEvents();
  ASSERT_EQ(events.size(), 2u);
  ExpectPhyStateEvent(events[0], PhyPowerState::kLinkIdle);
  ExpectBandwidthEvent(events[1], kExpectedIdleBandwidthBps);
  EXPECT_GE(ReadUfsInspectHistogramSampleCount(*dut(), "link_idle_latency_us"), 1u);

  // Duplicate kLinkIdle while already at 0 bps must emit 0 SetBandwidth IPCs.
  ASSERT_OK(dut()->Notify(NotifyEvent::kLinkIdle, 0));
  events = TakeEvents();
  ASSERT_EQ(events.size(), 1u);
  ExpectPhyStateEvent(events[0], PhyPowerState::kLinkIdle);

  // 3. ScopedLinkActive wake: SetBandwidth(1 GB/s) MUST precede SetPowerState(kActive).
  dut()->SetLinkIdleTimeoutForTesting(zx::sec(60));
  {
    Ufs::ScopedLinkActive guard(*dut());
    EXPECT_FALSE(dut()->IsLinkIdle());
    events = TakeEvents();
    ASSERT_EQ(events.size(), 2u);
    ExpectBandwidthEvent(events[0], kExpectedActiveBandwidthBps);
    ExpectPhyStateEvent(events[1], PhyPowerState::kActive);
    EXPECT_GE(ReadUfsInspectHistogramSampleCount(*dut(), "link_active_latency_us"), 1u);

    // Duplicate kLinkActive while already at 1 GB/s must emit 0 SetBandwidth IPCs.
    ASSERT_OK(dut()->Notify(NotifyEvent::kLinkActive, 0));
    events = TakeEvents();
    ASSERT_EQ(events.size(), 1u);
    ExpectPhyStateEvent(events[0], PhyPowerState::kActive);
  }
}

// Scenario 3: Direct kLinkIdle -> kEnterSleep zero-duplicate-IPC transition, kExitSleep ordering,
// and full end-to-end DeviceManager::EnterRuntimeSleep / ExitRuntimeSleep flow.
TEST_F(UfsPdevPowerTest, RuntimeSleepBandwidthDedupFromLinkIdleAndEndToEndFlow) {
  ASSERT_NO_FATAL_FAILURE(StartPdevDriver(/*supply_power_framework=*/true));
  dut()->SetLinkIdleTimeoutForTesting(zx::sec(60));
  dut()->SetRuntimeSleepTimeoutForTesting(zx::sec(60));
  static_cast<void>(TakeEvents());

  // 1. Direct kLinkIdle -> kEnterSleep must NOT issue a duplicate SetBandwidth(0) IPC.
  ASSERT_OK(dut()->Notify(NotifyEvent::kLinkIdle, 0));
  std::vector<PlatformEvent> events = TakeEvents();
  ASSERT_EQ(events.size(), 2u);
  ExpectPhyStateEvent(events[0], PhyPowerState::kLinkIdle);
  ExpectBandwidthEvent(events[1], kExpectedIdleBandwidthBps);

  ASSERT_OK(dut()->Notify(NotifyEvent::kEnterSleep, 0));
  events = TakeEvents();
  ASSERT_EQ(events.size(), 1u);
  ExpectPhyStateEvent(events[0], PhyPowerState::kHibernateSleep);

  // kExitSleep must restore 1 GB/s strictly BEFORE SetPowerState(kActive).
  ASSERT_OK(dut()->Notify(NotifyEvent::kExitSleep, 0));
  events = TakeEvents();
  ASSERT_EQ(events.size(), 2u);
  ExpectBandwidthEvent(events[0], kExpectedActiveBandwidthBps);
  ExpectPhyStateEvent(events[1], PhyPowerState::kActive);

  // 2. Full DeviceManager::EnterRuntimeSleep() -> ExitRuntimeSleep() driver flow.
  ASSERT_OK(dut()->GetDeviceManager().EnterRuntimeSleep());
  EXPECT_TRUE(dut()->GetDeviceManager().IsRuntimeSleeping());
  EXPECT_EQ(dut()->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kSleep);
  events = TakeEvents();
  ASSERT_EQ(events.size(), 2u);
  ExpectPhyStateEvent(events[0], PhyPowerState::kHibernateSleep);
  ExpectBandwidthEvent(events[1], kExpectedIdleBandwidthBps);

  ASSERT_OK(dut()->GetDeviceManager().ExitRuntimeSleep());
  EXPECT_FALSE(dut()->GetDeviceManager().IsRuntimeSleeping());
  EXPECT_EQ(dut()->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);
  events = TakeEvents();
  ASSERT_EQ(events.size(), 2u);
  ExpectBandwidthEvent(events[0], kExpectedActiveBandwidthBps);
  ExpectPhyStateEvent(events[1], PhyPowerState::kActive);
}

// Scenario 4a: System PowerDown suspend/resume (PostSuspend / PreResume) deduplication and
// StopResources() teardown vote.
TEST_F(UfsPdevPowerTest, SystemPowerDownSuspendResumeDedupAndStopResources) {
  ASSERT_NO_FATAL_FAILURE(StartPdevDriver(/*supply_power_framework=*/true));
  dut()->SetLinkIdleTimeoutForTesting(zx::sec(60));
  dut()->SetRuntimeSleepTimeoutForTesting(zx::sec(60));
  static_cast<void>(TakeEvents());

  // Arm Power Broker ("on") then suspend ("off"): PostSuspend orders kPowerDown -> SetBandwidth(0).
  SetPowerLevelSync(Ufs::kPowerLevelOn);
  ASSERT_TRUE(dut()->IsResumed());
  SetPowerLevelSync(Ufs::kPowerLevelOff);
  ASSERT_FALSE(dut()->IsResumed());
  EXPECT_EQ(dut()->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kPowerDown);
  std::vector<PlatformEvent> events = TakeEvents();
  ASSERT_EQ(events.size(), 2u);
  ExpectPhyStateEvent(events[0], PhyPowerState::kPowerDown);
  ExpectBandwidthEvent(events[1], kExpectedIdleBandwidthBps);

  // Resume ("on"): PreResume restores 1 GB/s BEFORE SetPowerState(kActive), and subsequent
  // ScopedLinkActive guards during controller re-init must emit 0 duplicate SetBandwidth IPCs.
  SetPowerLevelSync(Ufs::kPowerLevelOn);
  ASSERT_TRUE(dut()->IsResumed());
  EXPECT_EQ(dut()->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kActive);
  events = TakeEvents();
  ASSERT_GE(events.size(), 2u);
  ExpectBandwidthEvent(events[0], kExpectedActiveBandwidthBps);
  ExpectPhyStateEvent(events[1], PhyPowerState::kActive);
  size_t resume_bw_calls = 0;
  for (const auto& ev : events) {
    if (ev.kind == PlatformEventKind::kInterconnectSetBandwidth) {
      ++resume_bw_calls;
    }
  }
  EXPECT_EQ(resume_bw_calls, 1u);

  // StopDriver() -> UfsPdev::StopResources() must drop the interconnect vote to 0 bps.
  ASSERT_NO_FATAL_FAILURE(StopPdevDriver());
  events = TakeEvents();
  ASSERT_EQ(events.size(), 1u);
  ExpectBandwidthEvent(events[0], kExpectedIdleBandwidthBps);
}

// Scenario 4b: When the PHY does not support PowerDown suspend/resume, Sleep-mode system
// suspend/resume still drops and restores the interconnect bandwidth vote via kEnterSleep /
// kExitSleep.
TEST_F(UfsPdevPowerTest, SleepSystemSuspendWithoutPhyStillManagesInterconnectBandwidth) {
  driver_test().RunInEnvironmentTypeContext(
      [](UfsPdevEnvironment& env) { env.phy().set_supports_suspend_resume(false); });
  ASSERT_NO_FATAL_FAILURE(StartPdevDriver(/*supply_power_framework=*/true));
  EXPECT_FALSE(dut()->SupportsPowerDownSuspend());
  EXPECT_FALSE(dut()->SupportsRuntimePowerManagement());
  static_cast<void>(TakeEvents());

  SetPowerLevelSync(Ufs::kPowerLevelOn);
  SetPowerLevelSync(Ufs::kPowerLevelOff);
  ASSERT_FALSE(dut()->IsResumed());
  EXPECT_EQ(dut()->GetDeviceManager().GetCurrentPowerMode(), UfsPowerMode::kSleep);
  std::vector<PlatformEvent> events = TakeEvents();
  ASSERT_EQ(events.size(), 2u);
  ExpectPhyStateEvent(events[0], PhyPowerState::kHibernateSleep);
  ExpectBandwidthEvent(events[1], kExpectedIdleBandwidthBps);

  SetPowerLevelSync(Ufs::kPowerLevelOn);
  ASSERT_TRUE(dut()->IsResumed());
  events = TakeEvents();
  ASSERT_EQ(events.size(), 2u);
  ExpectBandwidthEvent(events[0], kExpectedActiveBandwidthBps);
  ExpectPhyStateEvent(events[1], PhyPowerState::kActive);
}

// Scenario 5: ZX_ERR_NOT_SUPPORTED PHY rejection (kLinkIdle keeps 1 GB/s while kEnterSleep still
// drops to 0 bps), SetBandwidth(0) error rollback to kActive, SetBandwidth(1 GB/s) wake error
// still restoring PHY kActive, PostSuspend PHY error still dropping to 0 bps, and multi-thread
// concurrency serialization under power_state_lock_.
TEST_F(UfsPdevPowerTest, PhyNotSupportedAndSetBandwidthErrorRollbackAndConcurrency) {
  ASSERT_NO_FATAL_FAILURE(StartPdevDriver(/*supply_power_framework=*/true));
  dut()->SetLinkIdleTimeoutForTesting(zx::sec(60));
  dut()->SetRuntimeSleepTimeoutForTesting(zx::sec(60));
  static_cast<void>(TakeEvents());

  // 1. ZX_ERR_NOT_SUPPORTED on kLinkIdle must NOT drop interconnect bandwidth (link stays active)
  //    and must disable SupportsRuntimePowerManagement(), whereas kEnterSleep MUST still drop
  //    interconnect bandwidth to 0 bps (without spurious PHY rollback on SetBandwidth error) and
  //    kExitSleep MUST restore 1 GB/s.
  driver_test().RunInEnvironmentTypeContext(
      [](UfsPdevEnvironment& env) { env.phy().set_reject_runtime_low_power(true); });
  EXPECT_STATUS(dut()->Notify(NotifyEvent::kLinkIdle, 0).status_value(), ZX_ERR_NOT_SUPPORTED);
  EXPECT_FALSE(dut()->SupportsRuntimePowerManagement());
  std::vector<PlatformEvent> events = TakeEvents();
  ASSERT_EQ(events.size(), 1u);
  ExpectPhyStateEvent(events[0], PhyPowerState::kLinkIdle, ZX_ERR_NOT_SUPPORTED);
  driver_test().RunInEnvironmentTypeContext([](UfsPdevEnvironment& env) {
    EXPECT_EQ(env.interconnect().current_avg_bandwidth_bps(), kExpectedActiveBandwidthBps);
    env.interconnect().SetNextError(ZX_ERR_IO);
  });

  EXPECT_STATUS(dut()->Notify(NotifyEvent::kEnterSleep, 0).status_value(), ZX_ERR_IO);
  events = TakeEvents();
  ASSERT_EQ(events.size(), 2u);
  ExpectPhyStateEvent(events[0], PhyPowerState::kHibernateSleep, ZX_ERR_NOT_SUPPORTED);
  ExpectBandwidthEvent(events[1], kExpectedIdleBandwidthBps, ZX_ERR_IO);

  ASSERT_OK(dut()->Notify(NotifyEvent::kEnterSleep, 0));
  events = TakeEvents();
  ASSERT_EQ(events.size(), 2u);
  ExpectPhyStateEvent(events[0], PhyPowerState::kHibernateSleep, ZX_ERR_NOT_SUPPORTED);
  ExpectBandwidthEvent(events[1], kExpectedIdleBandwidthBps, ZX_OK);
  driver_test().RunInEnvironmentTypeContext([](UfsPdevEnvironment& env) {
    EXPECT_EQ(env.phy().current_state(), PhyPowerState::kActive);
    EXPECT_EQ(env.interconnect().current_avg_bandwidth_bps(), kExpectedIdleBandwidthBps);
  });

  ASSERT_OK(dut()->Notify(NotifyEvent::kExitSleep, 0));
  events = TakeEvents();
  ASSERT_EQ(events.size(), 2u);
  ExpectBandwidthEvent(events[0], kExpectedActiveBandwidthBps, ZX_OK);
  ExpectPhyStateEvent(events[1], PhyPowerState::kActive, ZX_OK);
  driver_test().RunInEnvironmentTypeContext([](UfsPdevEnvironment& env) {
    EXPECT_EQ(env.interconnect().current_avg_bandwidth_bps(), kExpectedActiveBandwidthBps);
    env.phy().set_reject_runtime_low_power(false);
  });

  // 2. SetBandwidth(0) failure during kLinkIdle and kEnterSleep MUST roll PHY back to kActive.
  for (auto [event, target_state] : {
           std::pair{NotifyEvent::kLinkIdle, PhyPowerState::kLinkIdle},
           std::pair{NotifyEvent::kEnterSleep, PhyPowerState::kHibernateSleep},
       }) {
    driver_test().RunInEnvironmentTypeContext(
        [](UfsPdevEnvironment& env) { env.interconnect().SetNextError(ZX_ERR_IO); });
    EXPECT_STATUS(dut()->Notify(event, 0).status_value(), ZX_ERR_IO);
    events = TakeEvents();
    ASSERT_EQ(events.size(), 3u);
    ExpectPhyStateEvent(events[0], target_state, ZX_OK);
    ExpectBandwidthEvent(events[1], kExpectedIdleBandwidthBps, ZX_ERR_IO);
    ExpectPhyStateEvent(events[2], PhyPowerState::kActive, ZX_OK);
    driver_test().RunInEnvironmentTypeContext([](UfsPdevEnvironment& env) {
      EXPECT_EQ(env.phy().current_state(), PhyPowerState::kActive);
      EXPECT_EQ(env.interconnect().current_avg_bandwidth_bps(), kExpectedActiveBandwidthBps);
    });
  }

  // 3. Challenge 1: SetBandwidth(1 GB/s) failure on kLinkActive (from kLinkIdle) or kExitSleep
  //    (from kHibernateSleep) MUST still transition the PHY to kActive after the attempted
  //    SetBandwidth call, leave bandwidth at 0 bps, and retry SetBandwidth(1 GB/s) on the next
  //    kLinkActive notification.
  struct WakeErrorCase {
    NotifyEvent enter_event;
    PhyPowerState low_power_state;
    NotifyEvent wake_event;
  };
  for (const WakeErrorCase tc : {
           WakeErrorCase{
               .enter_event = NotifyEvent::kLinkIdle,
               .low_power_state = PhyPowerState::kLinkIdle,
               .wake_event = NotifyEvent::kLinkActive,
           },
           WakeErrorCase{
               .enter_event = NotifyEvent::kEnterSleep,
               .low_power_state = PhyPowerState::kHibernateSleep,
               .wake_event = NotifyEvent::kExitSleep,
           },
       }) {
    ASSERT_OK(dut()->Notify(tc.enter_event, 0));
    events = TakeEvents();
    ASSERT_EQ(events.size(), 2u);
    ExpectPhyStateEvent(events[0], tc.low_power_state, ZX_OK);
    ExpectBandwidthEvent(events[1], kExpectedIdleBandwidthBps, ZX_OK);

    driver_test().RunInEnvironmentTypeContext(
        [](UfsPdevEnvironment& env) { env.interconnect().SetNextError(ZX_ERR_IO); });
    EXPECT_STATUS(dut()->Notify(tc.wake_event, 0).status_value(), ZX_ERR_IO);
    events = TakeEvents();
    ASSERT_EQ(events.size(), 2u);
    ExpectBandwidthEvent(events[0], kExpectedActiveBandwidthBps, ZX_ERR_IO);
    ExpectPhyStateEvent(events[1], PhyPowerState::kActive, ZX_OK);
    driver_test().RunInEnvironmentTypeContext([](UfsPdevEnvironment& env) {
      EXPECT_EQ(env.phy().current_state(), PhyPowerState::kActive);
      EXPECT_EQ(env.interconnect().current_avg_bandwidth_bps(), kExpectedIdleBandwidthBps);
    });

    ASSERT_OK(dut()->Notify(NotifyEvent::kLinkActive, 0));
    events = TakeEvents();
    ASSERT_EQ(events.size(), 2u);
    ExpectBandwidthEvent(events[0], kExpectedActiveBandwidthBps, ZX_OK);
    ExpectPhyStateEvent(events[1], PhyPowerState::kActive, ZX_OK);
    driver_test().RunInEnvironmentTypeContext([](UfsPdevEnvironment& env) {
      EXPECT_EQ(env.phy().current_state(), PhyPowerState::kActive);
      EXPECT_EQ(env.interconnect().current_avg_bandwidth_bps(), kExpectedActiveBandwidthBps);
    });
  }

  // 4. Challenge 2: SetPowerState(kPowerDown) error during kPostSuspend MUST still drop
  //    interconnect bandwidth to 0 bps while returning the PHY error, and kPreResume restores
  //    1 GB/s and PHY kActive.
  driver_test().RunInEnvironmentTypeContext(
      [](UfsPdevEnvironment& env) { env.phy().SetNextError(ZX_ERR_IO); });
  EXPECT_STATUS(dut()->Notify(NotifyEvent::kPostSuspend, 0).status_value(), ZX_ERR_IO);
  events = TakeEvents();
  ASSERT_EQ(events.size(), 2u);
  ExpectPhyStateEvent(events[0], PhyPowerState::kPowerDown, ZX_ERR_IO);
  ExpectBandwidthEvent(events[1], kExpectedIdleBandwidthBps, ZX_OK);
  driver_test().RunInEnvironmentTypeContext([](UfsPdevEnvironment& env) {
    EXPECT_EQ(env.phy().current_state(), PhyPowerState::kPowerDown);
    EXPECT_EQ(env.interconnect().current_avg_bandwidth_bps(), kExpectedIdleBandwidthBps);
  });

  ASSERT_OK(dut()->Notify(NotifyEvent::kPreResume, 0));
  events = TakeEvents();
  ASSERT_EQ(events.size(), 2u);
  ExpectBandwidthEvent(events[0], kExpectedActiveBandwidthBps, ZX_OK);
  ExpectPhyStateEvent(events[1], PhyPowerState::kActive, ZX_OK);
  driver_test().RunInEnvironmentTypeContext([](UfsPdevEnvironment& env) {
    EXPECT_EQ(env.phy().current_state(), PhyPowerState::kActive);
    EXPECT_EQ(env.interconnect().current_avg_bandwidth_bps(), kExpectedActiveBandwidthBps);
  });

  // 5. Multi-thread concurrent kLinkIdle / kLinkActive stress: zero consecutive duplicate
  //    SetBandwidth votes and strict final (PHY state <-> interconnect bandwidth) consistency.
  std::vector<std::thread> threads;
  threads.reserve(4);
  for (int t = 0; t < 4; ++t) {
    threads.emplace_back([this, t] {
      for (int i = 0; i < 25; ++i) {
        const NotifyEvent ev =
            ((i + t) % 2 == 0) ? NotifyEvent::kLinkIdle : NotifyEvent::kLinkActive;
        EXPECT_OK(dut()->Notify(ev, 0));
      }
    });
  }
  for (auto& th : threads) {
    th.join();
  }
  events = TakeEvents();
  std::optional<uint64_t> prev_bw;
  for (const auto& ev : events) {
    if (ev.kind == PlatformEventKind::kInterconnectSetBandwidth) {
      if (prev_bw.has_value()) {
        EXPECT_NE(*prev_bw, ev.avg_bandwidth_bps);
      }
      prev_bw = ev.avg_bandwidth_bps;
    }
  }
  driver_test().RunInEnvironmentTypeContext([](UfsPdevEnvironment& env) {
    const bool phy_active = env.phy().current_state() == PhyPowerState::kActive;
    const uint64_t expected_bw =
        phy_active ? kExpectedActiveBandwidthBps : kExpectedIdleBandwidthBps;
    EXPECT_EQ(env.interconnect().current_avg_bandwidth_bps(), expected_bw);
  });
}

TEST_F(PowerTest, DisableCompletionSerializesWithInflightIoCompletions) {
  ASSERT_NO_FATAL_FAILURE(StartDriver());
  constexpr uint8_t kTestLun = 0;
  ufs::TransferRequestProcessor& transfer = dut_->GetTransferRequestProcessor();
  ufs::TaskManagementRequestProcessor& task_mgmt = dut_->GetTaskManagementRequestProcessor();

  // 1. DisableCompletion() and EnableCompletion() must serialize on slot_lock_ on both processors.
  for (RequestProcessor* proc : {
           static_cast<RequestProcessor*>(&transfer),
           static_cast<RequestProcessor*>(&task_mgmt),
       }) {
    libsync::Completion disable_entered;
    libsync::Completion disable_done;
    std::unique_lock<std::mutex> hold_lock(proc->GetSlotLock());
    std::thread disabler([&] {
      disable_entered.Signal();
      proc->DisableCompletion();
      disable_done.Signal();
    });
    disable_entered.Wait();
    EXPECT_STATUS(disable_done.Wait(zx::msec(15)), ZX_ERR_TIMED_OUT);
    hold_lock.unlock();
    ASSERT_OK(disable_done.Wait(zx::sec(5)));
    disabler.join();

    libsync::Completion enable_entered;
    libsync::Completion enable_done;
    hold_lock.lock();
    std::thread enabler([&] {
      enable_entered.Signal();
      proc->EnableCompletion();
      enable_done.Signal();
    });
    enable_entered.Wait();
    EXPECT_STATUS(enable_done.Wait(zx::msec(15)), ZX_ERR_TIMED_OUT);
    hold_lock.unlock();
    ASSERT_OK(enable_done.Wait(zx::sec(5)));
    enabler.join();
  }

  uint8_t cdb_buffer[6] = {};
  auto* cdb = reinterpret_cast<scsi::TestUnitReadyCDB*>(cdb_buffer);
  cdb->opcode = scsi::Opcode::TEST_UNIT_READY;
  ScsiCommandUpiu upiu(cdb_buffer, sizeof(*cdb), DataDirection::kNone);

  // Mask host controller interrupts and drain the IRQ, admin, and I/O worker dispatchers across
  // sub-tests 2 and 3 so Ufs::Isr() and Ufs::ProcessIo() cannot race manual completion harvesting.
  // InterruptEnableReg is restored on scope exit before sub-test 4.
  {
    const auto saved_ie = InterruptEnableReg::Get().ReadFrom(&dut_->GetMmio());
    InterruptEnableReg::Get().FromValue(0).WriteTo(&dut_->GetMmio());
    auto restore_ie = fit::defer([&] {
      InterruptStatusReg::Get()
          .FromValue(0)
          .set_utp_transfer_request_completion_status(true)
          .set_utp_task_management_request_completion_status(true)
          .WriteTo(&dut_->GetMmio());
      InterruptEnableReg::Get().FromValue(saved_ie.reg_value()).WriteTo(&dut_->GetMmio());
    });
    {
      dut_->SetHardwarePoweredOff(true);
      auto restore_powered_on = fit::defer([&] { dut_->SetHardwarePoweredOff(false); });
      libsync::Completion io_drained;
      ASSERT_OK(async::PostTask(dut_->io_worker_dispatcher()->async_dispatcher(),
                                [&io_drained] { io_drained.Signal(); }));
      ASSERT_OK(io_drained.Wait(zx::sec(5)));
    }

    // 2. Mid-batch per-slot slot_lock_ re-check: when slot 0 and slot 1 are both ready in UTRLDBR
    //    (bits cleared, OCS == kSuccess) and slot 0's completion callback calls
    //    DisableCompletion() after slot_lock_ is released around the callback, slot 1 must remain
    //    kScheduled.
    transfer.DisableCompletion();

    zx::result<uint8_t> slot_0 = transfer.ReserveSlot();
    ASSERT_OK(slot_0);
    ASSERT_EQ(slot_0.value(), 0u);
    zx::result<uint8_t> slot_1 = transfer.ReserveSlot();
    ASSERT_OK(slot_1);
    ASSERT_EQ(slot_1.value(), 1u);

    std::atomic<bool> slot_0_completed{false};
    std::atomic<bool> slot_1_completed{false};
    transfer.SendIoScsiCmd(upiu, kTestLun, slot_0.value(), zx::unowned_vmo(), 0, 0,
                           [&](zx_status_t status) {
                             EXPECT_OK(status);
                             slot_0_completed.store(true, std::memory_order_relaxed);
                             transfer.DisableCompletion();
                           });
    transfer.SendIoScsiCmd(upiu, kTestLun, slot_1.value(), zx::unowned_vmo(), 0, 0,
                           [&](zx_status_t status) {
                             EXPECT_OK(status);
                             slot_1_completed.store(true, std::memory_order_relaxed);
                           });
    ASSERT_EQ(transfer.GetInflightIoCount(), 2u);
    EXPECT_EQ(transfer.ProcessCompletionOfIoRequests(), 0u);

    transfer.EnableCompletion();
    EXPECT_EQ(transfer.ProcessCompletionOfIoRequests(), 1u);
    EXPECT_TRUE(slot_0_completed.load(std::memory_order_relaxed));
    EXPECT_FALSE(slot_1_completed.load(std::memory_order_relaxed));
    EXPECT_EQ(transfer.GetInflightIoCount(), 1u);
    {
      std::scoped_lock lock(transfer.GetSlotLock());
      EXPECT_EQ(transfer.GetRequestListLocked().GetSlot(slot_1.value()).state,
                SlotState::kScheduled);
    }

    transfer.EnableCompletion();
    EXPECT_EQ(transfer.ProcessCompletionOfIoRequests(), 1u);
    EXPECT_TRUE(slot_1_completed.load(std::memory_order_relaxed));
    EXPECT_EQ(transfer.GetInflightIoCount(), 0u);

    // 3. Admin and Task Management completion paths also honor DisableCompletion() under
    //    slot_lock_.
    transfer.DisableCompletion();
    zx::result<uint8_t> admin_slot = ReserveAdminSlot();
    ASSERT_OK(admin_slot);
    std::atomic<bool> admin_completed{false};
    NopOutUpiu nop_upiu;
    transfer.SendRequestUsingSlot(nop_upiu, 0, admin_slot.value(), zx::unowned_vmo(), 0, 0,
                                  [&](zx_status_t status) {
                                    EXPECT_OK(status);
                                    admin_completed.store(true, std::memory_order_relaxed);
                                  });
    EXPECT_EQ(transfer.ProcessCompletionOfAdminRequests(), 0u);
    EXPECT_FALSE(admin_completed.load(std::memory_order_relaxed));
    transfer.EnableCompletion();
    EXPECT_EQ(transfer.ProcessCompletionOfAdminRequests(), 1u);
    EXPECT_TRUE(admin_completed.load(std::memory_order_relaxed));

    task_mgmt.DisableCompletion();
    libsync::Completion tm_done;
    zx_status_t tm_status = ZX_ERR_INTERNAL;
    std::thread tm_sender([&] {
      TaskManagementRequestUpiu tm_req(TaskManagementFunction::kQueryTask, kTestLun, 0);
      tm_status = task_mgmt.SendTaskManagementRequest(tm_req).status_value();
      tm_done.Signal();
    });
    auto join_tm_sender = fit::defer([&] {
      if (tm_sender.joinable()) {
        task_mgmt.EnableCompletion();
        task_mgmt.ProcessCompletionOfIoRequests();
        tm_sender.join();
      }
    });
    // Wait until tm_sender has rung UTMRLDBR (DefaultUTMRLDBRHandler runs synchronously while
    // SendTaskManagementRequest holds slot_lock_) before asserting completion gating.
    const zx::time tm_schedule_deadline = zx::deadline_after(zx::sec(5));
    bool tm_scheduled = false;
    while (zx::clock::get_monotonic() < tm_schedule_deadline) {
      {
        std::scoped_lock lock(task_mgmt.GetSlotLock());
        tm_scheduled = task_mgmt.GetRequestListLocked().GetSlot(0).state == SlotState::kScheduled;
      }
      if (tm_scheduled) {
        break;
      }
      zx::nanosleep(zx::deadline_after(zx::msec(1)));
    }
    ASSERT_TRUE(tm_scheduled);
    EXPECT_STATUS(tm_done.Wait(zx::msec(15)), ZX_ERR_TIMED_OUT);
    EXPECT_EQ(task_mgmt.ProcessCompletionOfIoRequests(), 0u);
    task_mgmt.EnableCompletion();
    EXPECT_EQ(task_mgmt.ProcessCompletionOfIoRequests(), 1u);
    ASSERT_OK(tm_done.Wait(zx::sec(5)));
    tm_sender.join();
    EXPECT_OK(tm_status);
  }

  // 4. Concurrent ProcessCompletionOfIoRequests() hammer vs DisableCompletion() + earliest timeout.
  std::atomic<bool> stop_hammer{false};
  std::thread hammer([&] {
    while (!stop_hammer.load(std::memory_order_acquire)) {
      transfer.ProcessCompletionOfIoRequests();
      transfer.ProcessCompletionOfAdminRequests();
      std::this_thread::yield();
    }
  });
  auto join_hammer = fit::defer([&] {
    stop_hammer.store(true, std::memory_order_release);
    transfer.EnableCompletion();
    hammer.join();
  });

  const uint8_t max_io_slots =
      static_cast<uint8_t>(transfer.GetSlotCount() - kAdminCommandSlotCount);
  for (int iter = 0; iter < 32; ++iter) {
    transfer.DisableCompletion();
    ASSERT_EQ(transfer.GetEarliestTimeoutDeadline(), ZX_TIME_INFINITE);
    for (uint8_t i = 0; i < max_io_slots; ++i) {
      zx::result<uint8_t> slot = transfer.ReserveSlot();
      ASSERT_OK(slot);
      ASSERT_EQ(slot.value(), i);
      transfer.SendIoScsiCmd(upiu, kTestLun, slot.value(), zx::unowned_vmo(), 0, 0,
                             [](zx_status_t status) {});
    }
    ASSERT_EQ(transfer.GetInflightIoCount(), max_io_slots);
    zx_time_t slot_0_deadline = ZX_TIME_INFINITE;
    {
      std::scoped_lock lock(transfer.GetSlotLock());
      slot_0_deadline = transfer.GetRequestListLocked().GetSlot(0).deadline;
      for (uint8_t i = 0; i < max_io_slots; ++i) {
        EXPECT_LE(slot_0_deadline, transfer.GetRequestListLocked().GetSlot(i).deadline);
      }
    }
    ASSERT_EQ(transfer.GetEarliestTimeoutDeadline(), slot_0_deadline);
    transfer.EnableCompletion();
    while (transfer.GetInflightIoCount() != 0) {
      transfer.ProcessCompletionOfIoRequests();
    }
  }
}

}  // namespace

}  // namespace ufs
