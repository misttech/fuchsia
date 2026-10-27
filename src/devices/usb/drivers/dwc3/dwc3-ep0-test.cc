// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <usb/descriptors.h>

#include "src/devices/usb/drivers/dwc3/dwc3-test-fixture.h"

namespace dwc3 {

TEST_F(UnmanagedTestFixture, Ep0Lifecycle) {
  SetUpAndPowerOnDriver();

  // Start the controller to bring up the device fully (this triggers StartPeripheralMode).
  auto controller_client_end = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_TRUE(controller_client_end.is_ok());
  fidl::SyncClient controller_client(std::move(controller_client_end.value()));
  auto start_result = controller_client->StartController();
  ASSERT_TRUE(start_result.is_ok());

  // Verify that hardware initialization has completed by checking the DCTL RUN_STOP bit.
  bool is_run_stop_set = false;
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    uint32_t val = static_cast<uint32_t>(env.reg_region()[DCTL::Get().addr()].Read());
    is_run_stop_set = DCTL::Get().FromValue(val).RUN_STOP();
  });
  EXPECT_TRUE(is_run_stop_set);

  // Verify Ep0Start() completed and initial Setup TRB was queued.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  dut_.RunInNodeContext(
      [&](fdf_testing::TestNode& node) { EXPECT_EQ(node.children().size(), 1UL); });

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, NoPrematureWritesOnGetDescriptor) {
  SetUpAndPowerOnDriver();

  auto write_detected = std::make_shared<std::atomic<bool>>(false);
  FakeUsbDciInterface fake_dci;
  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    auto setup = MakeGetDescriptorSetup();
    Dwc3TestHelper::WriteEp0Buffer(drv, &setup, 0, sizeof(setup));
  });

  auto data_phase_started = std::make_shared<libsync::Completion>();

  // Use RAII guard to guarantee callback cleanup on scope exit regardless of assertions.
  auto cleanup_dctl = DeferClearDctlCallback();
  auto cleanup_depcmd = DeferClearDepcmdCallbacks(1);
  auto cleanup_dcfg = fit::defer([this]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[DCFG::Get().addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  SetDctlCallback([write_detected]() {
    fdf::info("DCTL write detected");
    write_detected->store(true);
  });

  auto called = std::make_shared<std::atomic<bool>>(false);
  dut_.RunInEnvironmentTypeContext([write_detected, called, data_phase_started](Environment& env) {
    env.reg_region()[DCFG::Get().addr()].SetWriteCallback([write_detected](uint64_t val_raw) {
      [[maybe_unused]] uint32_t val = static_cast<uint32_t>(val_raw);
      fdf::info("DCFG write: 0x{:x}", val);
      write_detected->store(true);
    });
    env.reg_region()[DEPCMD::Get(1).addr()].SetWriteCallback(
        [data_phase_started, called](uint64_t val_raw) {
          [[maybe_unused]] uint32_t val = static_cast<uint32_t>(val_raw);
          if (!called->exchange(true)) {
            data_phase_started->Signal();
          }
        });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);  // 0 is kEp0Out
  });

  ASSERT_EQ(data_phase_started->Wait(zx::sec(5)), ZX_OK);

  dut_.RunInDriverContext([&](Dwc3& drv) { EXPECT_FALSE(write_detected->load()); });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, InvalidSetupRequest) {
  SetUpAndPowerOnDriver();

  FakeUsbDciInterface fake_dci;
  fake_dci.SetControlStatus(ZX_ERR_NOT_SUPPORTED);
  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);
    fake_dci.SetControlStatus(ZX_ERR_NOT_SUPPORTED);

    auto setup = MakeSetupPacket(0xC0, 0x99, 0, 0, 8);
    Dwc3TestHelper::WriteEp0Buffer(drv, &setup, 0, sizeof(setup));

    // Simulate Setup phase completion
    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);
  });

  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        bool done = false;
        dut_.RunInDriverContext([&](Dwc3& drv) {
          done = (Dwc3TestHelper::GetEp0State(drv) == Dwc3TestHelper::State::Setup);
        });
        return done;
      },
      zx::sec(10)));

  dut_.RunInDriverContext([&](Dwc3& drv) {
    // Verification: It should have failed and returned to Setup state
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, OppositeDirectionXferNotReady) {
  SetUpAndPowerOnDriver();

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetControllerStarted(drv, true);

    // Simulate DataOut state (OUT transfer in progress)
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::DataOut);
    Dwc3TestHelper::SetEpTransferState(
        drv, 0, Dwc3TestHelper::TransferState::kActiveSingle);  // EP0 OUT in progress
    Dwc3TestHelper::SetEpRsrcId(drv, 0, 1);                     // Set valid resource ID

    // Simulate XferNotReady for EP0 IN (opposite direction)
    Dwc3TestHelper::HandleEp0TransferNotReadyEvent(drv, 1, DEPEVT_XFER_NOT_READY_STAGE_DATA);

    // Verification: It should have called fail() logic:
    // - Stalled EP0 OUT
    // - Queued Setup
    // And it should have returned to Setup state.
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, ResetDuringControlWrite) {
  SetUpAndPowerOnDriver();

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetControllerStarted(drv, true);

    // Simulate DataOut state (OUT transfer in progress)
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::DataOut);
    Dwc3TestHelper::SetEpTransferState(
        drv, 0, Dwc3TestHelper::TransferState::kActiveSingle);  // EP0 OUT in progress
    // Do NOT set rsrc_id to simulate uninitialized resource ID during transfer!
  });

  // Stop driver while transfer is in progress.
  // This should trigger Ep0Reset, which will call CmdEpEndTransfer.
  // If rsrc_id is invalid, it will assert/crash!
  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, Ep0ResetForceAbortsActiveTransfers) {
  // Shared sequential log to verify strict protocol stages chronologically!
  auto sequence_log = std::make_shared<std::vector<std::string>>();

  SetUpAndPowerOnDriver();

  // Use RAII guard to guarantee callback cleanup on scope exit regardless of assertions.
  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[DEPCMD::Get(0).addr()].SetWriteCallback([](uint64_t) {});
      env.reg_region()[DEPCMD::Get(1).addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  dut_.RunInEnvironmentTypeContext([&, sequence_log](Environment& env) {
    // Mock DEPCMD for EP0 OUT to capture the force End Transfer write!
    env.reg_region()[DEPCMD::Get(0).addr()].SetWriteCallback([sequence_log](uint64_t val_raw) {
      [[maybe_unused]] uint32_t val = static_cast<uint32_t>(val_raw);
      if (DEPCMD::Get(0).FromValue(val).CMDTYP() == DEPCMD::DEPENDXFER) {
        sequence_log->push_back("END_XFER_EP0_OUT");
      }
    });

    // Mock DEPCMD for EP0 IN to capture the force End Transfer write!
    env.reg_region()[DEPCMD::Get(1).addr()].SetWriteCallback([sequence_log](uint64_t val_raw) {
      [[maybe_unused]] uint32_t val = static_cast<uint32_t>(val_raw);
      if (DEPCMD::Get(1).FromValue(val).CMDTYP() == DEPCMD::DEPENDXFER) {
        sequence_log->push_back("END_XFER_EP0_IN");
      }
    });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetControllerStarted(drv, true);

    // Place EP0 in Setup state with active transfers on both directions
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetEpTransferState(drv, 0, Dwc3TestHelper::TransferState::kActiveSingle);
    Dwc3TestHelper::SetEpRsrcId(drv, 0, 1);
    Dwc3TestHelper::SetEpTransferState(drv, 1, Dwc3TestHelper::TransferState::kActiveSingle);
    Dwc3TestHelper::SetEpRsrcId(drv, 1, 2);

    // Invoke Ep0Reset! This must sequentially abort EP0 OUT then EP0 IN!
    Dwc3TestHelper::Ep0Reset(drv);
  });

  // Verify the strict, chronological protocol sequence!
  ASSERT_EQ(sequence_log->size(), 2u);
  EXPECT_EQ((*sequence_log)[0], "END_XFER_EP0_OUT")
      << "Protocol Violation: Setup phase force-abort must be written to EP0 OUT first!";
  EXPECT_EQ((*sequence_log)[1], "END_XFER_EP0_IN")
      << "Protocol Violation: Setup phase force-abort must be written to EP0 IN second!";

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, EndTransferOnValidResource) {
  auto depcmd_written = std::make_shared<std::atomic<bool>>(false);
  auto depcmd_val = std::make_shared<std::atomic<uint32_t>>(0);

  SetUpAndPowerOnDriver();

  auto cleanup_callbacks = DeferClearDepcmdCallbacks(0);

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    // Mock DEPCMD for EP0 OUT to detect the "End Transfer" command!
    env.reg_region()[DEPCMD::Get(0).addr()].SetWriteCallback(
        [depcmd_written, depcmd_val](uint64_t val_raw) {
          [[maybe_unused]] uint32_t val = static_cast<uint32_t>(val_raw);
          if (DEPCMD::Get(0).FromValue(val).CMDTYP() == DEPCMD::DEPENDXFER) {
            depcmd_written->store(true);
            depcmd_val->store(val);
          }
        });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetControllerStarted(drv, true);

    // Simulate DataOut state and SET A VALID RESOURCE ID!
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::DataOut);
    Dwc3TestHelper::SetEpTransferState(drv, 0, Dwc3TestHelper::TransferState::kActiveSingle);
    Dwc3TestHelper::SetEpRsrcId(drv, 0, 2);  // Valid resource ID
  });

  // Manually trigger the "End Transfer" logic by calling Ep0Reset directly!
  dut_.RunInDriverContext([&](Dwc3& drv) { Dwc3TestHelper::Ep0Reset(drv); });

  // Now that we've captured the write, we can let the fixture finish the official stop sequence.
  TearDownAndPowerOffDriver();

  // Verification: Driver should have written the End Transfer command to DEPCMD!
  EXPECT_TRUE(depcmd_written->load());
}

TEST_F(UnmanagedTestFixture, EndTransferOnBothEndpoints) {
  auto ep0_out_end_transfer = std::make_shared<std::atomic<bool>>(false);
  auto ep0_in_end_transfer = std::make_shared<std::atomic<bool>>(false);

  SetUpAndPowerOnDriver();

  // Use RAII guard to guarantee callback cleanup on scope exit regardless of assertions.
  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[DEPCMD::Get(0).addr()].SetWriteCallback([](uint64_t) {});
      env.reg_region()[DEPCMD::Get(1).addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    // Mock DEPCMD for EP0 OUT (0) to detect End Transfer and assert COMMANDPARAM
    env.reg_region()[DEPCMD::Get(0).addr()].SetWriteCallback(
        [ep0_out_end_transfer](uint64_t val_raw) {
          [[maybe_unused]] uint32_t val = static_cast<uint32_t>(val_raw);
          if (DEPCMD::Get(0).FromValue(val).CMDTYP() == DEPCMD::DEPENDXFER) {
            ep0_out_end_transfer->store(true);
            const uint32_t param = DEPCMD::Get(0).FromValue(val).COMMANDPARAM();
            if (param != 0) {
              EXPECT_EQ(param, 2u);
            }
          }
        });

    // Mock DEPCMD for EP0 IN (1) to detect End Transfer and assert COMMANDPARAM
    env.reg_region()[DEPCMD::Get(1).addr()].SetWriteCallback(
        [ep0_in_end_transfer](uint64_t val_raw) {
          [[maybe_unused]] uint32_t val = static_cast<uint32_t>(val_raw);
          if (DEPCMD::Get(1).FromValue(val).CMDTYP() == DEPCMD::DEPENDXFER) {
            ep0_in_end_transfer->store(true);
            const uint32_t param = DEPCMD::Get(1).FromValue(val).COMMANDPARAM();
            if (param != 0) {
              EXPECT_EQ(param, 2u);
            }
          }
        });
  });

  dut_.runtime().RunUntilIdle();

  // Setup the active transfer states for both directions
  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetControllerStarted(drv, true);

    // Simulate an active state
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::DataOut);

    // Set transfer_state to active and assign valid resource IDs for both EP0 IN and EP0 OUT
    Dwc3TestHelper::SetEpTransferState(drv, 0, Dwc3TestHelper::TransferState::kActiveSingle);
    Dwc3TestHelper::SetEpRsrcId(drv, 0, 2);

    Dwc3TestHelper::SetEpTransferState(drv, 1, Dwc3TestHelper::TransferState::kActiveSingle);
    Dwc3TestHelper::SetEpRsrcId(drv, 1, 2);
  });

  // Manually trigger the Reset logic which should end transfers
  dut_.RunInDriverContext([&](Dwc3& drv) { Dwc3TestHelper::Ep0Reset(drv); });

  // Complete fixture teardown safely
  TearDownAndPowerOffDriver();

  // Verification
  EXPECT_TRUE(ep0_out_end_transfer->load());
  EXPECT_TRUE(ep0_in_end_transfer->load());
}

