// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fit/defer.h>
#include <lib/fpromise/single_threaded_executor.h>

#include <future>

#include "src/storage/lib/block_client/cpp/remote_block_device.h"
#include "unit-lib.h"

namespace ufs {
using namespace ufs_mock_device;

class PowerTest : public UfsTest {
 public:
  void SetUp() override { InitMockDevice(); }

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

}  // namespace ufs