TEST_F(UnmanagedTestFixture, FidlControlCallFailure) {
  SetUpAndPowerOnDriver();

  BindDciInterfaceWithoutServer();

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto setup = MakeSetupPacket(0xC0, 0x99, 0, 0, 8);
    Dwc3TestHelper::WriteEp0Buffer(drv, &setup, 0, sizeof(setup));

    // Simulate Setup phase completion
    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);
  });

  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        bool done = false;
        dut_.RunInDriverContext([&](Dwc3& drv) {
          done = (Dwc3TestHelper::GetEp0State(drv) == Dwc3TestHelper::State::Setup);
        });
        return done;
      },
      zx::sec(10)));

  dut_.RunInDriverContext([&](Dwc3& drv) {
    // Verification: It should have failed and returned to Setup state
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, UsbBusResetDuringTransfer) {
  SetUpAndPowerOnDriver();

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetControllerStarted(drv, true);

    // Simulate DataOut state (OUT transfer in progress)
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::DataOut);
    Dwc3TestHelper::SetEpTransferState(
        drv, 0, Dwc3TestHelper::TransferState::kActiveSingle);  // EP0 OUT in progress
    Dwc3TestHelper::SetEpRsrcId(drv, 0, 1);                     // Set valid resource ID

    // Trigger Bus Reset
    Dwc3TestHelper::HandleResetEvent(drv);

    // Verification: State should be reset to Setup
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, DisableEndpointDuringTransfer) {
  SetUpAndPowerOnDriver();

  auto cleanup_callbacks = DeferClearDepcmdCallbacks(2);

  dut_.RunInEnvironmentTypeContext([](Environment& env) {
    // Mock DEPCMD for EP2 to return 0 (Command Complete)
    env.reg_region()[DEPCMD::Get(2).addr()].SetReadCallback([]() -> uint32_t { return 0; });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetControllerStarted(drv, true);

    // Access user endpoint 2
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);

    // Simulate transfer in progress
    Dwc3TestHelper::SetEpTransferState(drv, 2, Dwc3TestHelper::TransferState::kActiveSingle);
    // Do NOT set rsrc_id (leave invalid)

    // Simulate function driver calling DisableEndpoint
    // We call EpSetConfig(..., false) directly as it is the core of DisableEndpoint
    // and doesn't require complex FIDL mocking.
    Dwc3TestHelper::EpSetConfig(drv, uep->ep, false);
  });

  // StopDriver calls Stop() which runs QuiesceCore() -> StopActiveTransfers().
  // RemoveRequests guards against kInvalidResourceId and must not crash.
  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, ControllerStoppedDuringTransfer) {
  SetUpAndPowerOnDriver();

  auto gevntcount_read = std::make_shared<std::atomic<bool>>(false);
  auto read_count = std::make_shared<std::atomic<int>>(0);

  // Use RAII guard to guarantee callback cleanup on scope exit regardless of assertions.
  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[GEVNTCOUNT::Get(0).addr()].SetReadCallback([]() -> uint32_t { return 0; });
    });
  });

  dut_.RunInEnvironmentTypeContext([gevntcount_read, read_count](Environment& env) {
    env.reg_region()[GEVNTCOUNT::Get(0).addr()].SetReadCallback(
        [gevntcount_read, read_count]() -> uint32_t {
          gevntcount_read->store(true);
          if (read_count->fetch_add(1) == 0) {
            return 4;
          }
          return 0;
        });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    // Set controller_started_ = false
    Dwc3TestHelper::SetControllerStarted(drv, false);

    // Call HandleIrq directly via helper
    // Pass placeholder arguments
    Dwc3TestHelper::HandleIrq(drv, nullptr, nullptr, ZX_OK, nullptr);
  });

  // Verification: GEVNTCOUNT should NOT have been read!
  EXPECT_FALSE(gevntcount_read->load());

  TearDownAndPowerOffDriver();
}

// This test verifies that a control transfer with wLength = 0 (Zero Length Packet)
// skips the Data stage and proceeds directly to the Status stage.
TEST_F(UnmanagedTestFixture, ZeroLengthPacket) {
  FakeUsbDciInterface fake_dci;

  SetUpAndPowerOnDriver();

  auto callback_executed = std::make_shared<std::atomic<bool>>(false);
  fake_dci.SetControlCallback(
      [callback_executed](fuchsia_hardware_usb_descriptor::wire::UsbSetup setup,
                          cpp20::span<const uint8_t> data) { callback_executed->store(true); });

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);

    // Setup packet with wLength = 0
    auto setup =
        MakeSetupPacket(fdescriptor::EndpointDirection::kOut | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x01, 0, 0, 0);
    Dwc3TestHelper::WriteEp0Buffer(drv, &setup, 0, sizeof(setup));

    // Push placeholder TRB to avoid AdvanceRead error
    dwc3_trb_t trb{};
    Dwc3TestHelper::PushTrbToSharedFifo(drv, trb);

    // Call HandleEp0TransferCompleteEvent
    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);
  });

  // Wait for state transition to WaitHost deterministically
  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        bool done = false;
        dut_.RunInDriverContext([&](Dwc3& drv) {
          auto state = Dwc3TestHelper::GetEp0State(drv);
          done = (state == Dwc3TestHelper::State::WaitHost);
          if (state == Dwc3TestHelper::State::Setup) {
            done = true;
          }
        });
        return done;
      },
      zx::sec(10)));

  dut_.RunInDriverContext([&](Dwc3& drv) {
    // Verification: State should become WaitHost (after TwoStage)
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitHost);
  });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

// This test verifies that the driver stalls a Control Write that exceeds the maximum FIDL control
// request length.
TEST_F(UnmanagedTestFixture, MaxBufferSizeTransfer) {
  FakeUsbDciInterface fake_dci;

  SetUpAndPowerOnDriver();

  auto received_length = std::make_shared<std::atomic<size_t>>(0);
  fake_dci.SetControlCallback(
      [received_length](fuchsia_hardware_usb_descriptor::wire::UsbSetup setup,
                        cpp20::span<const uint8_t> data) { received_length->store(data.size()); });

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);

    // Use high-level construct to simulate Setup received!
    Dwc3TestHelper::SimulateSetupReceived(
        drv,
        MakeSetupPacket(fdescriptor::EndpointDirection::kOut | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x01, 0, 0, 65535));
    // A 3-stage Control Write first arms the 64 KiB Data OUT TRB in State::DataOut;
    // DoControlCall() checks the received payload length against kMaxControlRequestLen (65400)
    // and stalls EP0 only after the Data OUT phase completes.
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::DataOut);
    Dwc3TestHelper::SimulateDataOutPhase(drv, 65535);
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    // With the FIDL limit fix, oversized requests are stalled and state returns to Setup.
    EXPECT_TRUE(Dwc3TestHelper::IsEp0OutStalled(drv));
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

// This test verifies that the driver handles a Control Read that requires a ZLP.
TEST_F(UnmanagedTestFixture, ZlpInTransferRequired) {
  FakeUsbDciInterface fake_dci;

  SetUpAndPowerOnDriver();

  // Simulate device returning 512 bytes (MPS = 512)
  std::vector<uint8_t> read_data(512, 0xAA);
  fake_dci.SetReadData(read_data);

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);

    // Setup packet with wLength = 1024
    auto setup =
        MakeSetupPacket(fdescriptor::EndpointDirection::kIn | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x01, 0, 0, 1024);

    // Write setup packet to buffer
    Dwc3TestHelper::WriteEp0Buffer(drv, &setup, 0, sizeof(setup));

    // Call HandleEp0TransferCompleteEvent to trigger HandleSetup
    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);  // 0 is kEp0Out
  });

  // Wait for callback to be executed deterministically
  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        bool ready = false;
        dut_.RunInDriverContext([&](Dwc3& drv) { ready = !Dwc3TestHelper::IsFifoEmpty(drv); });
        return ready;
      },
      zx::sec(10)));

  dut_.RunInDriverContext([&](Dwc3& drv) {
    // Verification: State should be DataIn (correct behavior)
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::DataIn);

    Dwc3TestHelper::SimulateDataInPhase(drv);

    // Verification: State should be WaitZlpIn
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitZlpIn);

    // Verify TransferNotReady event in WaitZlpIn is handled without altering state.
    Dwc3TestHelper::HandleEp0TransferNotReadyEvent(drv, 1, 2);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitZlpIn);

    // Simulate ZLP completion on EP0 IN!
    Dwc3TestHelper::SimulateDataInPhase(drv);

    // Verification: State should be WaitNrdyOut
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitNrdyOut);
  });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

// This test verifies that if the device data length equals the Host wLength (and is a multiple of
// MPS), no ZLP is required because the transfer is fully completed.
TEST_F(UnmanagedTestFixture, ZlpInNotRequiredExactMatch) {
  FakeUsbDciInterface fake_dci;

  SetUpAndPowerOnDriver();

  // Simulate device returning 512 bytes (MPS = 512)
  std::vector<uint8_t> read_data(512, 0xAA);
  fake_dci.SetReadData(read_data);

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);

    // Setup packet with wLength = 512 (Exact match)
    auto setup =
        MakeSetupPacket(fdescriptor::EndpointDirection::kIn | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x01, 0, 0, 512);
    Dwc3TestHelper::WriteEp0Buffer(drv, &setup, 0, sizeof(setup));

    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);  // 0 is kEp0Out
  });

  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        bool ready = false;
        dut_.RunInDriverContext([&](Dwc3& drv) { ready = !Dwc3TestHelper::IsFifoEmpty(drv); });
        return ready;
      },
      zx::sec(10)));

  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::DataIn);
    Dwc3TestHelper::SimulateDataInPhase(drv);

    // Verification: State should be WaitNrdyOut directly (No ZLP required since transferred ==
    // wLength)
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitNrdyOut);
  });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

// This test verifies that if the device data length is less than wLength, but not a multiple of
// MPS, no ZLP is required because the short packet acts as the terminator.
TEST_F(UnmanagedTestFixture, ZlpInNotRequiredShortPacketTerminated) {
  FakeUsbDciInterface fake_dci;

  SetUpAndPowerOnDriver();

  // Simulate device returning 200 bytes (not a multiple of MPS = 512)
  std::vector<uint8_t> read_data(200, 0xAA);
  fake_dci.SetReadData(read_data);

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);

    // Setup packet with wLength = 1024
    auto setup =
        MakeSetupPacket(fdescriptor::EndpointDirection::kIn | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x01, 0, 0, 1024);
    Dwc3TestHelper::WriteEp0Buffer(drv, &setup, 0, sizeof(setup));

    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);  // 0 is kEp0Out
  });

  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        bool ready = false;
        dut_.RunInDriverContext([&](Dwc3& drv) { ready = !Dwc3TestHelper::IsFifoEmpty(drv); });
        return ready;
      },
      zx::sec(10)));

  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::DataIn);
    Dwc3TestHelper::SimulateDataInPhase(drv);

    // Verification: State should be WaitNrdyOut directly (No ZLP required since short packet is
    // terminator)
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitNrdyOut);
  });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

// Verifies that receiving a Transfer Complete on the wrong endpoint while waiting for ZLP IN
// aborts the transfer and resets to Setup.
TEST_F(UnmanagedTestFixture, WaitZlpInTransferCompleteMismatch) {
  FakeUsbDciInterface fake_dci;

  SetUpAndPowerOnDriver();

  std::vector<uint8_t> read_data(512, 0xAA);
  fake_dci.SetReadData(read_data);

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::SetEpRsrcId(drv, 0, 1);
    Dwc3TestHelper::SetEpRsrcId(drv, 1, 1);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);

    auto setup =
        MakeSetupPacket(fdescriptor::EndpointDirection::kIn | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x01, 0, 0, 1024);
    Dwc3TestHelper::WriteEp0Buffer(drv, &setup, 0, sizeof(setup));

    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);  // Setup
  });

  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        bool ready = false;
        dut_.RunInDriverContext([&](Dwc3& drv) { ready = !Dwc3TestHelper::IsFifoEmpty(drv); });
        return ready;
      },
      zx::sec(10)));

  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::DataIn);
    Dwc3TestHelper::SimulateDataInPhase(drv);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitZlpIn);

    // Mismatch: Send Transfer Complete on EP0 OUT instead of IN
    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);  // 0 is EP0 OUT

    // Expectation: Stall and reset to Setup
    EXPECT_TRUE(Dwc3TestHelper::IsEp0InStalled(drv));
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

// Verifies that receiving an XferNotReady on the wrong endpoint while waiting for ZLP IN
// aborts the transfer and resets to Setup.
TEST_F(UnmanagedTestFixture, WaitZlpInTransferNotReadyMismatch) {
  FakeUsbDciInterface fake_dci;

  SetUpAndPowerOnDriver();

  std::vector<uint8_t> read_data(512, 0xAA);
  fake_dci.SetReadData(read_data);

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::SetEpRsrcId(drv, 0, 1);
    Dwc3TestHelper::SetEpRsrcId(drv, 1, 1);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);

    auto setup =
        MakeSetupPacket(fdescriptor::EndpointDirection::kIn | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x01, 0, 0, 1024);
    Dwc3TestHelper::WriteEp0Buffer(drv, &setup, 0, sizeof(setup));

    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);  // Setup
  });

  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        bool ready = false;
        dut_.RunInDriverContext([&](Dwc3& drv) { ready = !Dwc3TestHelper::IsFifoEmpty(drv); });
        return ready;
      },
      zx::sec(10)));

  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::DataIn);
    Dwc3TestHelper::SimulateDataInPhase(drv);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitZlpIn);

    // Mismatch: Send Transfer Not Ready Data on EP0 OUT
    Dwc3TestHelper::HandleEp0TransferNotReadyEvent(
        drv, 0, DEPEVT_XFER_NOT_READY_STAGE_DATA);  // 0 is EP0 OUT

    // Expectation: Stall and reset to Setup
    EXPECT_TRUE(Dwc3TestHelper::IsEp0InStalled(drv));
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, UsbResetTransitionsToDefaultState) {
  SetUpAndPowerOnDriver();

  auto client_end = dut_.Connect<fuchsia_hardware_usb_policy::Service::Controller>();
  ASSERT_TRUE(client_end.is_ok());
  fidl::SyncClient client{std::move(*client_end)};

  // 1. Consume initial state.
  auto watch_result1 = client->WatchDeviceState();
  ASSERT_TRUE(watch_result1.is_ok());

  // 2. Set device address to 5!
  dut_.RunInDriverContext([](Dwc3& drv) { Dwc3TestHelper::SetDeviceAddress(drv, 5); });

  // 3. Verify state is kAddress via WatchDeviceState!
  auto watch_result2 = client->WatchDeviceState();
  ASSERT_TRUE(watch_result2.is_ok());
  ASSERT_TRUE(watch_result2->state().has_value());
  EXPECT_EQ(watch_result2->state().value(), fuchsia_hardware_usb_policy::DeviceState::kAddress);
  ASSERT_TRUE(watch_result2->address().has_value());
  EXPECT_EQ(watch_result2->address().value(), 5u);

  // 4. Simulate USB Reset event!
  dut_.RunInDriverContext([](Dwc3& drv) { Dwc3TestHelper::HandleEvent(drv, 257); });

  // 5. Verify state is kDefault via WatchDeviceState!
  auto watch_result3 = client->WatchDeviceState();
  ASSERT_TRUE(watch_result3.is_ok());
  ASSERT_TRUE(watch_result3->state().has_value());
  EXPECT_EQ(watch_result3->state().value(), fuchsia_hardware_usb_policy::DeviceState::kDefault);
  ASSERT_TRUE(watch_result3->address().has_value());
  EXPECT_EQ(watch_result3->address().value(), 0u);

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, SetInterfaceAlreadySet) {
  SetUpAndPowerOnDriver();

  auto client_end = dut_.Connect<fuchsia_hardware_usb_dci::UsbDciService::Device>();
  ASSERT_TRUE(client_end.is_ok());
  fidl::SyncClient client{std::move(*client_end)};

  // Create a valid interface client!
  auto [interface_client, interface_server] =
      fidl::Endpoints<fuchsia_hardware_usb_dci::UsbDciInterface>::Create();

  // Call SetInterface first time!
  auto result = client->SetInterface({{.interface = std::move(interface_client)}});
  ASSERT_TRUE(result.is_ok());

  // Create another valid interface client!
  auto [interface_client2, interface_server2] =
      fidl::Endpoints<fuchsia_hardware_usb_dci::UsbDciInterface>::Create();

  // Call SetInterface second time!
  auto result2 = client->SetInterface({{.interface = std::move(interface_client2)}});

  ASSERT_TRUE(result2.is_error());
  ASSERT_TRUE(result2.error_value().is_domain_error());
  EXPECT_EQ(result2.error_value().domain_error(), ZX_ERR_BAD_STATE);

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, TransferCompleteOnEmptyFifoIgnored) {
  SetUpAndPowerOnDriver();
  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::Ep0Reset(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);

    // Call via helper. Safe early return should occur without AdvanceRead failure log.
    Dwc3TestHelper::SimulateGhostTransferCompleteEvent(drv, 0);

    // State should remain stable.
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  TearDownAndPowerOffDriver();
}

// Verifies hardware race condition recovery during Transfer Complete processing.
// In practice, if a Transfer Complete interrupt fires while the TRB's Hardware Own (HWO) bit
// is still set (e.g. due to DMA cache latency or a spurious hardware event), the driver
// retries reading the TRB. If HWO remains set after retries, the driver safely aborts the
// transfer, stalls the endpoint, and resets EP0 to the Setup state.
TEST_F(UnmanagedTestFixture, SpuriousTransferCompleteWithHwoSetTriggersStallAbort) {
  SetUpAndPowerOnDriver();
  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::DataOut);

    // Push a "Dirty" TRB that specifies HWO is still set!
    dwc3_trb_t trb{};
    trb.control = TRB_HWO;
    Dwc3TestHelper::PushTrbToSharedFifo(drv, trb);

    // Call via helper wrapper (bypasses standard wrapper's auto-clear logic).
    // This forces 5 retries and then absolute abort.
    Dwc3TestHelper::SimulateGhostTransferCompleteEvent(drv, 0);

    // Assert abort logic fired!
    EXPECT_TRUE(Dwc3TestHelper::IsEp0OutStalled(drv));
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, ControlReadParseGetDescriptor) {
  SetUpAndPowerOnDriver();

  FakeUsbDciInterface fake_dci;
  auto control_called = std::make_shared<libsync::Completion>();
  fake_dci.SetControlCallback(
      [control_called](fuchsia_hardware_usb_descriptor::wire::UsbSetup setup,
                       cpp20::span<const uint8_t> data) { control_called->Signal(); });

  std::vector<uint8_t> read_data(18, 0xAA);
  fake_dci.SetReadData(std::move(read_data));

  auto data_phase_started = std::make_shared<libsync::Completion>();
  auto called = std::make_shared<std::atomic<bool>>(false);

  auto cleanup_callbacks = DeferClearDepcmdCallbacks(1);

  dut_.RunInEnvironmentTypeContext([data_phase_started, called](Environment& env) {
    env.reg_region()[DEPCMD::Get(1).addr()].SetWriteCallback(
        [data_phase_started, called](uint64_t val_raw) {
          [[maybe_unused]] uint32_t val = static_cast<uint32_t>(val_raw);
          if (!called->exchange(true)) {
            fdf::info("DEPCMD write detected!");
            data_phase_started->Signal();
          }
        });
  });

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    auto setup = MakeGetDescriptorSetup(18);
    Dwc3TestHelper::WriteEp0Buffer(drv, &setup, 0, sizeof(setup));
    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);  // 0 is kEp0Out
  });

  ASSERT_EQ(data_phase_started->Wait(zx::sec(5)), ZX_OK);
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto state = Dwc3TestHelper::GetEp0State(drv);
    EXPECT_TRUE(state == Dwc3TestHelper::State::DataIn || state == Dwc3TestHelper::State::Setup);
  });
  EXPECT_TRUE(fake_dci.control_called());

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, ControlReadCompleteGetDescriptor) {
  SetUpAndPowerOnDriver();

  FakeUsbDciInterface fake_dci;
  auto control_called = std::make_shared<libsync::Completion>();
  fake_dci.SetControlCallback(
      [control_called](fuchsia_hardware_usb_descriptor::wire::UsbSetup setup,
                       cpp20::span<const uint8_t> data) { control_called->Signal(); });

  std::vector<uint8_t> read_data(18, 0xAA);
  fake_dci.SetReadData(std::move(read_data));

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::Ep0Reset(drv);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    auto setup = MakeGetDescriptorSetup(18);
    Dwc3TestHelper::WriteEp0Buffer(drv, &setup, 0, sizeof(setup));
    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);  // 0 is kEp0Out
  });

  ASSERT_EQ(control_called->Wait(zx::sec(5)), ZX_OK);

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, ControlReadShortPacketDataIn) {
  SetUpAndPowerOnDriver();

  FakeUsbDciInterface fake_dci;
  auto control_called = std::make_shared<libsync::Completion>();
  fake_dci.SetControlCallback(
      [control_called](fuchsia_hardware_usb_descriptor::wire::UsbSetup setup,
                       cpp20::span<const uint8_t> data) { control_called->Signal(); });

  std::vector<uint8_t> read_data(32, 0xAA);
  fake_dci.SetReadData(std::move(read_data));

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    auto setup = MakeGetDescriptorSetup(64);
    setup.bm_request_type = fdescriptor::EndpointDirection::kIn |
                            fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice;
    setup.b_request = 0x01;
    Dwc3TestHelper::SimulateSetupReceived(drv, setup);
  });

  // Wait for the driver to actually queue the data phase transfer to the hardware!
  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        bool queued = false;
        dut_.RunInDriverContext(
            [&](Dwc3& drv) { queued = !Dwc3TestHelper::IsSharedFifoEmpty(drv); });
        return queued;
      },
      zx::sec(10)));

  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::DataIn);
    Dwc3TestHelper::SimulateDataInPhase(drv);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitNrdyOut);
    Dwc3TestHelper::SimulateStatusPhase(drv, false);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, DISABLED_ControlReadOversizedStallIn) {
  SetUpAndPowerOnDriver();

  FakeUsbDciInterface fake_dci;
  auto control_called = std::make_shared<libsync::Completion>();
  fake_dci.SetControlCallback(
      [control_called](fuchsia_hardware_usb_descriptor::wire::UsbSetup setup,
                       cpp20::span<const uint8_t> data) { control_called->Signal(); });

  std::vector<uint8_t> read_data(65, 0xAA);
  fake_dci.SetReadData(std::move(read_data));

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    auto setup = MakeGetDescriptorSetup(64);
    setup.bm_request_type = fdescriptor::EndpointDirection::kIn |
                            fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice;
    setup.b_request = 0x01;
    Dwc3TestHelper::SimulateSetupReceived(drv, setup);
  });

  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        bool stalled = false;
        dut_.RunInDriverContext([&](Dwc3& drv) {
          stalled = (Dwc3TestHelper::GetEp0State(drv) == Dwc3TestHelper::State::Setup);
        });
        return stalled;
      },
      zx::sec(5)));

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, ControlWriteComplete) {
  SetUpAndPowerOnDriver();

  FakeUsbDciInterface fake_dci;
  auto control_called = std::make_shared<libsync::Completion>();
  auto received_data = std::make_shared<std::vector<uint8_t>>();
  auto callback_received_len = std::make_shared<std::atomic<size_t>>(0);

  fake_dci.SetControlCallback(
      [control_called, received_data, callback_received_len](
          fuchsia_hardware_usb_descriptor::wire::UsbSetup setup, cpp20::span<const uint8_t> data) {
        received_data->assign(data.begin(), data.end());
        callback_received_len->store(data.size());
        control_called->Signal();
      });

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    auto setup =
        MakeSetupPacket(fdescriptor::EndpointDirection::kOut | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x99, 0, 0, 8);
    Dwc3TestHelper::WriteEp0Buffer(drv, &setup, 0, sizeof(setup));
    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);
    Dwc3TestHelper::SimulateDataOutPhase(drv, 8);
  });

  ASSERT_EQ(control_called->Wait(zx::sec(5)), ZX_OK);
  EXPECT_EQ(callback_received_len->load(), 8u);

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, ControlWriteDataOutOverflow) {
  SetUpAndPowerOnDriver();

  FakeUsbDciInterface fake_dci;
  auto control_called = std::make_shared<libsync::Completion>();
  auto received_data = std::make_shared<std::vector<uint8_t>>();
  auto callback_received_len = std::make_shared<std::atomic<size_t>>(0);

  fake_dci.SetControlCallback(
      [control_called, received_data, callback_received_len](
          fuchsia_hardware_usb_descriptor::wire::UsbSetup setup, cpp20::span<const uint8_t> data) {
        received_data->assign(data.begin(), data.end());
        callback_received_len->store(data.size());
        control_called->Signal();
      });

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    auto setup =
        MakeSetupPacket(fdescriptor::EndpointDirection::kOut | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x01, 0, 0, 8);
    Dwc3TestHelper::WriteEp0Buffer(drv, &setup, 0, sizeof(setup));
    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::DataOut);
    EXPECT_EQ(Dwc3TestHelper::GetEp0CurTransferLen(drv), 512u);

    Dwc3TestHelper::GetCurrentReadTrb(drv)->status =
        TRB_BUFSIZ(static_cast<uint32_t>(Dwc3TestHelper::GetEp0CurTransferLen(drv) - 16));

    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);

    EXPECT_TRUE(Dwc3TestHelper::IsEp0OutStalled(drv));
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  dut_.runtime().RunUntilIdle();
  EXPECT_FALSE(control_called->signaled());
  EXPECT_EQ(callback_received_len->load(), 0u);

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, ControlWriteShortPacketDataOut) {
  SetUpAndPowerOnDriver();

  FakeUsbDciInterface fake_dci;
  auto control_called = std::make_shared<libsync::Completion>();
  auto received_data = std::make_shared<std::vector<uint8_t>>();
  auto callback_received_len = std::make_shared<std::atomic<size_t>>(0);

  fake_dci.SetControlCallback(
      [control_called, received_data, callback_received_len](
          fuchsia_hardware_usb_descriptor::wire::UsbSetup setup, cpp20::span<const uint8_t> data) {
        received_data->assign(data.begin(), data.end());
        callback_received_len->store(data.size());
        control_called->Signal();
      });

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    auto setup =
        MakeSetupPacket(fdescriptor::EndpointDirection::kOut | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x01, 0, 0, 16);
    Dwc3TestHelper::WriteEp0Buffer(drv, &setup, 0, sizeof(setup));
    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::DataOut);
    EXPECT_EQ(Dwc3TestHelper::GetEp0CurTransferLen(drv), 512u);

    Dwc3TestHelper::GetCurrentReadTrb(drv)->status =
        TRB_BUFSIZ(static_cast<uint32_t>(Dwc3TestHelper::GetEp0CurTransferLen(drv) - 8));

    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);
  });

  // Wait for callback to be executed deterministically
  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() { return callback_received_len->load() != 0; }, zx::sec(10)));
  EXPECT_EQ(callback_received_len->load(), 8u);

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, MaxBufferSizeTransferIn) {
  FakeUsbDciInterface fake_dci;
  SetUpAndPowerOnDriver();

  std::optional<fidl::ServerBindingRef<fuchsia_hardware_usb_dci::UsbDciInterface>> binding;

  // Simulate device returning 65535 bytes
  std::vector<uint8_t> read_data(65535, 0xCC);
  fake_dci.SetReadData(read_data);

  binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);

    // Setup packet with wLength = 65535
    fuchsia_hardware_usb_descriptor::wire::UsbSetup setup;
    setup.bm_request_type = fdescriptor::EndpointDirection::kIn |
                            fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice;
    setup.b_request = 0x01;
    setup.w_length = 65535;

    // Use high-level primitive to simulate Setup received!
    Dwc3TestHelper::SimulateSetupReceived(drv, setup);
  });

  ASSERT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        bool in_setup = false;
        dut_.RunInDriverContext([&](Dwc3& drv) {
          in_setup = (Dwc3TestHelper::GetEp0State(drv) == Dwc3TestHelper::State::Setup);
        });
        return in_setup;
      },
      zx::sec(5)));

  dut_.RunInDriverContext([&](Dwc3& drv) {
    // With the FIDL limit fix, oversized requests are stalled and state returns to Setup.
    EXPECT_TRUE(Dwc3TestHelper::IsEp0OutStalled(drv));
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

// Verifies that a 3-stage Control IN transfer (which writes descriptor/data payload into
// ep0_.buffer) followed by status phase completion and immediately followed by the next control
// SETUP packet receives and decodes the next setup packet with 100% fidelity without cross-transfer
// corruption.
TEST_F(UnmanagedTestFixture, Ep0ControlInThenNextSetupPacket) {
  FakeUsbDciInterface fake_dci;
  SetUpAndPowerOnDriver();

  std::vector<uint8_t> read_data(32, 0xAA);
  fake_dci.SetReadData(std::move(read_data));

  auto last_received_request = std::make_shared<std::atomic<uint8_t>>(0);
  auto control_call_count = std::make_shared<std::atomic<int>>(0);
  fake_dci.SetControlCallback(
      [last_received_request, control_call_count](
          fuchsia_hardware_usb_descriptor::wire::UsbSetup setup, cpp20::span<const uint8_t> data) {
        last_received_request->store(setup.b_request);
        control_call_count->fetch_add(1);
      });

  auto binding = BindDciInterface(&fake_dci);

  // 1. Initial 3-stage Control IN transfer
  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    auto setup =
        MakeSetupPacket(fdescriptor::EndpointDirection::kIn | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x42, 0, 0, 32);
    Dwc3TestHelper::SimulateSetupReceived(drv, setup);
  });

  // Wait for the driver to queue the data phase transfer to hardware
  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        bool queued = false;
        dut_.RunInDriverContext(
            [&](Dwc3& drv) { queued = !Dwc3TestHelper::IsSharedFifoEmpty(drv); });
        return queued;
      },
      zx::sec(10)));

  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::DataIn);
    Dwc3TestHelper::SimulateDataInPhase(drv);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitNrdyOut);
    Dwc3TestHelper::SimulateStatusPhase(drv, /*is_in=*/false);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  EXPECT_EQ(control_call_count->load(), 1);
  EXPECT_EQ(last_received_request->load(), 0x42);

  // 2. Immediately deliver the NEXT Setup packet (SET_CONFIGURATION request = 0x09)
  dut_.RunInDriverContext([&](Dwc3& drv) {
    fuchsia_hardware_usb_descriptor::wire::UsbSetup next_setup;
    next_setup.bm_request_type = kStandardDeviceOut;
    next_setup.b_request = fidl::ToUnderlying(fdescriptor::StandardRequest::kSetConfiguration);
    next_setup.w_value = 1;
    next_setup.w_index = 0;
    next_setup.w_length = 0;

    Dwc3TestHelper::SimulateSetupReceived(drv, next_setup);
  });

  // Wait for the async FIDL Control response to execute and configure the device
  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        bool configured = false;
        dut_.RunInDriverContext([&](Dwc3& drv) {
          configured = (Dwc3TestHelper::GetDeviceState(drv) ==
                        fuchsia_hardware_usb_policy::wire::DeviceState::kConfigured);
        });
        return configured;
      },
      zx::sec(10)));

  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_EQ(Dwc3TestHelper::GetDeviceState(drv),
              fuchsia_hardware_usb_policy::wire::DeviceState::kConfigured);
  });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

// Verifies that alternating back-to-back Control IN and Control OUT transfers preserve
// setup packet integrity and do not corrupt memory or stall the EP0 state machine.
TEST_F(UnmanagedTestFixture, MultipleSequentialControlTransfers) {
  FakeUsbDciInterface fake_dci;
  SetUpAndPowerOnDriver();

  std::vector<uint8_t> read_data(64, 0x55);
  fake_dci.SetReadData(std::move(read_data));

  auto control_call_count = std::make_shared<std::atomic<int>>(0);
  fake_dci.SetControlCallback(
      [control_call_count](fuchsia_hardware_usb_descriptor::wire::UsbSetup setup,
                           cpp20::span<const uint8_t> data) { control_call_count->fetch_add(1); });

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);
  });

  for (uint8_t iter = 0; iter < 5; ++iter) {
    // 1. Control IN transfer
    dut_.RunInDriverContext([&](Dwc3& drv) {
      auto setup =
          MakeSetupPacket(fdescriptor::EndpointDirection::kIn | fdescriptor::RequestType::kVendor |
                              fdescriptor::RequestRecipient::kDevice,
                          static_cast<uint8_t>(0x10 + iter), 0, 0, 64);
      Dwc3TestHelper::SimulateSetupReceived(drv, setup);
    });

    EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
        [&]() {
          bool queued = false;
          dut_.RunInDriverContext(
              [&](Dwc3& drv) { queued = !Dwc3TestHelper::IsSharedFifoEmpty(drv); });
          return queued;
        },
        zx::sec(10)));

    dut_.RunInDriverContext([&](Dwc3& drv) {
      EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::DataIn);
      Dwc3TestHelper::SimulateDataInPhase(drv);
      EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitNrdyOut);
      Dwc3TestHelper::SimulateStatusPhase(drv, /*is_in=*/false);
      EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
    });

    // 2. Control OUT 2-stage transfer (SET_ADDRESS)
    dut_.RunInDriverContext([&](Dwc3& drv) {
      auto setup = MakeSetupPacket(kStandardDeviceOut,
                                   fidl::ToUnderlying(fdescriptor::StandardRequest::kSetAddress),
                                   static_cast<uint16_t>(1 + iter), 0, 0);
      Dwc3TestHelper::SimulateSetupReceived(drv, setup);
      EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitHost);
      Dwc3TestHelper::SimulateStatusPhase(drv, /*is_in=*/true);
      EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
    });
  }

  EXPECT_EQ(control_call_count->load(), 5);

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

// Verifies that a 2-stage Control OUT transfer (SET_ADDRESS) followed by Status IN phase
// completion and immediately followed by the next control SETUP packet decodes cleanly.
TEST_F(UnmanagedTestFixture, TwoStageControlOutThenNextSetupPacket) {
  FakeUsbDciInterface fake_dci;
  SetUpAndPowerOnDriver();

  auto received_request = std::make_shared<std::atomic<uint8_t>>(0);
  fake_dci.SetControlCallback(
      [received_request](fuchsia_hardware_usb_descriptor::wire::UsbSetup setup,
                         cpp20::span<const uint8_t> data) {
        received_request->store(setup.b_request);
      });

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    // 1. First 2-stage SET_ADDRESS request
    auto setup =
        MakeSetupPacket(kStandardDeviceOut,
                        fidl::ToUnderlying(fdescriptor::StandardRequest::kSetAddress), 0x05, 0, 0);
    Dwc3TestHelper::SimulateSetupReceived(drv, setup);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitHost);
    Dwc3TestHelper::SimulateStatusPhase(drv, /*is_in=*/true);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);

    // 2. Immediately deliver next GET_DESCRIPTOR setup packet
    auto next_setup = MakeGetDescriptorSetup(64);
    Dwc3TestHelper::SimulateSetupReceived(drv, next_setup);
  });

  // Verify that the driver decoded the next request
  dut_.runtime().RunUntil([&]() {
    return received_request->load() ==
           fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor);
  });
  EXPECT_EQ(received_request->load(),
            fidl::ToUnderlying(fdescriptor::StandardRequest::kGetDescriptor));

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

// Verifies that a 3-stage Control OUT transfer receives only the exact payload length
// without contamination from residual bytes previously residing in ep0_.buffer.
TEST_F(UnmanagedTestFixture, ControlDataOutWithResidualBufferData) {
  FakeUsbDciInterface fake_dci;
  SetUpAndPowerOnDriver();

  auto received_payload = std::make_shared<std::vector<uint8_t>>();
  auto control_called = std::make_shared<libsync::Completion>();
  fake_dci.SetControlCallback(
      [control_called, received_payload](fuchsia_hardware_usb_descriptor::wire::UsbSetup setup,
                                         cpp20::span<const uint8_t> data) {
        received_payload->assign(data.begin(), data.end());
        control_called->Signal();
      });

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    // Pre-populate ep0_.buffer with residual garbage pattern
    uint8_t garbage[256];
    std::memset(garbage, 0xEE, sizeof(garbage));
    Dwc3TestHelper::WriteEp0Buffer(drv, garbage, 0, sizeof(garbage));

    // Setup 16-byte Vendor OUT transfer
    auto setup =
        MakeSetupPacket(fdescriptor::EndpointDirection::kOut | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x33, 0, 0, 16);
    Dwc3TestHelper::SimulateSetupReceived(drv, setup);
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::DataOut);

    // Simulate 16 bytes of valid data written to buffer
    uint8_t payload[16];
    std::memset(payload, 0x42, sizeof(payload));
    Dwc3TestHelper::WriteEp0Buffer(drv, payload, 0, sizeof(payload));
    Dwc3TestHelper::SimulateDataOutPhase(drv, 16);
  });

  ASSERT_EQ(control_called->Wait(zx::sec(10)), ZX_OK);
  ASSERT_EQ(received_payload->size(), 16u);
  for (size_t i = 0; i < 16; ++i) {
    EXPECT_EQ((*received_payload)[i], 0x42);
  }

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

// Verifies that a Control IN transfer requiring a ZLP followed by status completion
// leaves the EP0 state machine ready to process the next incoming setup packet.
TEST_F(UnmanagedTestFixture, ControlInZlpThenNextSetupPacket) {
  FakeUsbDciInterface fake_dci;
  SetUpAndPowerOnDriver();

  // Return 512 bytes where Host wLength is 1024 (multiple of MPS, requiring ZLP)
  std::vector<uint8_t> read_data(512, 0x77);
  fake_dci.SetReadData(std::move(read_data));

  auto next_received_request = std::make_shared<std::atomic<uint8_t>>(0);
  fake_dci.SetControlCallback(
      [next_received_request](fuchsia_hardware_usb_descriptor::wire::UsbSetup setup,
                              cpp20::span<const uint8_t> data) {
        if (setup.b_request != 0x50) {
          next_received_request->store(setup.b_request);
        }
      });

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    auto setup =
        MakeSetupPacket(fdescriptor::EndpointDirection::kIn | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x50, 0, 0, 1024);
    Dwc3TestHelper::SimulateSetupReceived(drv, setup);
  });

  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        bool queued = false;
        dut_.RunInDriverContext(
            [&](Dwc3& drv) { queued = !Dwc3TestHelper::IsSharedFifoEmpty(drv); });
        return queued;
      },
      zx::sec(10)));

  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::DataIn);
    Dwc3TestHelper::SimulateDataInPhase(drv);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitZlpIn);
    Dwc3TestHelper::SimulateDataInPhase(drv);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitNrdyOut);
    Dwc3TestHelper::SimulateStatusPhase(drv, /*is_in=*/false);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);

    // Deliver next Setup packet
    auto next_setup =
        MakeSetupPacket(fdescriptor::EndpointDirection::kIn | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x51, 0, 0, 64);
    Dwc3TestHelper::SimulateSetupReceived(drv, next_setup);
  });

  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() { return next_received_request->load() == 0x51; }, zx::sec(10)));
  EXPECT_EQ(next_received_request->load(), 0x51);

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

// Verifies that stall recovery after a protocol error cleanly flushes FIFO descriptors
// and resets EP0 to State::Setup to safely accept subsequent Setup packets.
TEST_F(UnmanagedTestFixture, Ep0StallRecoveryLeavesCleanBufferForNextSetup) {
  FakeUsbDciInterface fake_dci;
  SetUpAndPowerOnDriver();

  auto next_received_request = std::make_shared<std::atomic<uint8_t>>(0);
  fake_dci.SetControlCallback(
      [next_received_request](fuchsia_hardware_usb_descriptor::wire::UsbSetup setup,
                              cpp20::span<const uint8_t> data) {
        next_received_request->store(setup.b_request);
      });

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    // 1. Simulate directional protocol error during DataOut phase: receiving XferNotReady(Data)
    // on the opposite endpoint (EP0 IN / ep_num = 1) indicates the host reversed direction,
    // which triggers Ep0EndAndStall(ep0_.out).
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::DataOut);
    Dwc3TestHelper::HandleEp0TransferNotReadyEvent(drv, 1, DEPEVT_XFER_NOT_READY_STAGE_DATA);

    // Endpoint should stall and re-queue setup
    EXPECT_TRUE(Dwc3TestHelper::IsEp0OutStalled(drv));
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);

    // 2. Deliver new Setup packet (which clears stall)
    auto setup =
        MakeSetupPacket(fdescriptor::EndpointDirection::kIn | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x60, 0, 0, 64);
    Dwc3TestHelper::SimulateSetupReceived(drv, setup);
    EXPECT_FALSE(Dwc3TestHelper::IsEp0OutStalled(drv));
  });

  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() { return next_received_request->load() == 0x60; }, zx::sec(10)));
  EXPECT_EQ(next_received_request->load(), 0x60);

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

// Verifies that ep0_.buffer is allocated with caching disabled (uncached) to prevent
// CPU cache line writeback hazards (Clean operations during CacheFlushInvalidate) from
// clobbering incoming SETUP packets delivered directly to RAM via hardware DMA.
TEST_F(UnmanagedTestFixture, Ep0BufferIsUncachedToPreventDmaClobber) {
  SetUpAndPowerOnDriver();

  dut_.RunInDriverContext([&](Dwc3& drv) {
    // ep0_.buffer must be allocated with CacheOptions::kDisabled.
    EXPECT_FALSE(Dwc3TestHelper::GetEp0BufferEnableCache(drv));
    EXPECT_EQ(Dwc3TestHelper::GetEp0BufferCacheOptions(drv), dma_buffer::CacheOptions::kDisabled);
  });

  TearDownAndPowerOffDriver();
}

// Verifies that dirty cache lines from preceding data/status phases do not overwrite
// incoming SETUP packets written to RAM by hardware DMA when Ep0QueueSetup executes.
TEST_F(UnmanagedTestFixture, Ep0QueueSetupWithDirtyCacheDoesNotClobberIncomingDmaSetupPacket) {
  FakeUsbDciInterface fake_dci;
  SetUpAndPowerOnDriver();

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    // 1. Simulate dirty cache lines remaining in ep0_.buffer from previous transfer.
    // Direct memcpy into virt() leaves bytes dirty in CPU cache without issuing a CacheFlush.
    uint8_t dirty[sizeof(fuchsia_hardware_usb_descriptor::wire::UsbSetup)];
    std::memset(dirty, 0xAA, sizeof(dirty));
    std::memcpy(Dwc3TestHelper::GetEp0BufferVirt(drv), dirty, sizeof(dirty));

    // 2. Simulate hardware DMA writing a fresh incoming Setup packet directly to backing physical
    // RAM/VMO
    fuchsia_hardware_usb_descriptor::wire::UsbSetup new_setup;
    new_setup.bm_request_type = kStandardDeviceOut;
    new_setup.b_request = fidl::ToUnderlying(fdescriptor::StandardRequest::kSetConfiguration);
    new_setup.w_value = 1;
    new_setup.w_index = 0;
    new_setup.w_length = 0;

    Dwc3TestHelper::WriteEp0Buffer(drv, &new_setup, 0, sizeof(new_setup));

    // 3. Driver prepares for Setup packet
    Dwc3TestHelper::Ep0QueueSetup(drv);

    // 4. Trigger Setup Complete event from hardware
    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);  // EP0 OUT
  });

  // Verify that the driver decoded the new setup packet rather than stale dirty 0xAA bytes
  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        bool configured = false;
        dut_.RunInDriverContext([&](Dwc3& drv) {
          configured = (Dwc3TestHelper::GetDeviceState(drv) ==
                        fuchsia_hardware_usb_policy::wire::DeviceState::kConfigured);
        });
        return configured;
      },
      zx::sec(10)));

  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_EQ(Dwc3TestHelper::GetDeviceState(drv),
              fuchsia_hardware_usb_policy::wire::DeviceState::kConfigured);
  });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, Ep0OutBabbleUnderflow) {
  SetUpAndPowerOnDriver();

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::DataOut);
    Dwc3TestHelper::SetEp0CurTransferLen(drv, 16);

    const uint64_t initial_bytes = Dwc3TestHelper::GetEp0OutTotalBytes(drv);

    dwc3_trb_t trb{};
    trb.status = TRB_BUFSIZ(32);
    Dwc3TestHelper::PushTrbToSharedFifo(drv, trb);

    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);

    EXPECT_EQ(Dwc3TestHelper::GetEp0OutTotalBytes(drv), initial_bytes);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, Ep0InBabbleUnderflow) {
  SetUpAndPowerOnDriver();

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::DataIn);
    Dwc3TestHelper::SetEp0CurTransferLen(drv, 16);

    const uint64_t initial_bytes = Dwc3TestHelper::GetEp0InTotalBytes(drv);

    dwc3_trb_t trb{};
    trb.status = TRB_BUFSIZ(32);
    Dwc3TestHelper::PushTrbToSharedFifo(drv, trb);

    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 1);

    EXPECT_EQ(Dwc3TestHelper::GetEp0InTotalBytes(drv), initial_bytes);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitNrdyOut);
  });

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, ControlWriteMaxPacketMultipleDataOut) {
  SetUpAndPowerOnDriver();

  FakeUsbDciInterface fake_dci;
  auto control_called = std::make_shared<libsync::Completion>();
  auto received_data = std::make_shared<std::vector<uint8_t>>();

  fake_dci.SetControlCallback(
      [control_called, received_data](fuchsia_hardware_usb_descriptor::wire::UsbSetup setup,
                                      cpp20::span<const uint8_t> data) {
        received_data->assign(data.begin(), data.end());
        control_called->Signal();
      });

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    auto setup =
        MakeSetupPacket(fdescriptor::EndpointDirection::kOut | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x01, 0, 0, 512);
    Dwc3TestHelper::SimulateSetupReceived(drv, setup);

    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::DataOut);
    EXPECT_EQ(Dwc3TestHelper::GetEp0CurTransferLen(drv), 512u);
    ASSERT_FALSE(Dwc3TestHelper::IsSharedFifoEmpty(drv));
    dwc3_trb_t* trb = Dwc3TestHelper::GetCurrentReadTrb(drv);
    ASSERT_NE(trb, nullptr);
    EXPECT_EQ(TRB_TRBCTL(trb->control), static_cast<uint32_t>(TRB_TRBCTL_CONTROL_DATA));
    EXPECT_EQ(TRB_BUFSIZ(trb->status), 512u);

    std::vector<uint8_t> payload(512, 0x5A);
    Dwc3TestHelper::WriteEp0Buffer(drv, payload.data(), 0, payload.size());
    Dwc3TestHelper::SimulateDataOutPhase(drv, 512);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitNrdyIn);
  });

  ASSERT_EQ(control_called->Wait(zx::sec(10)), ZX_OK);
  ASSERT_EQ(received_data->size(), 512u);
  for (uint8_t byte : *received_data) {
    EXPECT_EQ(byte, 0x5A);
  }

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SimulateStatusPhase(drv, /*is_in=*/true);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, ControlWriteHighSpeedMaxPacketMultipleDataOut) {
  SetUpAndPowerOnDriver();

  FakeUsbDciInterface fake_dci;
  auto control_called = std::make_shared<libsync::Completion>();
  auto received_data = std::make_shared<std::vector<uint8_t>>();

  fake_dci.SetControlCallback(
      [control_called, received_data](fuchsia_hardware_usb_descriptor::wire::UsbSetup setup,
                                      cpp20::span<const uint8_t> data) {
        received_data->assign(data.begin(), data.end());
        control_called->Signal();
      });

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::SetEp0MaxPacketSize(drv, 64);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    auto setup =
        MakeSetupPacket(fdescriptor::EndpointDirection::kOut | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x01, 0, 0, 64);
    Dwc3TestHelper::SimulateSetupReceived(drv, setup);

    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::DataOut);
    EXPECT_EQ(Dwc3TestHelper::GetEp0CurTransferLen(drv), 64u);
    ASSERT_FALSE(Dwc3TestHelper::IsSharedFifoEmpty(drv));
    dwc3_trb_t* trb = Dwc3TestHelper::GetCurrentReadTrb(drv);
    ASSERT_NE(trb, nullptr);
    EXPECT_EQ(TRB_TRBCTL(trb->control), static_cast<uint32_t>(TRB_TRBCTL_CONTROL_DATA));
    EXPECT_EQ(TRB_BUFSIZ(trb->status), 64u);

    std::vector<uint8_t> payload(64, 0x3C);
    Dwc3TestHelper::WriteEp0Buffer(drv, payload.data(), 0, payload.size());
    Dwc3TestHelper::SimulateDataOutPhase(drv, 64);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitNrdyIn);
  });

  ASSERT_EQ(control_called->Wait(zx::sec(10)), ZX_OK);
  ASSERT_EQ(received_data->size(), 64u);
  for (uint8_t byte : *received_data) {
    EXPECT_EQ(byte, 0x3C);
  }

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SimulateStatusPhase(drv, /*is_in=*/true);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, ControlWriteUnalignedWLengthRoundsUpToMaxPacketSize) {
  SetUpAndPowerOnDriver();

  FakeUsbDciInterface fake_dci;
  auto control_called = std::make_shared<libsync::Completion>();
  auto received_data = std::make_shared<std::vector<uint8_t>>();

  fake_dci.SetControlCallback(
      [control_called, received_data](fuchsia_hardware_usb_descriptor::wire::UsbSetup setup,
                                      cpp20::span<const uint8_t> data) {
        received_data->assign(data.begin(), data.end());
        control_called->Signal();
      });

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::SetEp0MaxPacketSize(drv, 512);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    // 6-byte unaligned vendor control-write at SuperSpeed (MPS = 512).
    auto setup =
        MakeSetupPacket(fdescriptor::EndpointDirection::kOut | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x30, 0, 0, 6);
    Dwc3TestHelper::SimulateSetupReceived(drv, setup);

    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::DataOut);
    EXPECT_EQ(Dwc3TestHelper::GetEp0CurTransferLen(drv), 512u);
    ASSERT_FALSE(Dwc3TestHelper::IsSharedFifoEmpty(drv));
    dwc3_trb_t* trb = Dwc3TestHelper::GetCurrentReadTrb(drv);
    ASSERT_NE(trb, nullptr);
    EXPECT_EQ(TRB_TRBCTL(trb->control), static_cast<uint32_t>(TRB_TRBCTL_CONTROL_DATA));
    EXPECT_EQ(TRB_BUFSIZ(trb->status), 512u);

    std::vector<uint8_t> payload(6, 0xA5);
    Dwc3TestHelper::WriteEp0Buffer(drv, payload.data(), 0, payload.size());
    Dwc3TestHelper::SimulateDataOutPhase(drv, 6);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitNrdyIn);
  });

  ASSERT_EQ(control_called->Wait(zx::sec(10)), ZX_OK);
  ASSERT_EQ(received_data->size(), 6u);
  for (uint8_t byte : *received_data) {
    EXPECT_EQ(byte, 0xA5);
  }

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SimulateStatusPhase(drv, /*is_in=*/true);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, ControlWriteHighSpeedUnalignedMultiPacketRoundsUp) {
  SetUpAndPowerOnDriver();

  FakeUsbDciInterface fake_dci;
  auto control_called = std::make_shared<libsync::Completion>();
  auto received_data = std::make_shared<std::vector<uint8_t>>();

  fake_dci.SetControlCallback(
      [control_called, received_data](fuchsia_hardware_usb_descriptor::wire::UsbSetup setup,
                                      cpp20::span<const uint8_t> data) {
        received_data->assign(data.begin(), data.end());
        control_called->Signal();
      });

  auto binding = BindDciInterface(&fake_dci);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::SetEp0MaxPacketSize(drv, 64);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::Setup);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    // 100-byte unaligned multi-packet control-write at High-Speed (MPS = 64 -> TRB size = 128).
    auto setup =
        MakeSetupPacket(fdescriptor::EndpointDirection::kOut | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x31, 0, 0, 100);
    Dwc3TestHelper::SimulateSetupReceived(drv, setup);

    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::DataOut);
    EXPECT_EQ(Dwc3TestHelper::GetEp0CurTransferLen(drv), 128u);
    ASSERT_FALSE(Dwc3TestHelper::IsSharedFifoEmpty(drv));
    dwc3_trb_t* trb = Dwc3TestHelper::GetCurrentReadTrb(drv);
    ASSERT_NE(trb, nullptr);
    EXPECT_EQ(TRB_TRBCTL(trb->control), static_cast<uint32_t>(TRB_TRBCTL_CONTROL_DATA));
    EXPECT_EQ(TRB_BUFSIZ(trb->status), 128u);

    std::vector<uint8_t> payload(100, 0x7E);
    Dwc3TestHelper::WriteEp0Buffer(drv, payload.data(), 0, payload.size());
    Dwc3TestHelper::SimulateDataOutPhase(drv, 100);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitNrdyIn);
  });

  ASSERT_EQ(control_called->Wait(zx::sec(10)), ZX_OK);
  ASSERT_EQ(received_data->size(), 100u);
  for (uint8_t byte : *received_data) {
    EXPECT_EQ(byte, 0x7E);
  }

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SimulateStatusPhase(drv, /*is_in=*/true);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

// Verifies USB 2.0 Test Mode support for all standard test mode selectors, ensuring DCTL.TSTCTL is
// only written after the Status stage completes.
TEST_F(UnmanagedTestFixture, SetFeatureTestModeValidSelectors) {
  FakeUsbDciInterface fake_dci;
  SetUpAndPowerOnDriver();

  auto dci_called = std::make_shared<std::atomic<bool>>(false);
  fake_dci.SetControlCallback(
      [dci_called](fuchsia_hardware_usb_descriptor::wire::UsbSetup, cpp20::span<const uint8_t>) {
        dci_called->store(true);
      });
  auto binding = BindDciInterface(&fake_dci);

  constexpr uint8_t kValidSelectors[] = {
      fidl::ToUnderlying(fdescriptor::TestModeSelector::kTestJ),
      fidl::ToUnderlying(fdescriptor::TestModeSelector::kTestK),
      fidl::ToUnderlying(fdescriptor::TestModeSelector::kTestSe0Nak),
      fidl::ToUnderlying(fdescriptor::TestModeSelector::kTestPacket),
      fidl::ToUnderlying(fdescriptor::TestModeSelector::kTestForceEnable),
  };

  for (uint8_t selector : kValidSelectors) {
    // Initialize DCTL with RUN_STOP=1, TSTCTL=0, and a non-zero ULSTCHNGREQ readback value to
    // verify that read-modify-write masks ULSTCHNGREQ(0) and preserves RUN_STOP.
    dctl_val_.store(
        DCTL::Get().FromValue(0).set_RUN_STOP(1).set_ULSTCHNGREQ(8).set_TSTCTL(0).reg_value());

    std::atomic<uint32_t> dctl_write_count{0};
    auto cleanup_dctl = DeferClearDctlCallback();
    SetDctlCallback([&dctl_write_count]() { dctl_write_count.fetch_add(1); });

    dut_.RunInDriverContext([&](Dwc3& drv) {
      Dwc3TestHelper::SetEp0OutEnabled(drv, true);
      Dwc3TestHelper::SetEp0InEnabled(drv, true);
      Dwc3TestHelper::Ep0QueueSetup(drv);
      Dwc3TestHelper::SetControllerStarted(drv, true);

      auto setup = MakeSetupPacket(
          kStandardDeviceOut, fidl::ToUnderlying(fdescriptor::StandardRequest::kSetFeature),
          fidl::ToUnderlying(fdescriptor::FeatureSelector::kDeviceTestMode),
          static_cast<uint16_t>(selector << 8), 0);
      Dwc3TestHelper::SimulateSetupReceived(drv, setup);

      // Must wait for the host to initiate the Status stage without writing DCTL yet.
      EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitHost);
      EXPECT_EQ(Dwc3TestHelper::GetEp0TestMode(drv), selector);
      EXPECT_FALSE(Dwc3TestHelper::IsEp0OutStalled(drv));
      EXPECT_EQ(dctl_write_count.load(), 0u);

      // Host initiates Status stage (XferNotReady(Status) on EP0 IN).
      Dwc3TestHelper::HandleEp0TransferNotReadyEvent(drv, 1, DEPEVT_XFER_NOT_READY_STAGE_STATUS);
      EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Status);
      // DCTL must still not be written before Status stage XferComplete.
      EXPECT_EQ(dctl_write_count.load(), 0u);

      // Status stage completes (XferComplete on EP0 IN).
      Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 1);
      EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
      EXPECT_EQ(Dwc3TestHelper::GetEp0TestMode(drv), 0u);
    });

    EXPECT_EQ(dctl_write_count.load(), 1u);
    auto dctl_reg = DCTL::Get().FromValue(Read_DCTL());
    EXPECT_EQ(dctl_reg.TSTCTL(), static_cast<uint32_t>(selector));
    EXPECT_EQ(dctl_reg.RUN_STOP(), 1u);
    EXPECT_EQ(dctl_reg.ULSTCHNGREQ(), 0u);
    EXPECT_FALSE(dci_called->load());
  }

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

// Verifies that malformed SET_FEATURE(DEVICE_TEST_MODE) requests and
// CLEAR_FEATURE(DEVICE_TEST_MODE) requests stall EP0 OUT without entering a data stage or
// modifying DCTL.TSTCTL.
TEST_F(UnmanagedTestFixture, SetFeatureTestModeInvalidRequestsStall) {
  FakeUsbDciInterface fake_dci;
  SetUpAndPowerOnDriver();

  auto dci_called = std::make_shared<std::atomic<bool>>(false);
  fake_dci.SetControlCallback(
      [dci_called](fuchsia_hardware_usb_descriptor::wire::UsbSetup, cpp20::span<const uint8_t>) {
        dci_called->store(true);
      });
  auto binding = BindDciInterface(&fake_dci);

  std::atomic<uint32_t> dctl_write_count{0};
  auto cleanup_dctl = DeferClearDctlCallback();
  SetDctlCallback([&dctl_write_count]() { dctl_write_count.fetch_add(1); });

  const fuchsia_hardware_usb_descriptor::wire::UsbSetup kInvalidRequests[] = {
      // Reserved selector 0x00
      MakeSetupPacket(kStandardDeviceOut,
                      fidl::ToUnderlying(fdescriptor::StandardRequest::kSetFeature),
                      fidl::ToUnderlying(fdescriptor::FeatureSelector::kDeviceTestMode), 0x0000, 0),
      // Reserved selector 0x06 (above TestModeSelector::kTestForceEnable)
      MakeSetupPacket(kStandardDeviceOut,
                      fidl::ToUnderlying(fdescriptor::StandardRequest::kSetFeature),
                      fidl::ToUnderlying(fdescriptor::FeatureSelector::kDeviceTestMode), 0x0600, 0),
      // Vendor-specific selector 0xC0 (unsupported)
      MakeSetupPacket(kStandardDeviceOut,
                      fidl::ToUnderlying(fdescriptor::StandardRequest::kSetFeature),
                      fidl::ToUnderlying(fdescriptor::FeatureSelector::kDeviceTestMode), 0xC000, 0),
      // Non-zero low byte of wIndex
      MakeSetupPacket(kStandardDeviceOut,
                      fidl::ToUnderlying(fdescriptor::StandardRequest::kSetFeature),
                      fidl::ToUnderlying(fdescriptor::FeatureSelector::kDeviceTestMode),
                      static_cast<uint16_t>(
                          (fidl::ToUnderlying(fdescriptor::TestModeSelector::kTestJ) << 8) | 0x01),
                      0),
      // Non-zero wLength (must stall immediately without entering a data stage)
      MakeSetupPacket(
          kStandardDeviceOut, fidl::ToUnderlying(fdescriptor::StandardRequest::kSetFeature),
          fidl::ToUnderlying(fdescriptor::FeatureSelector::kDeviceTestMode),
          static_cast<uint16_t>(fidl::ToUnderlying(fdescriptor::TestModeSelector::kTestPacket)
                                << 8),
          8),
      // CLEAR_FEATURE(DEVICE_TEST_MODE) with wLength == 0
      MakeSetupPacket(
          kStandardDeviceOut, fidl::ToUnderlying(fdescriptor::StandardRequest::kClearFeature),
          fidl::ToUnderlying(fdescriptor::FeatureSelector::kDeviceTestMode),
          static_cast<uint16_t>(fidl::ToUnderlying(fdescriptor::TestModeSelector::kTestJ) << 8), 0),
      // CLEAR_FEATURE(DEVICE_TEST_MODE) with wLength != 0
      MakeSetupPacket(kStandardDeviceOut,
                      fidl::ToUnderlying(fdescriptor::StandardRequest::kClearFeature),
                      fidl::ToUnderlying(fdescriptor::FeatureSelector::kDeviceTestMode), 0, 4),
  };

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, 0, 1);
  });

  for (const auto& req : kInvalidRequests) {
    dut_.RunInDriverContext([&](Dwc3& drv) {
      Dwc3TestHelper::SimulateSetupReceived(drv, req);

      EXPECT_TRUE(Dwc3TestHelper::IsEp0OutStalled(drv));
      EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
      EXPECT_EQ(Dwc3TestHelper::GetEp0TestMode(drv), 0u);

      const uint8_t nrdy_ep = (req.w_length == 0) ? 1 : 0;
      const uint32_t nrdy_stage = (req.w_length == 0) ? DEPEVT_XFER_NOT_READY_STAGE_STATUS
                                                      : DEPEVT_XFER_NOT_READY_STAGE_DATA;
      Dwc3TestHelper::HandleEp0TransferNotReadyEvent(drv, nrdy_ep, nrdy_stage);
      Dwc3TestHelper::HandleEpTransferStartedEvent(drv, 0, 1);

      EXPECT_TRUE(Dwc3TestHelper::IsEp0OutStalled(drv));
      EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
    });
  }

  EXPECT_EQ(dctl_write_count.load(), 0u);
  EXPECT_FALSE(dci_called->load());

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

// Verifies that a pending or active test mode is cleared on stall recovery and bus reset.
TEST_F(UnmanagedTestFixture, SetFeatureTestModeClearedOnAbortedTransferAndReset) {
  SetUpAndPowerOnDriver();

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    // 1. Start a valid SET_FEATURE(DEVICE_TEST_MODE) request, then abort it before the Status stage
    // completes by delivering an unexpected XferNotReady(Data) event.
    auto test_setup = MakeSetupPacket(
        kStandardDeviceOut, fidl::ToUnderlying(fdescriptor::StandardRequest::kSetFeature),
        fidl::ToUnderlying(fdescriptor::FeatureSelector::kDeviceTestMode),
        static_cast<uint16_t>(fidl::ToUnderlying(fdescriptor::TestModeSelector::kTestK) << 8), 0);
    Dwc3TestHelper::SimulateSetupReceived(drv, test_setup);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitHost);
    EXPECT_EQ(Dwc3TestHelper::GetEp0TestMode(drv),
              fidl::ToUnderlying(fdescriptor::TestModeSelector::kTestK));

    Dwc3TestHelper::HandleEp0TransferNotReadyEvent(drv, 1, DEPEVT_XFER_NOT_READY_STAGE_DATA);
    EXPECT_TRUE(Dwc3TestHelper::IsEp0OutStalled(drv));
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
    EXPECT_EQ(Dwc3TestHelper::GetEp0TestMode(drv), 0u);

    // A subsequent two-stage control transfer (SET_ADDRESS) must not apply the aborted test mode.
    auto addr_setup = MakeSetupPacket(
        kStandardDeviceOut, fidl::ToUnderlying(fdescriptor::StandardRequest::kSetAddress), 5, 0, 0);
    Dwc3TestHelper::SimulateSetupReceived(drv, addr_setup);
    Dwc3TestHelper::SimulateStatusPhase(drv, /*is_in=*/true);
  });

  EXPECT_EQ(DCTL::Get().FromValue(Read_DCTL()).TSTCTL(), 0u);

  // 2. Complete a valid SET_FEATURE(DEVICE_TEST_MODE) request so DCTL.TSTCTL becomes non-zero, then
  // verify that a bus reset clears DCTL.TSTCTL back to 0.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto test_setup = MakeSetupPacket(
        kStandardDeviceOut, fidl::ToUnderlying(fdescriptor::StandardRequest::kSetFeature),
        fidl::ToUnderlying(fdescriptor::FeatureSelector::kDeviceTestMode),
        static_cast<uint16_t>(fidl::ToUnderlying(fdescriptor::TestModeSelector::kTestPacket) << 8),
        0);
    Dwc3TestHelper::SimulateSetupReceived(drv, test_setup);
    Dwc3TestHelper::SimulateStatusPhase(drv, /*is_in=*/true);
  });

  EXPECT_EQ(DCTL::Get().FromValue(Read_DCTL()).TSTCTL(),
            static_cast<uint32_t>(fidl::ToUnderlying(fdescriptor::TestModeSelector::kTestPacket)));

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::HandleResetEvent(drv);
    EXPECT_EQ(Dwc3TestHelper::GetEp0TestMode(drv), 0u);
  });

  EXPECT_EQ(DCTL::Get().FromValue(Read_DCTL()).TSTCTL(), 0u);
  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, Ep0CancelTransfersAdvancesRingWithoutClear) {
  SetUpAndPowerOnDriver();

  std::vector<zx_paddr_t> start_trb_addrs;
  uint64_t last_par0 = 0;
  uint64_t last_par1 = 0;
  auto cleanup_depcmd = DeferClearDepcmdCallbacks(0);
  auto cleanup_deppar = fit::defer([this]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[DEPCMDPAR0::Get(0).addr()].SetWriteCallback([](uint64_t) {});
      env.reg_region()[DEPCMDPAR1::Get(0).addr()].SetWriteCallback([](uint64_t) {});
    });
  });
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    env.reg_region()[DEPCMDPAR0::Get(0).addr()].SetWriteCallback(
        [&](uint64_t val) { last_par0 = val; });
    env.reg_region()[DEPCMDPAR1::Get(0).addr()].SetWriteCallback(
        [&](uint64_t val) { last_par1 = val; });
    env.reg_region()[DEPCMD::Get(0).addr()].SetWriteCallback([&](uint64_t val_raw) {
      DEPCMD cmd = DEPCMD::Get(0).FromValue(static_cast<uint32_t>(val_raw));
      if (cmd.CMDTYP() == DEPCMD::DEPSTRTXFER) {
        start_trb_addrs.push_back((static_cast<zx_paddr_t>(last_par0) << 32) | last_par1);
      }
    });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    // 1. Queue initial Setup TRB and mark its DEPSTRTXFER started.
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, 0, 1);

    dwc3_trb_t* first_trb = Dwc3TestHelper::GetCurrentReadTrb(drv);
    ASSERT_NE(first_trb, nullptr);
    EXPECT_NE(first_trb->control & TRB_HWO, 0u);
    EXPECT_EQ(Dwc3TestHelper::GetEpQueuedTrbs(drv, 0), 1u);
    EXPECT_EQ(Dwc3TestHelper::GetEpQueuedTrbs(drv, 1), 0u);

    // 2. Cancel via Ep0Reset(): TRB_HWO must be cleared and queued_trbs reset to 0.
    Dwc3TestHelper::Ep0Reset(drv);
    EXPECT_EQ(Dwc3TestHelper::GetEpQueuedTrbs(drv, 0), 0u);
    EXPECT_EQ(Dwc3TestHelper::GetEpQueuedTrbs(drv, 1), 0u);
    EXPECT_TRUE(Dwc3TestHelper::IsSharedFifoEmpty(drv));
    EXPECT_EQ(first_trb->control & TRB_HWO, 0u);

    // 3. Next Ep0QueueSetup() must write at first_trb + 1 instead of rewinding to first_.
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, 0, 2);

    dwc3_trb_t* second_trb = Dwc3TestHelper::GetCurrentReadTrb(drv);
    EXPECT_EQ(second_trb, first_trb + 1);
    EXPECT_NE(second_trb->control & TRB_HWO, 0u);
    EXPECT_EQ(Dwc3TestHelper::GetEpQueuedTrbs(drv, 0), 1u);

    // 4. Cancel via Ep0EndAndStall() (triggered by opposite-direction XferNotReady(Data) during
    // DataOut stage), which cancels second_trb and immediately queues a third Setup TRB at
    // first_trb + 2.
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::DataOut);
    Dwc3TestHelper::HandleEp0TransferNotReadyEvent(drv, 1, DEPEVT_XFER_NOT_READY_STAGE_DATA);
    EXPECT_EQ(second_trb->control & TRB_HWO, 0u);
    EXPECT_EQ(Dwc3TestHelper::GetEpQueuedTrbs(drv, 0), 1u);
    EXPECT_EQ(Dwc3TestHelper::GetEpQueuedTrbs(drv, 1), 0u);
    EXPECT_EQ(Dwc3TestHelper::GetCurrentReadTrb(drv), first_trb + 2);
  });

  ASSERT_EQ(start_trb_addrs.size(), 3u);
  EXPECT_EQ(start_trb_addrs[1], start_trb_addrs[0] + sizeof(dwc3_trb_t));
  EXPECT_EQ(start_trb_addrs[2], start_trb_addrs[0] + (2 * sizeof(dwc3_trb_t)));

  TearDownAndPowerOffDriver();
}

// Verifies that receiving XferNotReady(Data/Status) while already in Ep0::State::Setup (for
// example, after an unsupported control request stalls and re-queues the next Setup TRB) does not
// emit a duplicate DEPSTRTXFER or subsequent DEPENDXFER on DEPCMD(0) that aborts the active Setup
// transfer in hardware.
TEST_F(UnmanagedTestFixture, XferNotReadyInSetupStatePreservesActiveSetupTrb) {
  SetUpAndPowerOnDriver();

  FakeUsbDciInterface fake_dci;
  fake_dci.SetControlStatus(ZX_ERR_NOT_SUPPORTED);
  auto binding = BindDciInterface(&fake_dci);

  // Track transfer commands (DEPSTRTXFER / DEPENDXFER) written to DEPCMD(0) (EP0_OUT) and model
  // whether the DWC3 hardware currently has an active Setup transfer armed on EP0_OUT.
  auto depcmd0_transfer_cmds = std::make_shared<std::vector<uint32_t>>();
  auto hw_ep0_out_transfer_active = std::make_shared<std::atomic<bool>>(false);
  auto duplicate_start_xfer_count = std::make_shared<std::atomic<uint32_t>>(0);

  auto cleanup_depcmd = DeferClearDepcmdCallbacks(0);
  dut_.RunInEnvironmentTypeContext([depcmd0_transfer_cmds, hw_ep0_out_transfer_active,
                                    duplicate_start_xfer_count](Environment& env) {
    env.reg_region()[DEPCMD::Get(0).addr()].SetWriteCallback(
        [depcmd0_transfer_cmds, hw_ep0_out_transfer_active,
         duplicate_start_xfer_count](uint64_t val_raw) {
          const uint32_t val = static_cast<uint32_t>(val_raw);
          const uint32_t cmd_type = DEPCMD::Get(0).FromValue(val).CMDTYP();
          if (cmd_type == DEPCMD::DEPSTRTXFER) {
            depcmd0_transfer_cmds->push_back(cmd_type);
            if (hw_ep0_out_transfer_active->exchange(true)) {
              // Real DWC3 silicon rejects a duplicate DEPSTRTXFER with NO_RESOURCE if a transfer is
              // already active on the endpoint.
              duplicate_start_xfer_count->fetch_add(1);
            }
          } else if (cmd_type == DEPCMD::DEPENDXFER) {
            depcmd0_transfer_cmds->push_back(cmd_type);
            hw_ep0_out_transfer_active->store(false);
          }
        });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, 0, 1);

    // 1. Host sends an unsupported vendor Control-IN request, consuming the initial Setup TRB.
    ASSERT_TRUE(hw_ep0_out_transfer_active->exchange(false));
    depcmd0_transfer_cmds->clear();

    auto setup = MakeSetupPacket(kVendorDeviceIn, 0x99, 0, 0, 8);
    Dwc3TestHelper::SimulateSetupReceived(drv, setup);
  });

  // Wait for DoControlCall() error callback to run fail() -> Ep0QueueSetup(), which issues
  // DEPSTRTXFER #1 on DEPCMD(0) to listen for the next Setup packet.
  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        bool done = false;
        dut_.RunInDriverContext([&](Dwc3& drv) {
          done = (Dwc3TestHelper::GetEp0State(drv) == Dwc3TestHelper::State::Setup);
        });
        return done;
      },
      zx::sec(10)));

  dut_.RunInDriverContext([&](Dwc3& drv) {
    // 2. Hardware delivers XferNotReady(Data) for the stalled control request while DEPSTRTXFER #1
    // is in flight, followed by the command-complete event (HandleEpTransferStartedEvent) for
    // DEPSTRTXFER #1.
    // On broken code, XferNotReady emits a duplicate DEPSTRTXFER #2 while #1 is in flight, and
    // HandleEpTransferStartedEvent then emits DEPENDXFER to abort #1, leaving 0 active transfers.
    Dwc3TestHelper::HandleEp0TransferNotReadyEvent(drv, 1, DEPEVT_XFER_NOT_READY_STAGE_DATA);
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, 0, 1);

    EXPECT_EQ(depcmd0_transfer_cmds->size(), 1u);
    if (!depcmd0_transfer_cmds->empty()) {
      EXPECT_EQ((*depcmd0_transfer_cmds)[0], static_cast<uint32_t>(DEPCMD::DEPSTRTXFER));
    }
    EXPECT_EQ(duplicate_start_xfer_count->load(), 0u);
    EXPECT_TRUE(hw_ep0_out_transfer_active->load());

    // 3. Because the hardware still has an active, un-canceled DEPSTRTXFER on DEPCMD(0), it can
    // receive the next Setup packet (SET_ADDRESS).
    ASSERT_TRUE(hw_ep0_out_transfer_active->exchange(false));
    depcmd0_transfer_cmds->clear();

    auto next_setup = MakeSetupPacket(
        kStandardDeviceOut, fidl::ToUnderlying(fdescriptor::StandardRequest::kSetAddress), 5, 0, 0);
    Dwc3TestHelper::SimulateSetupReceived(drv, next_setup);
    EXPECT_FALSE(Dwc3TestHelper::IsEp0OutStalled(drv));
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitHost);
    Dwc3TestHelper::SimulateStatusPhase(drv, /*is_in=*/true);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);

    // 4. Verify the same DEPCMD(0) invariant when DEPEVT_XFER_NOT_READY with XferActive reason
    // arrives on EP0_OUT while the newly queued Setup TRB is in flight.
    ASSERT_EQ(depcmd0_transfer_cmds->size(), 1u);
    EXPECT_EQ((*depcmd0_transfer_cmds)[0], static_cast<uint32_t>(DEPCMD::DEPSTRTXFER));
    ASSERT_TRUE(hw_ep0_out_transfer_active->load());

    const uint32_t xfer_active_nrdy =
        DEPEVT_XFER_NOT_READY_REASON |
        (DEPEVT_XFER_NOT_READY_STAGE_DATA << DEPEVT_XFER_NOT_READY_STAGE_START) |
        (DEPEVT_XFER_NOT_READY << DEPEVT_TYPE_START);
    Dwc3TestHelper::HandleEvent(drv, xfer_active_nrdy);
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, 0, 1);

    EXPECT_EQ(depcmd0_transfer_cmds->size(), 1u);
    EXPECT_EQ(duplicate_start_xfer_count->load(), 0u);
    EXPECT_TRUE(hw_ep0_out_transfer_active->load());
  });

  if (binding.has_value()) {
    binding->Unbind();
    dut_.runtime().RunUntilIdle();
  }

  TearDownAndPowerOffDriver();
}

// Verifies that if Ep0QueueSetup() fails to arm a Setup TRB (e.g. due to CacheFlushInvalidate
// failing on the ReadStruct error path while in Ep0::State::Setup), ep0_.state moves off
// Ep0::State::Setup to Ep0::State::None so a subsequent XferNotReady event re-arms Ep0QueueSetup().
TEST_F(UnmanagedTestFixture, Ep0QueueSetupFailureLeavesSetupStateAndRecoversOnXferNotReady) {
  class FailingContiguousBuffer final : public dma_buffer::ContiguousBuffer {
   public:
    const dma_buffer::Buffer& buffer() const override { return buffer_; }
    zx_paddr_t phys() const override { return 0; }
    zx::unowned_vmo vmo() const override { return zx::unowned_vmo{}; }

   private:
    // A zero-length cached buffer causes ReadStruct() and CacheFlushInvalidate() to fail with
    // ZX_ERR_OUT_OF_RANGE.
    dma_buffer::Buffer buffer_{nullptr, 0, dma_buffer::CacheOptions::kEnabled};
  };

  SetUpAndPowerOnDriver();

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::SetControllerStarted(drv, true);
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, 0, 1);
    ASSERT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);

    // Swap in a buffer whose ReadStruct() and CacheFlushInvalidate() fail, then trigger Setup
    // completion so HandleEp0TransferCompleteEvent hits the ReadStruct failure path ->
    // Ep0EndAndStall() -> Ep0QueueSetup() failure.
    auto real_buffer =
        Dwc3TestHelper::SwapEp0Buffer(drv, std::make_unique<FailingContiguousBuffer>());
    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);

    EXPECT_TRUE(Dwc3TestHelper::IsEp0OutStalled(drv));
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::None);

    // Restore the real EP0 buffer and verify that XferNotReady(Data) in Ep0::State::None recovers
    // and re-arms Ep0QueueSetup().
    Dwc3TestHelper::SwapEp0Buffer(drv, std::move(real_buffer));
    Dwc3TestHelper::HandleEp0TransferNotReadyEvent(drv, 0, DEPEVT_XFER_NOT_READY_STAGE_DATA);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  TearDownAndPowerOffDriver();
}

// Verifies that receiving DEPEVT_XFER_NOT_READY with XferActive reason outside Ep0::State::Setup
// ends any active Data/Status transfer on EP0 OUT or EP0 IN before re-queuing Setup.
TEST_F(UnmanagedTestFixture, XferActiveNotReadyOutsideSetupEndsActiveTransfer) {
  SetUpAndPowerOnDriver();

  auto depcmd0_end_count = std::make_shared<std::atomic<uint32_t>>(0);
  auto depcmd1_end_count = std::make_shared<std::atomic<uint32_t>>(0);

  auto cleanup_depcmd0 = DeferClearDepcmdCallbacks(0);
  auto cleanup_depcmd1 = DeferClearDepcmdCallbacks(1);
  dut_.RunInEnvironmentTypeContext([depcmd0_end_count, depcmd1_end_count](Environment& env) {
    env.reg_region()[DEPCMD::Get(0).addr()].SetWriteCallback([depcmd0_end_count](uint64_t val_raw) {
      if (DEPCMD::Get(0).FromValue(static_cast<uint32_t>(val_raw)).CMDTYP() == DEPCMD::DEPENDXFER) {
        depcmd0_end_count->fetch_add(1);
      }
    });
    env.reg_region()[DEPCMD::Get(1).addr()].SetWriteCallback([depcmd1_end_count](uint64_t val_raw) {
      if (DEPCMD::Get(1).FromValue(static_cast<uint32_t>(val_raw)).CMDTYP() == DEPCMD::DEPENDXFER) {
        depcmd1_end_count->fetch_add(1);
      }
    });
  });

  constexpr uint32_t kXferActiveNrdy =
      DEPEVT_XFER_NOT_READY_REASON |
      (DEPEVT_XFER_NOT_READY_STAGE_DATA << DEPEVT_XFER_NOT_READY_STAGE_START) |
      (DEPEVT_XFER_NOT_READY << DEPEVT_TYPE_START);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    // 1. Active DataOut transfer on EP0 OUT (ep_num = 0).
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::DataOut);
    Dwc3TestHelper::PushTrbToSharedFifo(drv, dwc3_trb_t{.control = TRB_HWO});
    Dwc3TestHelper::SetEpTransferState(drv, 0, Dwc3TestHelper::TransferState::kActiveSingle);
    Dwc3TestHelper::SetEpRsrcId(drv, 0, 1);

    Dwc3TestHelper::HandleEvent(drv, kXferActiveNrdy);
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, 0, 1);
    EXPECT_EQ(depcmd0_end_count->load(), 1u);
    EXPECT_EQ(depcmd1_end_count->load(), 0u);
    EXPECT_TRUE(Dwc3TestHelper::IsEp0OutStalled(drv));
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);

    // 2. Active DataIn transfer on EP0 IN (ep_num = 1).
    Dwc3TestHelper::SetEpTransferState(drv, 0, Dwc3TestHelper::TransferState::kIdle);
    Dwc3TestHelper::SetEpRsrcId(drv, 0, Endpoint::kInvalidResourceId);
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::DataIn);
    Dwc3TestHelper::PushTrbToSharedFifo(drv, dwc3_trb_t{.control = TRB_HWO});
    Dwc3TestHelper::SetEpTransferState(drv, 1, Dwc3TestHelper::TransferState::kActiveSingle);
    Dwc3TestHelper::SetEpRsrcId(drv, 1, 2);

    Dwc3TestHelper::HandleEvent(drv, kXferActiveNrdy);
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, 0, 1);
    EXPECT_EQ(depcmd0_end_count->load(), 1u);
    EXPECT_EQ(depcmd1_end_count->load(), 1u);
    EXPECT_TRUE(Dwc3TestHelper::IsEp0OutStalled(drv));
    EXPECT_FALSE(Dwc3TestHelper::IsEp0InStalled(drv));
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
  });

  TearDownAndPowerOffDriver();
}

TEST_F(UnmanagedTestFixture, Ep0SetupPendingAbortsControlTransferAndRearmsSetup) {
  SetUpAndPowerOnDriver();

  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::SetEp0OutEnabled(drv, true);
    Dwc3TestHelper::SetEp0InEnabled(drv, true);
    Dwc3TestHelper::SetControllerStarted(drv, true);

    // 1. Setup interrupted by TRB_TRBSTS_SETUP_PENDING: must discard the superseded Setup packet
    // without executing HandleEp0Setup or stalling EP0 OUT, and immediately re-arm Ep0QueueSetup().
    Dwc3TestHelper::Ep0QueueSetup(drv);
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, 0, 1);
    auto superseded_setup = MakeSetupPacket(
        kStandardDeviceOut, fidl::ToUnderlying(fdescriptor::StandardRequest::kSetAddress), 9, 0, 0);
    Dwc3TestHelper::WriteEp0Buffer(drv, &superseded_setup, 0, sizeof(superseded_setup));
    dwc3_trb_t* setup_trb = Dwc3TestHelper::GetCurrentReadTrb(drv);
    ASSERT_NE(setup_trb, nullptr);
    setup_trb->status = (TRB_TRBSTS_SETUP_PENDING << TRB_TRBSTS_START);
    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);

    EXPECT_FALSE(Dwc3TestHelper::IsEp0OutStalled(drv));
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
    EXPECT_EQ(Dwc3TestHelper::GetDeviceState(drv),
              fuchsia_hardware_usb_policy::wire::DeviceState::kPowered);
    ASSERT_NE(Dwc3TestHelper::GetCurrentReadTrb(drv), nullptr);
    EXPECT_NE(Dwc3TestHelper::GetCurrentReadTrb(drv)->control & TRB_HWO, 0u);
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, 0, 2);

    // 2. DataOut interrupted by TRB_TRBSTS_SETUP_PENDING, followed by trailing
    // DEPEVT_XFER_NOT_READY(Control_Status) from the aborted transfer.
    auto out_setup =
        MakeSetupPacket(fdescriptor::EndpointDirection::kOut | fdescriptor::RequestType::kVendor |
                            fdescriptor::RequestRecipient::kDevice,
                        0x10, 0, 0, 16);
    Dwc3TestHelper::SimulateSetupReceived(drv, out_setup);
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, 0, 3);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::DataOut);

    dwc3_trb_t* trb = Dwc3TestHelper::GetCurrentReadTrb(drv);
    ASSERT_NE(trb, nullptr);
    trb->status = (TRB_TRBSTS_SETUP_PENDING << TRB_TRBSTS_START) | TRB_BUFSIZ(512);
    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 0);

    EXPECT_FALSE(Dwc3TestHelper::IsEp0OutStalled(drv));
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
    dwc3_trb_t* rearmed_after_data_out = Dwc3TestHelper::GetCurrentReadTrb(drv);
    ASSERT_NE(rearmed_after_data_out, nullptr);
    EXPECT_NE(rearmed_after_data_out->control & TRB_HWO, 0u);
    // Trailing XferNotReady(Status) from the aborted DataOut transfer must preserve the newly armed
    // Setup TRB without cancelling or re-queuing it.
    Dwc3TestHelper::HandleEp0TransferNotReadyEvent(drv, 1, DEPEVT_XFER_NOT_READY_STAGE_STATUS);
    EXPECT_EQ(Dwc3TestHelper::GetCurrentReadTrb(drv), rearmed_after_data_out);
    EXPECT_NE(rearmed_after_data_out->control & TRB_HWO, 0u);
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, 0, 4);

    // 3. DataIn interrupted by TRB_TRBSTS_SETUP_PENDING, followed by trailing
    // DEPEVT_XFER_NOT_READY(Control_Status).
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::DataIn);
    Dwc3TestHelper::SetEp0CurTransferLen(drv, 64);
    dwc3_trb_t data_in_trb{};
    data_in_trb.status = (TRB_TRBSTS_SETUP_PENDING << TRB_TRBSTS_START) | TRB_BUFSIZ(64);
    Dwc3TestHelper::PushTrbToSharedFifo(drv, data_in_trb);
    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 1);

    EXPECT_FALSE(Dwc3TestHelper::IsEp0OutStalled(drv));
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
    dwc3_trb_t* rearmed_after_data_in = Dwc3TestHelper::GetCurrentReadTrb(drv);
    ASSERT_NE(rearmed_after_data_in, nullptr);
    EXPECT_NE(rearmed_after_data_in->control & TRB_HWO, 0u);
    Dwc3TestHelper::HandleEp0TransferNotReadyEvent(drv, 0, DEPEVT_XFER_NOT_READY_STAGE_STATUS);
    EXPECT_EQ(Dwc3TestHelper::GetCurrentReadTrb(drv), rearmed_after_data_in);
    EXPECT_NE(rearmed_after_data_in->control & TRB_HWO, 0u);
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, 0, 5);

    // 4. WaitZlpIn interrupted by TRB_TRBSTS_SETUP_PENDING, followed by trailing
    // DEPEVT_XFER_NOT_READY(Control_Status).
    Dwc3TestHelper::SetEp0State(drv, Dwc3TestHelper::State::WaitZlpIn);
    dwc3_trb_t zlp_in_trb{};
    zlp_in_trb.status = (TRB_TRBSTS_SETUP_PENDING << TRB_TRBSTS_START);
    Dwc3TestHelper::PushTrbToSharedFifo(drv, zlp_in_trb);
    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 1);

    EXPECT_FALSE(Dwc3TestHelper::IsEp0OutStalled(drv));
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
    dwc3_trb_t* rearmed_after_zlp_in = Dwc3TestHelper::GetCurrentReadTrb(drv);
    ASSERT_NE(rearmed_after_zlp_in, nullptr);
    EXPECT_NE(rearmed_after_zlp_in->control & TRB_HWO, 0u);
    Dwc3TestHelper::HandleEp0TransferNotReadyEvent(drv, 0, DEPEVT_XFER_NOT_READY_STAGE_STATUS);
    EXPECT_EQ(Dwc3TestHelper::GetCurrentReadTrb(drv), rearmed_after_zlp_in);
    EXPECT_NE(rearmed_after_zlp_in->control & TRB_HWO, 0u);
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, 0, 6);

    // 5. Status (with latched test_mode) interrupted by TRB_TRBSTS_SETUP_PENDING must NOT issue
    // EP0_OUT_STALL and must NOT apply test_mode to DCTL.TSTCTL.
    auto test_setup = MakeSetupPacket(
        kStandardDeviceOut, fidl::ToUnderlying(fdescriptor::StandardRequest::kSetFeature),
        fidl::ToUnderlying(fdescriptor::FeatureSelector::kDeviceTestMode),
        static_cast<uint16_t>(fidl::ToUnderlying(fdescriptor::TestModeSelector::kTestPacket) << 8),
        0);
    Dwc3TestHelper::SimulateSetupReceived(drv, test_setup);
    EXPECT_FALSE(Dwc3TestHelper::IsEp0OutStalled(drv));
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::WaitHost);
    Dwc3TestHelper::HandleEp0TransferNotReadyEvent(drv, 1, DEPEVT_XFER_NOT_READY_STAGE_STATUS);
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, 1, 7);
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Status);

    dwc3_trb_t* status_trb = Dwc3TestHelper::GetCurrentReadTrb(drv);
    ASSERT_NE(status_trb, nullptr);
    status_trb->status = (TRB_TRBSTS_SETUP_PENDING << TRB_TRBSTS_START);
    Dwc3TestHelper::HandleEp0TransferCompleteEvent(drv, 1);

    EXPECT_FALSE(Dwc3TestHelper::IsEp0OutStalled(drv));
    EXPECT_EQ(Dwc3TestHelper::GetEp0State(drv), Dwc3TestHelper::State::Setup);
    EXPECT_EQ(Dwc3TestHelper::GetEp0TestMode(drv), 0u);
    dwc3_trb_t* final_setup_trb = Dwc3TestHelper::GetCurrentReadTrb(drv);
    ASSERT_NE(final_setup_trb, nullptr);
    EXPECT_NE(final_setup_trb->control & TRB_HWO, 0u);
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, 0, 8);
  });

  EXPECT_EQ(DCTL::Get().FromValue(Read_DCTL()).TSTCTL(), 0u);
  TearDownAndPowerOffDriver();
}
}  // namespace dwc3
