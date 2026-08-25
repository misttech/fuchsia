// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/sync/cpp/completion.h>

#include <gtest/gtest.h>

#include "src/devices/usb/drivers/dwc3/dwc3-test-fixture.h"
#include "src/devices/usb/drivers/dwc3/dwc3.h"
#include "src/lib/testing/predicates/status.h"

namespace dwc3 {

namespace fendpoint = fuchsia_hardware_usb_endpoint;
namespace fdescriptor = fuchsia_hardware_usb_descriptor;
namespace fdci = fuchsia_hardware_usb_dci;
namespace frequest = fuchsia_hardware_usb_request;

namespace {
void AssertZlpUnchainedControlBits(const std::vector<dwc3_trb_t>& trbs, bool enqueue_many) {
  ASSERT_GE(trbs.size(), 2u);
  // First TRB must exactly match normal and hardware-owned (unchained). Due to deadlock prevention,
  // it must now also have IOC.
  uint32_t expected = TRB_TRBCTL_NORMAL | TRB_IOC | TRB_HWO;
  EXPECT_EQ(expected, trbs[0].control);

  // Ongoing transfers do not use TRB_LST
  uint32_t expected_control = TRB_TRBCTL_NORMAL | TRB_IOC | TRB_HWO;
  if (!enqueue_many) {
    expected_control |= TRB_LST;
  }
  EXPECT_EQ(expected_control, trbs[1].control);
  EXPECT_EQ(0u, TRB_BUFSIZ(trbs[1].status));
}
}  // namespace

// Test fixture parameterized over whether enqueueing multiple TRBs is enabled.
class Dwc3EndpointsTest : public TestFixture<true, testing::TestWithParam<bool>> {
 public:
  static constexpr uint32_t kResourceId = 12;

  void SetUp() override {
    TestFixture::SetUp();
    dut_.RunInDriverContext([&](Dwc3& drv) { drv.SetEnableEnqueueManyTrbs(GetParam()); });
    dut_.RunInEnvironmentTypeContext([&](Environment& env) {
      // Mock GHWPARAMS0 to return MDWIDTH = 2 (128-bit = 16 bytes).
      auto& ghwparams0 = env.reg_region()[GHWPARAMS0::Get().addr()];
      ghwparams0.SetReadCallback([]() -> uint32_t {
        return GHWPARAMS0::Get().FromValue(0).set_DWC_USB31_MDWIDTH(2).reg_value();
      });
      // Mock GRXFIFOSIZ for FIFO 0 to have depth 64 (1024 bytes).
      auto& grxfifosiz0 = env.reg_region()[GRXFIFOSIZ::Get(0).addr()];
      grxfifosiz0.SetReadCallback(
          []() -> uint32_t { return GRXFIFOSIZ::Get(0).FromValue(0).set_RXFDEP(64).reg_value(); });
      // Mock GTXFIFOSIZ for FIFOs to have depth 64 (1024 bytes).
      for (unsigned i = 0; i < 16; i++) {
        auto& gtxfifosiz = env.reg_region()[GTXFIFOSIZ::Get(i).addr()];
        gtxfifosiz.SetReadCallback([i]() -> uint32_t {
          return GTXFIFOSIZ::Get(i).FromValue(0).set_TXFDEP(64).reg_value();
        });
      }
    });

    // Start the client loop thread to process async callbacks.
    ASSERT_EQ(client_loop_.StartThread("client-loop"), ZX_OK);
  }

  void TearDown() override {
    client_loop_.Shutdown();
    TestFixture::TearDown();
  }

 protected:
  void TriggerConnection(bool start_controller = true,
                         fdescriptor::UsbSpeed speed = fdescriptor::UsbSpeed::kSuper) {
    TriggerConnectionPlugIn(speed);

    auto dci_service = dut_.Connect<fdci::UsbDciService::Device>();
    ASSERT_TRUE(dci_service.is_ok())
        << "Failed to connect to UsbDciService: " << dci_service.status_string();
    dci_.Bind(std::move(*dci_service));

    if (start_controller) {
      fidl::WireResult res = dci_->StartController();
      ASSERT_OK(res.status());
    }
  }

  void SetupEndpoint(uint8_t ep_address, fdescriptor::EndpointType ep_type,
                     uint16_t max_packet_size) {
    fdescriptor::wire::UsbEndpointDescriptor ep_desc{
        .b_length = sizeof(fdescriptor::wire::UsbEndpointDescriptor),
        .b_descriptor_type = USB_DT_ENDPOINT,
        .b_endpoint_address = ep_address,
        .bm_attributes = static_cast<uint8_t>(ep_type),
        .w_max_packet_size = max_packet_size,
        .b_interval = 0,
    };
    fdescriptor::wire::UsbSsEpCompDescriptor ss_comp_desc{
        .b_length = sizeof(fdescriptor::wire::UsbSsEpCompDescriptor),
        .b_descriptor_type = USB_DT_SS_EP_COMPANION,
        .b_max_burst = 0,
        .bm_attributes = 0,
        .w_bytes_per_interval = 0,
    };

    fidl::WireResult config_res = dci_->ConfigureEndpoint(ep_desc, ss_comp_desc);
    ASSERT_OK(config_res.status());
    ASSERT_TRUE(config_res.value().is_ok())
        << "ConfigureEndpoint protocol failed: "
        << zx_status_get_string(config_res.value().error_value());

    zx::result endpoints = fidl::CreateEndpoints<fendpoint::Endpoint>();
    ASSERT_OK(endpoints);
    auto [client_end, server_end] = std::move(*endpoints);

    fidl::WireResult conn_res = dci_->ConnectToEndpoint(ep_address, std::move(server_end));
    ASSERT_OK(conn_res.status());
    ASSERT_TRUE(conn_res.value().is_ok()) << "ConnectToEndpoint protocol failed: "
                                          << zx_status_get_string(conn_res.value().error_value());

    ep_client_.Bind(std::move(client_end), client_loop_.dispatcher(), &event_handler_);
  }

  void RegisterVmo(uint8_t vmo_id, uint64_t size) {
    fidl::Arena arena;
    fendpoint::wire::VmoInfo vmo_info =
        fendpoint::wire::VmoInfo::Builder(arena).id(vmo_id).size(size).Build();

    fidl::WireResult result = ep_client_.wire_sync()->RegisterVmos(
        fidl::VectorView<fendpoint::wire::VmoInfo>::FromExternal(&vmo_info, 1));
    ASSERT_OK(result.status());
    EXPECT_EQ(1UL, result->vmos.size());
    EXPECT_EQ(vmo_id, result->vmos[0].id());
  }

  void QueueRequest(uint8_t vmo_id, uint64_t offset, uint64_t size,
                    fdescriptor::EndpointType ep_type, bool short_bit = false) {
    QueueRequests(1, vmo_id, offset, size, ep_type, short_bit);
  }

  void QueueRequests(size_t count, uint8_t vmo_id, uint64_t offset, uint64_t size,
                     fdescriptor::EndpointType ep_type, bool short_bit = false) {
    std::vector<frequest::Request> reqs;
    for (size_t i = 0; i < count; i++) {
      frequest::Buffer buffer = frequest::Buffer::WithVmoId(vmo_id);

      frequest::BufferRegion region;
      region.buffer(std::move(buffer));
      region.offset(offset + (size * i));
      region.size(size);

      std::vector<frequest::BufferRegion> regions;
      regions.push_back(std::move(region));

      frequest::RequestInfo req_info =
          (ep_type == fdescriptor::EndpointType::kBulk)
              ? frequest::RequestInfo::WithBulk(frequest::BulkRequestInfo{})
              : frequest::RequestInfo::WithInterrupt(frequest::InterruptRequestInfo{});

      frequest::Request req;
      req.data(std::move(regions));
      req.defer_completion(false);
      req.information(std::move(req_info));
      req.short_(short_bit);
      reqs.push_back(std::move(req));
    }

    fit::result result = ep_client_->QueueRequests({std::move(reqs)});
    ASSERT_TRUE(result.is_ok()) << "QueueRequests failed: "
                                << result.error_value().FormatDescription();
  }

  void WaitForState(uint8_t ep_num, TransferState expected_state) {
    dut_.runtime().RunUntil([&]() {
      dut_.runtime().RunUntilIdle();
      return dut_.RunInDriverContext<bool>([&](Dwc3& drv) {
        return GetUserEndpoint(drv, ep_num).ep.transfer_state == expected_state;
      });
    });
  }

  void WaitForQueuedCount(uint8_t ep_num, size_t count) {
    dut_.runtime().RunUntil([&]() {
      dut_.runtime().RunUntilIdle();
      return dut_.RunInDriverContext<bool>([&](Dwc3& drv) {
        return GetUserEndpoint(drv, ep_num).server->queued_reqs.size() == count;
      });
    });
  }

  void WaitForActiveCount(uint8_t ep_num, size_t count) {
    dut_.runtime().RunUntil([&]() {
      dut_.runtime().RunUntilIdle();
      return dut_.RunInDriverContext<bool>([&](Dwc3& drv) {
        return GetUserEndpoint(drv, ep_num).server->active_reqs.size() == count;
      });
    });
  }

  async::Loop client_loop_{&kAsyncLoopConfigNeverAttachToThread};
  fidl::WireSyncClient<fdci::UsbDci> dci_;

  struct CompletionResult {
    zx_status_t status;
    uint64_t transfer_size;
  };

  class EventHandler : public fidl::AsyncEventHandler<fendpoint::Endpoint> {
   public:
    void OnCompletion(fidl::Event<fendpoint::Endpoint::OnCompletion>& event) override {
      std::lock_guard<std::mutex> lock(mutex_);
      for (const auto& completion : event.completion()) {
        completions_.push_back(CompletionResult{
            .status = completion.status().value_or(ZX_ERR_INTERNAL),
            .transfer_size = completion.transfer_size().value_or(0),
        });
      }
      completion_cond_.notify_all();
    }

    std::vector<CompletionResult> WaitForCompletions(size_t count) {
      std::unique_lock<std::mutex> lock(mutex_);
      completion_cond_.wait(lock, [&]() { return completions_.size() >= count; });

      size_t take = std::min(count, completions_.size());
      std::vector<CompletionResult> res(std::make_move_iterator(completions_.begin()),
                                        std::make_move_iterator(completions_.begin() + take));
      completions_.erase(completions_.begin(), completions_.begin() + take);
      return res;
    }

    size_t completion_count() {
      std::lock_guard<std::mutex> lock(mutex_);
      return completions_.size();
    }

   private:
    std::mutex mutex_;
    std::condition_variable completion_cond_;
    std::vector<CompletionResult> completions_;
  };

  EventHandler event_handler_;
  fidl::SharedClient<fendpoint::Endpoint> ep_client_;
};

TEST_P(Dwc3EndpointsTest, InterruptEndpointQueueAndComplete) {
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  // Interrupt endpoints are always using a single-transfer setup.
  SetupEndpoint(ep_address, fdescriptor::EndpointType::kInterrupt, 64);
  RegisterVmo(1, 4096);

  // Initially transfer state is kIdle.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(TransferState::kIdle, uep.ep.transfer_state);
    EXPECT_FALSE(uep.ep.got_not_ready);
  });

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_TRUE(uep.ep.got_not_ready);
    EXPECT_EQ(TransferState::kIdle, uep.ep.transfer_state);
  });

  // Client queues a request.
  QueueRequest(1, 0, 64, fdescriptor::EndpointType::kInterrupt);
  WaitForState(ep_num, TransferState::kStartingSingle);

  // Trigger started event to initialize rsrc_id and transition to active.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, TransferState::kActiveSingle);

  // Check state transitions to kActiveSingle.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(1u, uep.fifo.GetActiveCount());
  });

  // Host sends Transfer Complete event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferComplete(drv, ep_num); });
  WaitForState(ep_num, TransferState::kIdle);

  // State should be back to kIdle.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(0u, uep.fifo.GetActiveCount());
  });

  // Verify completion is received.
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1UL);
  EXPECT_OK(completions[0].status);
  EXPECT_EQ(64UL, completions[0].transfer_size);
}

TEST_P(Dwc3EndpointsTest, BulkEndpointQueueAndComplete) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue first request.
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  auto expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(1u, uep.fifo.GetActiveCount());
  });

  // Queue second request.
  QueueRequest(1, 512, 512, fdescriptor::EndpointType::kBulk);
  WaitForQueuedCount(ep_num, 1u);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(expected_starting_state, uep.ep.transfer_state);
    EXPECT_EQ(1u, uep.fifo.GetActiveCount());
    EXPECT_EQ(1u, uep.server->queued_reqs.size());
    EXPECT_EQ(1u, uep.server->active_reqs.size());
  });

  // Trigger started event for first request to initialize rsrc_id.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  auto expected_first_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_first_state);

  if (enqueue_many) {
    // If enqueue_many is enabled, starting the transfer automatically queues the next queued
    // requests.
    WaitForActiveCount(ep_num, 2u);

    dut_.RunInDriverContext([&](Dwc3& drv) {
      auto& uep = GetUserEndpoint(drv, ep_num);
      EXPECT_EQ(kResourceId, uep.ep.rsrc_id);
      EXPECT_EQ(2u, uep.fifo.GetActiveCount());
      EXPECT_EQ(0u, uep.server->queued_reqs.size());
    });

    // Complete request 1.
    dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferInProgress(drv, ep_num); });
    WaitForActiveCount(ep_num, 1u);

    dut_.RunInDriverContext([&](Dwc3& drv) {
      auto& uep = GetUserEndpoint(drv, ep_num);
      EXPECT_EQ(TransferState::kActiveOngoing, uep.ep.transfer_state);
      EXPECT_EQ(1u, uep.fifo.GetActiveCount());
    });

    // Complete request 2.
    dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferInProgress(drv, ep_num); });
    WaitForActiveCount(ep_num, 0u);
  } else {
    // Complete request 1. This transitions state to kIdle, and UserEpQueueNext kicks in to start
    // request 2.
    dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferComplete(drv, ep_num); });
    WaitForState(ep_num, TransferState::kStartingSingle);

    dut_.RunInDriverContext([&](Dwc3& drv) {
      auto& uep = GetUserEndpoint(drv, ep_num);
      EXPECT_EQ(1u, uep.fifo.GetActiveCount());
      EXPECT_EQ(0u, uep.server->queued_reqs.size());
      EXPECT_EQ(1u, uep.server->active_reqs.size());
    });

    // Trigger started event for second request to initialize rsrc_id.
    dut_.RunInDriverContext(
        [&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId + 1); });
    WaitForState(ep_num, TransferState::kActiveSingle);

    // Complete request 2.
    dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferComplete(drv, ep_num); });
    WaitForState(ep_num, TransferState::kIdle);
  }

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    auto expected_final_state = enqueue_many ? TransferState::kActiveOngoing : TransferState::kIdle;
    EXPECT_EQ(expected_final_state, uep.ep.transfer_state);
    EXPECT_EQ(0u, uep.fifo.GetActiveCount());
    EXPECT_EQ(0u, uep.server->active_reqs.size());
  });

  // Verify completions.
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(2);
  ASSERT_EQ(completions.size(), 2UL);
  EXPECT_OK(completions[0].status);
  EXPECT_EQ(512UL, completions[0].transfer_size);
  EXPECT_OK(completions[1].status);
  EXPECT_EQ(512UL, completions[1].transfer_size);
}

TEST_P(Dwc3EndpointsTest, CancelAllRequests) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue two requests.
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  QueueRequest(1, 512, 512, fdescriptor::EndpointType::kBulk);
  WaitForQueuedCount(ep_num, 1u);

  auto expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);

  // Trigger started event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  auto expected_state = enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_state);

  if (enqueue_many) {
    WaitForActiveCount(ep_num, 2u);
  }

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(expected_state, uep.ep.transfer_state);
    if (enqueue_many) {
      EXPECT_EQ(2u, uep.server->active_reqs.size());
      EXPECT_EQ(0u, uep.server->queued_reqs.size());
    } else {
      EXPECT_EQ(1u, uep.server->active_reqs.size());
      EXPECT_EQ(1u, uep.server->queued_reqs.size());
    }
  });

  // Cancel all requests via client asynchronously.
  std::optional<fidl::Result<fendpoint::Endpoint::CancelAll>> cancel_result;
  libsync::Completion cancel_completed;
  ep_client_->CancelAll().Then([&](fidl::Result<fendpoint::Endpoint::CancelAll>& res) {
    cancel_result = res;
    cancel_completed.Signal();
  });

  WaitForState(ep_num, TransferState::kCanceling);
  EXPECT_FALSE(cancel_result.has_value());

  // The state should be kCanceling, and active_reqs should not be empty yet.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(enqueue_many ? 2u : 1u, uep.server->active_reqs.size());
  });

  // Hardware emits Command Complete (End Transfer) to acknowledge End Transfer.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferEnded(drv, ep_num); });
  WaitForState(ep_num, TransferState::kIdle);

  cancel_completed.Wait();
  ASSERT_TRUE(cancel_result.has_value());
  ASSERT_TRUE(cancel_result->is_ok());

  // Now, active_reqs should be empty.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(0u, uep.server->active_reqs.size());
  });

  // Verify completions returned with cancellation error.
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(2);
  ASSERT_EQ(completions.size(), 2UL);
  EXPECT_EQ(ZX_ERR_IO_NOT_PRESENT, completions[0].status);
  EXPECT_EQ(ZX_ERR_IO_NOT_PRESENT, completions[1].status);
}

TEST_P(Dwc3EndpointsTest, CancelAllRequestsOnControllerStop) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue two requests.
  QueueRequests(2, 1, 0, 512, fdescriptor::EndpointType::kBulk);
  WaitForActiveCount(ep_num, enqueue_many ? 2 : 1);

  auto expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);

  // Trigger started event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  auto expected_state = enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_state);

  if (enqueue_many) {
    WaitForActiveCount(ep_num, 2u);
  }

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(expected_state, uep.ep.transfer_state);
    if (enqueue_many) {
      EXPECT_EQ(2u, uep.server->active_reqs.size());
      EXPECT_EQ(0u, uep.server->queued_reqs.size());
    } else {
      EXPECT_EQ(1u, uep.server->active_reqs.size());
      EXPECT_EQ(1u, uep.server->queued_reqs.size());
    }
  });

  // Stop controller.
  fidl::WireResult res = dci_->StopController();
  ASSERT_OK(res.status());
  WaitForState(ep_num, TransferState::kIdle);

  // Now, active_reqs and queued_reqs should be empty.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(0u, uep.server->active_reqs.size());
    EXPECT_EQ(0u, uep.server->queued_reqs.size());
  });

  // Verify completions returned with cancellation error.
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(2);
  ASSERT_EQ(completions.size(), 2UL);
  EXPECT_EQ(ZX_ERR_IO_NOT_PRESENT, completions[0].status);
  EXPECT_EQ(ZX_ERR_IO_NOT_PRESENT, completions[1].status);
}

TEST_P(Dwc3EndpointsTest, CancelAllRequestsWhenControllerStopped) {
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);

  // Stop controller.
  fidl::WireResult res = dci_->StopController();
  ASSERT_OK(res.status());
  WaitForState(ep_num, TransferState::kIdle);

  // Cancel all requests via client. It should reply immediately because controller is stopped.
  libsync::Completion cancel_completed;
  std::optional<fidl::Result<fendpoint::Endpoint::CancelAll>> cancel_result;
  ep_client_->CancelAll().Then([&](fidl::Result<fendpoint::Endpoint::CancelAll>& res) {
    cancel_result = res;
    cancel_completed.Signal();
  });

  cancel_completed.Wait();
  ASSERT_TRUE(cancel_result.has_value());
  ASSERT_TRUE(cancel_result->is_ok());
}

TEST_P(Dwc3EndpointsTest, CancelAllRequestsWhenIdle) {
  TriggerConnection();

  const uint8_t ep_address = 0x02;

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);

  // Endpoint is idle (no requests queued).
  // Cancel all requests via client. It should reply immediately because endpoint is idle.
  libsync::Completion cancel_completed;
  std::optional<fidl::Result<fendpoint::Endpoint::CancelAll>> cancel_result;
  ep_client_->CancelAll().Then([&](fidl::Result<fendpoint::Endpoint::CancelAll>& res) {
    cancel_result = res;
    cancel_completed.Signal();
  });

  cancel_completed.Wait();
  ASSERT_TRUE(cancel_result.has_value());
  ASSERT_TRUE(cancel_result->is_ok());
}

TEST_P(Dwc3EndpointsTest, InputEndpointZlpComplete) {
  TriggerConnection();

  // 0x82 is an INPUT (IN) endpoint.
  const uint8_t ep_address = 0x82;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  // Configure endpoint as Bulk IN with max packet size 512.
  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue a request with short_bit = true, size = 512 (multiple of max packet
  // size).
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk, /*short_bit=*/true);

  // Wait for the endpoint state to become starting.
  bool enqueue_many = GetParam();
  TransferState expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);

  // Verify that two TRBs were written in the FIFO.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(2u, uep.fifo.GetActiveCount());
    const auto& trbs = uep.fifo.Read(2);

    AssertZlpUnchainedControlBits(trbs, enqueue_many);
  });

  // Trigger started event to initialize rsrc_id.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  TransferState expected_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_state);

  // Complete first TRB (data TRB).
  dut_.RunInDriverContext([&](Dwc3& drv) {
    if (enqueue_many) {
      TriggerEpTransferInProgress(drv, ep_num);
    } else {
      TriggerEpTransferComplete(drv, ep_num);
    }
  });
  dut_.runtime().RunUntilIdle();

  // The request should NOT be completed yet, because the ZLP TRB is still
  // pending.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(1u, uep.fifo.GetActiveCount());
  });

  dut_.runtime().RunUntilIdle();
  // Verify that no completions are received.
  EXPECT_EQ(0u, event_handler_.completion_count());

  // Complete the second TRB (ZLP TRB).
  dut_.RunInDriverContext([&](Dwc3& drv) {
    if (enqueue_many) {
      TriggerEpTransferInProgress(drv, ep_num);
    } else {
      TriggerEpTransferComplete(drv, ep_num);
    }
  });

  // Wait for the endpoint transfer to complete.
  if (enqueue_many) {
    WaitForActiveCount(ep_num, 0u);
  } else {
    WaitForState(ep_num, TransferState::kIdle);
  }

  // Verify that the completion is received.
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1UL);
  EXPECT_OK(completions[0].status);
  EXPECT_EQ(512UL, completions[0].transfer_size);
}

TEST_P(Dwc3EndpointsTest, RingBufferWraparoundZlp) {
  if (!GetParam()) {
    return;
  }
  TriggerConnection();

  const uint8_t ep_address = 0x82;  // IN endpoint
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Enqueue initial test request to trigger start_transfer (so it resets FIFO)
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk, false);
  WaitForState(ep_num, TransferState::kStartingOngoing);

  // Trigger started event to initialize rsrc_id and enter kActiveOngoing
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, TransferState::kActiveOngoing);

  // Complete the initial test request
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferInProgress(drv, ep_num); });
  dut_.runtime().RunUntilIdle();

  // Now manually advance write and read pointers to `last_ - 1`
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    // Advance the write pointer so it sits on the very last usable entry before the Link TRB
    // (index last_ - 1). This forces 2-TRB queries to wrap across the boundary.
    // TotalSlots() returns capacity minus Link TRB.
    // Thus TotalSlots() evaluates to 255. The Link TRB operates at index 255.
    // Therefore index 254 (TotalSlots() - 1) is strictly the last usable entry before the wrap.
    ASSERT_LT(uep.fifo.WriteOffset(), uep.fifo.TotalSlots());
    size_t advance_count = uep.fifo.TotalSlots() - 1 - uep.fifo.WriteOffset();
    for (size_t i = 0; i < advance_count; i++) {
      uep.fifo.AdvanceWrite();
      uep.fifo.AdvanceRead();
    }
    ASSERT_EQ(uep.fifo.GetActiveCount(), 0u);
    EXPECT_EQ(uep.fifo.TotalSlots() - 1, uep.fifo.WriteOffset())
        << "Failed to advance write pointer to the wraparound boundary";
    EXPECT_EQ(uep.fifo.TotalSlots() - 1, uep.fifo.ReadOffset())
        << "Failed to advance read pointer to the wraparound boundary";
  });

  // Enqueue the primary request that requires two TRBs and will trigger a ring buffer wraparound.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  QueueRequest(1, 512, 512, fdescriptor::EndpointType::kBulk, true);
  WaitForActiveCount(ep_num, 1);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(2u, uep.fifo.GetActiveCount());
    const auto& trbs = uep.fifo.Read(2);

    // Note: RingBufferWraparoundZlp is only ever executed with enqueue_many == true
    AssertZlpUnchainedControlBits(trbs, true);

    // Explicitly verify pointer addresses to prove wrap
    auto* initial_read = uep.fifo.current_read();
    uep.fifo.AdvanceRead();
    auto* wrapped_read = uep.fifo.current_read();

    // Distance should not be linear, proving it wrapped.
    EXPECT_LT(wrapped_read, initial_read);
  });
}

TEST_P(Dwc3EndpointsTest, InputEndpointMultiPacketZlpComplete) {
  TriggerConnection();

  const uint8_t ep_address = 0x82;  // IN endpoint
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  // Configure endpoint as Bulk IN with max packet size 512.
  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 8192);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue a multi-packet request (4096 bytes = 8 x 512-byte max packets) with short_bit = true.
  QueueRequest(1, 0, 4096, fdescriptor::EndpointType::kBulk, /*short_bit=*/true);

  bool enqueue_many = GetParam();
  TransferState expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);

  // Verify that two TRBs were written: 1 multi-packet Data TRB (4096 bytes) + 1 ZLP TRB.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(2u, uep.fifo.GetActiveCount());
    const auto& trbs = uep.fifo.Read(2);

    EXPECT_EQ(4096UL, TRB_BUFSIZ(trbs[0].status));
    EXPECT_EQ(0UL, TRB_BUFSIZ(trbs[1].status));
    AssertZlpUnchainedControlBits(trbs, enqueue_many);
  });

  // Trigger started event to initialize rsrc_id.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  TransferState expected_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_state);

  // Complete data TRB.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    if (enqueue_many) {
      TriggerEpTransferInProgress(drv, ep_num);
    } else {
      TriggerEpTransferComplete(drv, ep_num);
    }
  });
  dut_.runtime().RunUntilIdle();

  // Request pending completion of ZLP TRB.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(1u, uep.fifo.GetActiveCount());
  });

  // Complete ZLP TRB.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    if (enqueue_many) {
      TriggerEpTransferInProgress(drv, ep_num);
    } else {
      TriggerEpTransferComplete(drv, ep_num);
    }
  });

  if (enqueue_many) {
    WaitForActiveCount(ep_num, 0u);
  } else {
    WaitForState(ep_num, TransferState::kIdle);
  }

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1UL);
  EXPECT_OK(completions[0].status);
  EXPECT_EQ(4096UL, completions[0].transfer_size);
}

TEST_P(Dwc3EndpointsTest, OutEndpointRingBufferWraparound) {
  if (!GetParam()) {
    return;
  }
  TriggerConnection();

  const uint8_t ep_address = 0x02;  // OUT endpoint 2
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Enqueue initial test request to trigger start_transfer (resets FIFO)
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk, false);
  WaitForState(ep_num, TransferState::kStartingOngoing);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, TransferState::kActiveOngoing);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferInProgress(drv, ep_num); });
  dut_.runtime().RunUntilIdle();

  // Manually advance write and read pointers to `last_ - 1` (index 254)
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    ASSERT_LT(uep.fifo.WriteOffset(), uep.fifo.TotalSlots());
    size_t advance_count = uep.fifo.TotalSlots() - 1 - uep.fifo.WriteOffset();
    for (size_t i = 0; i < advance_count; i++) {
      uep.fifo.AdvanceWrite();
      uep.fifo.AdvanceRead();
    }
    ASSERT_EQ(uep.fifo.GetActiveCount(), 0u);
    EXPECT_EQ(uep.fifo.TotalSlots() - 1, uep.fifo.WriteOffset());
    EXPECT_EQ(uep.fifo.TotalSlots() - 1, uep.fifo.ReadOffset());
  });

  // Enqueue OUT request sitting on wraparound boundary.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  QueueRequest(1, 512, 1024, fdescriptor::EndpointType::kBulk, false);
  WaitForActiveCount(ep_num, 1);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(1u, uep.fifo.GetActiveCount());

    auto* initial_read = uep.fifo.current_read();
    uep.fifo.AdvanceRead();
    auto* wrapped_read = uep.fifo.current_read();

    // Verify wraparound across slot 255 Link TRB back to index 0
    EXPECT_LT(wrapped_read, initial_read);
  });
}
namespace {
INSTANTIATE_TEST_SUITE_P(Dwc3EndpointsTestCases, Dwc3EndpointsTest, testing::Bool());
}

TEST_P(Dwc3EndpointsTest, EndpointStallAndClear) {
  uint32_t last_depcmd = 0;
  bool write_called = false;

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    uep->ep.type = fuchsia_hardware_usb_descriptor::EndpointType::kBulk;
    uep->ep.max_packet_size = 512;

    // Enable the endpoint
    Dwc3TestHelper::EpSetConfig(drv, uep->ep, true);
  });

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(2).addr()];
    depcmd.SetWriteCallback([&](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      last_depcmd = val;
      write_called = true;
    });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    EXPECT_TRUE(uep->ep.enabled);
    Dwc3TestHelper::EpSetStall(drv, uep->ep, true);
  });

  EXPECT_TRUE(write_called);
  EXPECT_EQ(DEPCMD::DEPSSTALL, DEPCMD::Get(2).FromValue(last_depcmd).CMDTYP());

  write_called = false;
  last_depcmd = 0;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    EXPECT_TRUE(uep->ep.enabled);
    Dwc3TestHelper::EpSetStall(drv, uep->ep, false);
  });

  EXPECT_TRUE(write_called);
  EXPECT_EQ(DEPCMD::DEPCSTALL, DEPCMD::Get(2).FromValue(last_depcmd).CMDTYP());
}

TEST_P(Dwc3EndpointsTest, EndpointConfiguration) {
  bool depcfg_called = false;
  bool depxfercfg_called = false;
  bool dalepena_called = false;

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    uep->ep.type = fuchsia_hardware_usb_descriptor::EndpointType::kBulk;
    uep->ep.max_packet_size = 512;
  });

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(2).addr()];
    depcmd.SetWriteCallback([&](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      auto cmd = DEPCMD::Get(2).FromValue(val);
      if (cmd.CMDTYP() == DEPCMD::DEPCFG) {
        depcfg_called = true;
      } else if (cmd.CMDTYP() == DEPCMD::DEPXFERCFG) {
        depxfercfg_called = true;
      }
    });

    auto& dalepena = env.reg_region()[DALEPENA::Get().addr()];
    dalepena.SetWriteCallback([&](uint64_t val_raw) { dalepena_called = true; });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    Dwc3TestHelper::EpSetConfig(drv, uep->ep, true);
  });

  EXPECT_TRUE(depcfg_called);
  EXPECT_TRUE(depxfercfg_called);
  EXPECT_TRUE(dalepena_called);
}

TEST_P(Dwc3EndpointsTest, EndpointReset) {
  bool dalepena_called = false;
  uint32_t dalepena_val = 0;

  SetUpAndPowerOnEndpoints();

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    uep->ep.type = fuchsia_hardware_usb_descriptor::EndpointType::kBulk;
    uep->ep.max_packet_size = 512;

    // Enable it first
    Dwc3TestHelper::EpSetConfig(drv, uep->ep, true);

    // Set some flags
    Dwc3TestHelper::SetGotNotReady(drv, 2, true);
    Dwc3TestHelper::SetEpRsrcId(drv, 2, 5);
    Dwc3TestHelper::SetEpTransferState(drv, 2, Dwc3TestHelper::TransferState::kActiveSingle);
  });

  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& dalepena = env.reg_region()[DALEPENA::Get().addr()];
    dalepena.SetWriteCallback([&](uint64_t val_raw) {
      dalepena_called = true;
      dalepena_val = static_cast<uint32_t>(val_raw);
    });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    Dwc3TestHelper::EpReset(drv, uep->ep);

    // Verify flags are reset
    EXPECT_FALSE(Dwc3TestHelper::GetGotNotReady(drv, 2));
  });

  EXPECT_TRUE(dalepena_called);
  EXPECT_EQ(0u, dalepena_val & (1 << 2));
}

struct EndpointTransferSweepParams {
  uint8_t ep_addr;
  fdescriptor::EndpointType ep_type;
  uint32_t max_packet_size;
};

class Dwc3EndpointTransferSweepTest
    : public TestFixture<true>,
      public testing::WithParamInterface<EndpointTransferSweepParams> {};

TEST_P(Dwc3EndpointTransferSweepTest, DisconnectDuringActiveTransfer) {
  auto params = GetParam();
  uint8_t ep_addr = params.ep_addr;
  fdescriptor::EndpointType ep_type = params.ep_type;
  uint32_t max_packet_size = params.max_packet_size;

  SetUpAndPowerOnEndpoints();

  auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_endpoint::Endpoint>();
  ASSERT_TRUE(endpoints.is_ok());

  bool completed = false;
  zx_status_t completion_status = ZX_OK;
  TestEndpointEventHandler event_handler(completed, completion_status);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, ep_addr);
    ASSERT_NE(uep, nullptr);
    uep->ep.type = ep_type;
    uep->ep.max_packet_size = static_cast<uint16_t>(max_packet_size);
    uep->ep.enabled = true;
    uep->ep.got_not_ready = true;
    ASSERT_TRUE(Dwc3TestHelper::InitFifo(drv, ep_addr).is_ok());

    auto* dispatcher = Dwc3TestHelper::GetDispatcher(drv);
    uep->server->Connect(dispatcher, std::move(endpoints->server));
  });

  fidl::SyncClient<fuchsia_hardware_usb_endpoint::Endpoint> sync_client{
      std::move(endpoints->client)};

  auto vmo_res = CreateVmoBuffer(sync_client, max_packet_size, max_packet_size);
  auto requests = std::move(vmo_res.requests);

  libsync::Completion completion;
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(ep_addr).addr()];
    depcmd.SetWriteCallback([&](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      if (DEPCMD::Get(ep_addr).FromValue(val).CMDTYP() == DEPCMD::DEPSTRTXFER) {
        completion.Signal();
      }
    });
    depcmd.SetReadCallback([]() -> uint32_t { return 0; });
  });

  auto result = sync_client->QueueRequests({std::move(requests)});
  ASSERT_TRUE(result.is_ok()) << "QueueRequests failed: " << result.error_value().status_string();

  completion.Wait();

  // Safe Barrier Fulfillment: Simulate missing hardware interrupt to release deferred command!
  dut_.RunInDriverContext([&](Dwc3& drv) {
    constexpr uint8_t kMockRsrcId = 5;
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, ep_addr, kMockRsrcId);
  });

  EXPECT_OK(WaitForPhy());

  // Trigger disconnect
  this->dsts_val_.store(DSTS::Get()
                            .FromValue(this->dsts_val_.load())
                            .set_USBLNKST(DSTS::USBLNKST_DISCONNECTED)
                            .reg_value());
  dut_.RunInEnvironmentTypeContext([&](Environment& env) { env.usb_phy().TriggerDisconnect(); });

  // Simulate the hardware core posting the aborted interrupt event!
  dut_.RunInDriverContext(
      [&](Dwc3& drv) { Dwc3TestHelper::HandleEpTransferCompleteEvent(drv, ep_addr); });

  // Wait for request completion
  ASSERT_TRUE(sync_client.HandleOneEvent(event_handler).ok());
  EXPECT_TRUE(completed);
  EXPECT_TRUE(completion_status == ZX_ERR_IO_NOT_PRESENT || completion_status == ZX_ERR_CANCELED)
      << "Unexpected completion status: " << zx_status_get_string(completion_status);

  sync_client = {};
}

TEST_P(Dwc3EndpointTransferSweepTest, StallDuringActiveTransfer) {
  auto params = GetParam();
  uint8_t ep_addr = params.ep_addr;
  fdescriptor::EndpointType ep_type = params.ep_type;
  uint32_t max_packet_size = params.max_packet_size;

  SetUpAndPowerOnEndpoints();

  auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_endpoint::Endpoint>();
  ASSERT_TRUE(endpoints.is_ok());

  bool completed = false;
  zx_status_t completion_status = ZX_OK;
  TestEndpointEventHandler event_handler(completed, completion_status);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, ep_addr);
    ASSERT_NE(uep, nullptr);
    uep->ep.type = ep_type;
    uep->ep.max_packet_size = static_cast<uint16_t>(max_packet_size);
    uep->ep.enabled = true;
    uep->ep.got_not_ready = true;
    ASSERT_TRUE(Dwc3TestHelper::InitFifo(drv, ep_addr).is_ok());

    auto* dispatcher = Dwc3TestHelper::GetDispatcher(drv);
    uep->server->Connect(dispatcher, std::move(endpoints->server));
  });

  fidl::SyncClient<fuchsia_hardware_usb_endpoint::Endpoint> sync_client{
      std::move(endpoints->client)};

  auto vmo_res = CreateVmoBuffer(sync_client, max_packet_size, max_packet_size);
  auto requests = std::move(vmo_res.requests);

  libsync::Completion completion;
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(ep_addr).addr()];
    depcmd.SetWriteCallback([&](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      if (DEPCMD::Get(ep_addr).FromValue(val).CMDTYP() == DEPCMD::DEPSTRTXFER) {
        completion.Signal();
      }
    });
    depcmd.SetReadCallback([]() -> uint32_t { return 0; });
  });

  auto result = sync_client->QueueRequests({std::move(requests)});
  ASSERT_TRUE(result.is_ok()) << "QueueRequests failed: " << result.error_value().status_string();

  completion.Wait();

  // Stall endpoint
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, ep_addr);
    Dwc3TestHelper::EpSetStall(drv, uep->ep, true);
  });

  // Verify request is not completed
  EXPECT_FALSE(completed);

  // Clear stall
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, ep_addr);
    Dwc3TestHelper::EpSetStall(drv, uep->ep, false);
  });

  // Verify request is still not completed
  EXPECT_FALSE(completed);

  // Simulate completion event
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, ep_addr);
    dwc3_trb_t* trb = uep->fifo.current_read();
    trb->control &= ~TRB_HWO;
    Dwc3TestHelper::HandleEpTransferCompleteEvent(drv, ep_addr);
  });

  // Wait for request completion
  ASSERT_TRUE(sync_client.HandleOneEvent(event_handler).ok());
  EXPECT_TRUE(completed);
  EXPECT_OK(completion_status);

  sync_client = {};
}

// TODO(b/509735595): Re-enable once the deferred cancel and reset logic production fixes land.
TEST_P(Dwc3EndpointTransferSweepTest, DISABLED_CancelAllDuringActiveTransfer) {
  auto params = GetParam();
  uint8_t ep_addr = params.ep_addr;
  fdescriptor::EndpointType ep_type = params.ep_type;
  uint32_t max_packet_size = params.max_packet_size;

  SetUpAndPowerOnEndpoints();

  auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_endpoint::Endpoint>();
  ASSERT_TRUE(endpoints.is_ok());

  bool completed = false;
  zx_status_t completion_status = ZX_OK;
  TestEndpointEventHandler event_handler(completed, completion_status);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, ep_addr);
    ASSERT_NE(uep, nullptr);
    uep->ep.type = ep_type;
    uep->ep.max_packet_size = static_cast<uint16_t>(max_packet_size);
    uep->ep.enabled = true;
    uep->ep.got_not_ready = true;
    ASSERT_TRUE(Dwc3TestHelper::InitFifo(drv, ep_addr).is_ok());

    auto* dispatcher = Dwc3TestHelper::GetDispatcher(drv);
    uep->server->Connect(dispatcher, std::move(endpoints->server));
  });

  fidl::SyncClient<fuchsia_hardware_usb_endpoint::Endpoint> sync_client{
      std::move(endpoints->client)};

  auto vmo_res = CreateVmoBuffer(sync_client, max_packet_size, max_packet_size);
  auto requests = std::move(vmo_res.requests);

  libsync::Completion completion;
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(ep_addr).addr()];
    depcmd.SetWriteCallback([&](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      if (DEPCMD::Get(ep_addr).FromValue(val).CMDTYP() == DEPCMD::DEPSTRTXFER) {
        completion.Signal();
      }
    });
    depcmd.SetReadCallback([]() -> uint32_t { return 0; });
  });

  auto result = sync_client->QueueRequests({std::move(requests)});
  ASSERT_TRUE(result.is_ok()) << "QueueRequests failed: " << result.error_value().status_string();

  completion.Wait();

  // Call CancelAll
  auto cancel_result = sync_client->CancelAll();
  ASSERT_TRUE(cancel_result.is_ok());

  // Safe Barrier Fulfillment: Simulate missing hardware interrupt to release deferred command!
  dut_.RunInDriverContext([&](Dwc3& drv) {
    constexpr uint8_t kMockRsrcId = 5;
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, ep_addr, kMockRsrcId);
  });

  if (ep_addr % 2 == 0) {
    dut_.RunInDriverContext(
        [&](Dwc3& drv) { Dwc3TestHelper::HandleEpTransferCompleteEvent(drv, ep_addr); });
  }

  // Wait for request completion
  ASSERT_TRUE(sync_client.HandleOneEvent(event_handler).ok());
  EXPECT_TRUE(completed);
  EXPECT_EQ(ZX_ERR_CANCELED, completion_status);

  sync_client = {};
}

// TODO(b/509735595): Re-enable once the deferred cancel and reset logic production fixes land.
TEST_P(Dwc3EndpointTransferSweepTest, DISABLED_DisableDuringActiveTransfer) {
  auto params = GetParam();
  uint8_t ep_addr = params.ep_addr;
  fdescriptor::EndpointType ep_type = params.ep_type;
  uint32_t max_packet_size = params.max_packet_size;

  SetUpAndPowerOnEndpoints();

  auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_endpoint::Endpoint>();
  ASSERT_TRUE(endpoints.is_ok());

  bool completed = false;
  zx_status_t completion_status = ZX_OK;
  TestEndpointEventHandler event_handler(completed, completion_status);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, ep_addr);
    ASSERT_NE(uep, nullptr);
    uep->ep.type = ep_type;
    uep->ep.max_packet_size = static_cast<uint16_t>(max_packet_size);
    uep->ep.enabled = true;
    uep->ep.got_not_ready = true;
    ASSERT_TRUE(Dwc3TestHelper::InitFifo(drv, ep_addr).is_ok());

    auto* dispatcher = Dwc3TestHelper::GetDispatcher(drv);
    uep->server->Connect(dispatcher, std::move(endpoints->server));
  });

  fidl::SyncClient<fuchsia_hardware_usb_endpoint::Endpoint> sync_client{
      std::move(endpoints->client)};

  auto vmo_res = CreateVmoBuffer(sync_client, max_packet_size, max_packet_size);
  auto requests = std::move(vmo_res.requests);

  libsync::Completion completion;
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(ep_addr).addr()];
    depcmd.SetWriteCallback([&](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      if (DEPCMD::Get(ep_addr).FromValue(val).CMDTYP() == DEPCMD::DEPSTRTXFER) {
        completion.Signal();
      }
    });
    depcmd.SetReadCallback([]() -> uint32_t { return 0; });
  });

  auto result = sync_client->QueueRequests({std::move(requests)});
  ASSERT_TRUE(result.is_ok()) << "QueueRequests failed: " << result.error_value().status_string();

  completion.Wait();

  // Verify DEPENDXFER was NOT sent
  bool dependxfer_sent = false;
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(ep_addr).addr()];
    depcmd.SetWriteCallback([&](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      if (DEPCMD::Get(ep_addr).FromValue(val).CMDTYP() == DEPCMD::DEPENDXFER) {
        dependxfer_sent = true;
      }
    });
  });

  // Call DisableEndpoint (via EpReset in test helper)
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, ep_addr);
    Dwc3TestHelper::EpReset(drv, uep->ep);
  });

  // Verify DEPENDXFER was NOT sent
  EXPECT_FALSE(dependxfer_sent);

  // Explicitly dispatch the asynchronous request completion event from the sync client!
  ASSERT_TRUE(sync_client.HandleOneEvent(event_handler).ok());
  EXPECT_TRUE(completed);
  EXPECT_EQ(ZX_ERR_CANCELED, completion_status);

  sync_client = {};
}

INSTANTIATE_TEST_SUITE_P(
    Dwc3EndpointTransferSweep, Dwc3EndpointTransferSweepTest,
    testing::Values(EndpointTransferSweepParams{.ep_addr = 2,
                                                .ep_type = fdescriptor::EndpointType::kBulk,
                                                .max_packet_size = 512},  // Bulk OUT
                    EndpointTransferSweepParams{.ep_addr = 3,
                                                .ep_type = fdescriptor::EndpointType::kBulk,
                                                .max_packet_size = 512},  // Bulk IN
                    EndpointTransferSweepParams{.ep_addr = 2,
                                                .ep_type = fdescriptor::EndpointType::kInterrupt,
                                                .max_packet_size = 64},  // Interrupt OUT
                    EndpointTransferSweepParams{.ep_addr = 3,
                                                .ep_type = fdescriptor::EndpointType::kInterrupt,
                                                .max_packet_size = 64}  // Interrupt IN
                    ),
    [](const testing::TestParamInfo<Dwc3EndpointTransferSweepTest::ParamType>& info) {
      std::stringstream test_name;
      test_name << info.index << "_"
                << (info.param.ep_type == fdescriptor::EndpointType::kBulk ? "BULK" : "INTERRUPT")
                << "_" << (info.param.ep_addr % 2 == 0 ? "OUT" : "IN");
      return test_name.str();
    });

TEST_P(Dwc3EndpointsTest, ShortPacketTransfer) {
  auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_endpoint::Endpoint>();
  ASSERT_TRUE(endpoints.is_ok());

  bool completed = false;
  zx_status_t completion_status = ZX_OK;
  size_t completed_length = 0;

  TestEndpointEventHandler event_handler(completed, completion_status, &completed_length);

  SetUpAndPowerOnEndpoints();

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    uep->ep.type = fuchsia_hardware_usb_descriptor::EndpointType::kBulk;
    uep->ep.max_packet_size = 512;
    uep->ep.enabled = true;
    uep->ep.got_not_ready = true;
    ASSERT_TRUE(Dwc3TestHelper::InitFifo(drv, 2).is_ok());

    auto* dispatcher = Dwc3TestHelper::GetDispatcher(drv);
    uep->server->Connect(dispatcher, std::move(endpoints->server));
  });

  fidl::SyncClient<fuchsia_hardware_usb_endpoint::Endpoint> sync_client{
      std::move(endpoints->client)};

  auto vmo_res = CreateVmoBuffer(sync_client, 512, 512, 1, false);
  auto requests = std::move(vmo_res.requests);

  libsync::Completion completion;
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(2).addr()];
    depcmd.SetWriteCallback([&](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      if (DEPCMD::Get(2).FromValue(val).CMDTYP() == DEPCMD::DEPSTRTXFER) {
        completion.Signal();
      }
    });
  });

  auto result = sync_client->QueueRequests({std::move(requests)});
  ASSERT_TRUE(result.is_ok());

  completion.Wait();

  // Simulate completion event with short packet (256 bytes)
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    dwc3_trb_t* trb = uep->fifo.current_read();
    trb->control &= ~TRB_HWO;
    // Set remaining size to 256 (so 256 bytes were transferred)
    trb->status = TRB_BUFSIZ(256);
    Dwc3TestHelper::HandleEpTransferCompleteEvent(drv, 2);
  });

  // Wait for request completion
  ASSERT_TRUE(sync_client.HandleOneEvent(event_handler).ok());
  EXPECT_TRUE(completed);
  EXPECT_OK(completion_status);
  EXPECT_EQ(256UL, completed_length);

  sync_client = {};
}

// TODO(b/509735595): Re-enable once the deferred cancel and reset logic production fixes land.
TEST_P(Dwc3EndpointsTest, DISABLED_ZeroLengthTransfer) {
  SetUpAndPowerOnEndpoints();

  auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_endpoint::Endpoint>();
  ASSERT_TRUE(endpoints.is_ok());

  bool completed = false;
  zx_status_t completion_status = ZX_OK;
  TestEndpointEventHandler event_handler(completed, completion_status);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 3);  // Use IN endpoint 3
    ASSERT_NE(uep, nullptr);
    uep->ep.type = fuchsia_hardware_usb_descriptor::EndpointType::kBulk;
    uep->ep.max_packet_size = 512;
    uep->ep.enabled = true;
    uep->ep.got_not_ready = true;
    ASSERT_TRUE(Dwc3TestHelper::InitFifo(drv, 3).is_ok());  // Use IN endpoint 3

    auto* dispatcher = Dwc3TestHelper::GetDispatcher(drv);
    uep->server->Connect(dispatcher, std::move(endpoints->server));
  });

  fidl::SyncClient<fuchsia_hardware_usb_endpoint::Endpoint> sync_client{
      std::move(endpoints->client)};

  auto vmo_res = CreateVmoBuffer(sync_client, 512, 0, 1, false);
  auto requests = std::move(vmo_res.requests);

  libsync::Completion completion;
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(3).addr()];  // Use IN endpoint 3
    depcmd.SetWriteCallback([&](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      if (DEPCMD::Get(3).FromValue(val).CMDTYP() == DEPCMD::DEPSTRTXFER) {
        completion.Signal();
      }
    });
  });

  auto result = sync_client->QueueRequests({std::move(requests)});
  ASSERT_TRUE(result.is_ok());

  completion.Wait();

  // Verify TRB length is 0
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 3);  // Use IN endpoint 3
    dwc3_trb_t* trb = uep->fifo.current_read();
    EXPECT_EQ(0UL, TRB_BUFSIZ(trb->status));
  });

  // Simulate completion event
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 3);  // Use IN endpoint 3
    dwc3_trb_t* trb = uep->fifo.current_read();
    trb->control &= ~TRB_HWO;
    Dwc3TestHelper::HandleEpTransferCompleteEvent(drv, 3);  // Use IN endpoint 3
  });

  // Wait for request completion
  ASSERT_TRUE(sync_client.HandleOneEvent(event_handler).ok());
  EXPECT_TRUE(completed);
  EXPECT_OK(completion_status);

  sync_client = {};
}

// TODO(b/509735595): Re-enable once the deferred cancel and reset logic production fixes land.
TEST_P(Dwc3EndpointsTest, DISABLED_VerifySiliconBufferingDuringHandshakeReset) {
  SetUpAndPowerOnEndpoints();

  auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_endpoint::Endpoint>();
  ASSERT_TRUE(endpoints.is_ok());

  bool completed = false;
  zx_status_t completion_status = ZX_OK;
  TestEndpointEventHandler event_handler(completed, completion_status);

  // 1. Initialize endpoint server, but keep it disabled!
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    uep->ep.type = fuchsia_hardware_usb_descriptor::EndpointType::kBulk;
    uep->ep.max_packet_size = 512;

    // Explicitly disabled!
    uep->ep.enabled = false;
    uep->ep.got_not_ready = true;

    ASSERT_TRUE(Dwc3TestHelper::InitFifo(drv, 2).is_ok());

    auto* dispatcher = Dwc3TestHelper::GetDispatcher(drv);
    uep->server->Connect(dispatcher, std::move(endpoints->server));
  });

  fidl::SyncClient<fuchsia_hardware_usb_endpoint::Endpoint> sync_client{
      std::move(endpoints->client)};

  auto vmo_res = CreateVmoBuffer(sync_client, 512, 512, 1, false);
  auto requests = std::move(vmo_res.requests);

  // 4. Queue request while endpoint is disabled!
  auto result = sync_client->QueueRequests({std::move(requests)});
  ASSERT_TRUE(result.is_ok()) << "QueueRequests failed: " << result.error_value().status_string();

  dut_.runtime().RunUntilIdle();

  // 5. VERIFY SILICON BUFFERING:
  // - Assert that the request has not completed (completed is false)
  // - Assert that Dwc3 has safely buffered the request in uep->server->queued_reqs!
  EXPECT_FALSE(completed);

  dut_.runtime().RunUntil([&]() {
    size_t size = 0;
    dut_.RunInDriverContext([&](Dwc3& drv) { size = Dwc3TestHelper::GetQueuedReqsSize(drv, 2); });
    return size == 1;
  });

  size_t queued_size = 0;
  dut_.RunInDriverContext(
      [&](Dwc3& drv) { queued_size = Dwc3TestHelper::GetQueuedReqsSize(drv, 2); });
  EXPECT_EQ(1UL, queued_size) << "Request was not buffered in queued_reqs!";

  // 6. Simulate Host enabling the endpoint (e.g. SET_CONFIGURATION complete)
  // - Enable it in the driver and call UserEpQueueNext()
  libsync::Completion completion;
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(2).addr()];
    depcmd.SetWriteCallback([&](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      if (DEPCMD::Get(2).FromValue(val).CMDTYP() ==
          DEPCMD::DEPSTRTXFER) {  // DEPSTRTXFER (DMA starts)
        completion.Signal();
      }
    });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    // Configure and enable! This natively and automatically drains the software queue!
    Dwc3TestHelper::EpSetConfig(drv, uep->ep, true);
  });

  // 7. VERIFY RESUMPTION:
  // - Assert that DMA successfully starts on the buffered request!
  completion.Wait();

  // Simulate completion event
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    dwc3_trb_t* trb = uep->fifo.current_read();
    trb->control &= ~TRB_HWO;
    Dwc3TestHelper::HandleEpTransferCompleteEvent(drv, 2);
  });

  // Wait for completion event to bubble up to client
  ASSERT_TRUE(sync_client.HandleOneEvent(event_handler).ok());
  EXPECT_TRUE(completed);
  EXPECT_OK(completion_status);

  sync_client = {};
}

// DeferredCancelDisableAccountingLeak requires deferred cancel logic which is not in production
// yet.
// TODO(b/509735595): Re-enable once the deferred cancel and reset logic production fixes land.
TEST_P(Dwc3EndpointsTest, DISABLED_DeferredCancelDisableAccountingLeak) {
  SetUpAndPowerOnEndpoints();

  auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_usb_endpoint::Endpoint>();
  ASSERT_TRUE(endpoints.is_ok());

  bool completed = false;
  zx_status_t completion_status = ZX_OK;
  TestEndpointEventHandler event_handler(completed, completion_status);

  // 1. Initialize and configure a mock Bulk Endpoint 7
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 7);
    ASSERT_NE(uep, nullptr);
    uep->ep.type = fuchsia_hardware_usb_descriptor::EndpointType::kBulk;
    uep->ep.max_packet_size = 512;
    uep->ep.enabled = true;
    uep->ep.got_not_ready = true;
    ASSERT_TRUE(Dwc3TestHelper::InitFifo(drv, 7).is_ok());

    auto* dispatcher = Dwc3TestHelper::GetDispatcher(drv);
    uep->server->Connect(dispatcher, std::move(endpoints->server));
  });

  fidl::SyncClient<fuchsia_hardware_usb_endpoint::Endpoint> sync_client{
      std::move(endpoints->client)};

  auto vmo_res = CreateVmoBuffer(sync_client, 512, 512, 1, false);
  auto requests = std::move(vmo_res.requests);

  libsync::Completion completion;
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(7).addr()];
    depcmd.SetWriteCallback([&](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      if (DEPCMD::Get(7).FromValue(val).CMDTYP() == DEPCMD::DEPSTRTXFER) {
        completion.Signal();
      }
    });
    depcmd.SetReadCallback([]() -> uint32_t { return 0; });
  });

  auto result = sync_client->QueueRequests({std::move(requests)});
  ASSERT_TRUE(result.is_ok());

  completion.Wait();

  // 2. Force an EndTransfer operation with a simulated busy hardware return to mark
  // ep->pending_cancel = true. This is done by calling CancelAll before
  // HandleEpTransferStartedEvent, which leaves ep.rsrc_id at kInvalidResourceId.
  auto cancel_result = sync_client->CancelAll();
  ASSERT_TRUE(cancel_result.is_ok());

  // 3. Forcefully execute the endpoint clear/disable track (EpEnable(ep, false))
  // which completes the request with ZX_ERR_CANCELED and wipes the tracking pointer.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 7);
    Dwc3TestHelper::EpEnable(drv, uep->ep, false);
  });

  // Flush completion events to event handler so we are 100% in sync
  ASSERT_TRUE(sync_client.HandleOneEvent(event_handler).ok());
  EXPECT_TRUE(completed);
  EXPECT_EQ(ZX_ERR_CANCELED, completion_status);

  // 4. Fire a mock trailing edge hardware event interrupt (DEPEVT_XFER_COMPLETE) against
  // Endpoint 7.
  dut_.RunInDriverContext(
      [&](Dwc3& drv) { Dwc3TestHelper::HandleEpTransferCompleteEvent(drv, 7); });

  sync_client = {};
}

TEST_P(Dwc3EndpointsTest, CancelAllRequestsOnUnbound) {
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  const bool enqueue_many = GetParam();
  const TransferState starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  const TransferState active_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue a request.
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  WaitForActiveCount(ep_num, 1);
  WaitForState(ep_num, starting_state);

  // Trigger started event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, active_state);

  // Cancel all requests via client asynchronously.
  bool cancel_replied = false;
  ep_client_->CancelAll().Then(
      [&](fidl::Result<fendpoint::Endpoint::CancelAll>& res) { cancel_replied = true; });

  WaitForState(ep_num, TransferState::kCanceling);
  EXPECT_FALSE(cancel_replied);

  // Verify that cancel_completers has 1 pending completer in the server.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    ASSERT_TRUE(uep.server.has_value());
    EXPECT_EQ(uep.server->cancel_completers.size(), 1u);
  });

  // Close the client endpoint channel to unbind the endpoint server.
  ep_client_ = {};

  // Wait for the driver dispatcher to process OnUnbound and flush completers.
  dut_.runtime().RunUntil([&]() {
    dut_.runtime().RunUntilIdle();
    return dut_.RunInDriverContext<bool>([&](Dwc3& drv) {
      auto& uep = GetUserEndpoint(drv, ep_num);
      return uep.server.has_value() && uep.server->cancel_completers.empty();
    });
  });

  // Complete the hardware transfer.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferEnded(drv, ep_num); });
  WaitForState(ep_num, TransferState::kIdle);
}

TEST_P(Dwc3EndpointsTest, CancelAllTransferEndedBeforeUnbound) {
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  const bool enqueue_many = GetParam();
  const TransferState starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  const TransferState active_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue a request.
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  WaitForActiveCount(ep_num, 1);
  WaitForState(ep_num, starting_state);

  // Trigger started event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, active_state);

  // Cancel all requests via client asynchronously.
  bool cancel_replied = false;
  ep_client_->CancelAll().Then(
      [&](fidl::Result<fendpoint::Endpoint::CancelAll>& res) { cancel_replied = true; });

  WaitForState(ep_num, TransferState::kCanceling);
  EXPECT_FALSE(cancel_replied);

  // Verify that cancel_completers has 1 pending completer in the server.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    ASSERT_TRUE(uep.server.has_value());
    EXPECT_EQ(uep.server->cancel_completers.size(), 1u);
  });

  // Close the client endpoint channel to initiate unbind.
  ep_client_ = {};

  // Fire HandleEpTransferEndedEvent BEFORE driver dispatcher processes OnUnbound.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferEnded(drv, ep_num); });

  // Now process queued events on driver dispatcher including OnUnbound.
  dut_.runtime().RunUntilIdle();

  // Verify state is cleanly Idle and completers vector is empty.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    ASSERT_TRUE(uep.server.has_value());
    EXPECT_EQ(uep.server->cancel_completers.size(), 0u);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kIdle);
  });
}

// Tests that completing a single-transfer request via TransferInProgress correctly
// returns the endpoint state machine to kIdle and starts the next queued request.
// DISABLED: Requires driver support for single-transfer state reset and next request dispatch
// upon TransferInProgress.
TEST_P(Dwc3EndpointsTest, DISABLED_SingleTransferInProgress_CompletesAndStartsNext) {
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kInterrupt, 64);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 64, fdescriptor::EndpointType::kInterrupt);
  QueueRequest(1, 64, 64, fdescriptor::EndpointType::kInterrupt);

  WaitForState(ep_num, TransferState::kStartingSingle);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, TransferState::kActiveSingle);

  // Complete request 1 via TransferInProgress.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferInProgress(drv, ep_num); });

  // Verify that request 2 is dequeued and begins starting.
  WaitForState(ep_num, TransferState::kStartingSingle);
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 1u);
    EXPECT_EQ(uep.server->active_reqs.size(), 1u);
    EXPECT_EQ(uep.server->queued_reqs.size(), 0u);
  });

  // Start and complete request 2 via TransferInProgress.
  dut_.RunInDriverContext(
      [&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId + 1); });
  WaitForState(ep_num, TransferState::kActiveSingle);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferInProgress(drv, ep_num); });

  WaitForState(ep_num, TransferState::kIdle);
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(2);
  EXPECT_EQ(completions.size(), 2u);
}

// Tests that when the active queue drains to 0 via TransferInProgress, the endpoint
// returns to kIdle and subsequent calls to QueueRequests start transfers cleanly.
// DISABLED: Requires driver support for single-transfer idle state recovery upon active queue
// exhaustion via TransferInProgress.
TEST_P(Dwc3EndpointsTest, DISABLED_SingleTransferInProgress_ActiveQueueDrainThenRequeue) {
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kInterrupt, 64);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 64, fdescriptor::EndpointType::kInterrupt);
  WaitForState(ep_num, TransferState::kStartingSingle);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, TransferState::kActiveSingle);

  // Complete request 1 via TransferInProgress so active count drains to 0.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferInProgress(drv, ep_num); });

  // State should return to kIdle once active requests drain.
  WaitForState(ep_num, TransferState::kIdle);
  std::vector<CompletionResult> completions1 = event_handler_.WaitForCompletions(1);
  EXPECT_EQ(completions1.size(), 1u);

  // Later, client queues Request 2.
  QueueRequest(1, 64, 64, fdescriptor::EndpointType::kInterrupt);
  WaitForState(ep_num, TransferState::kStartingSingle);
  dut_.RunInDriverContext(
      [&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId + 1); });
  WaitForState(ep_num, TransferState::kActiveSingle);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferComplete(drv, ep_num); });

  WaitForState(ep_num, TransferState::kIdle);
  std::vector<CompletionResult> completions2 = event_handler_.WaitForCompletions(1);
  EXPECT_EQ(completions2.size(), 1u);
}

// Tests that a 64-byte transfer on a 64-byte max-packet Interrupt IN endpoint with short_bit set
// correctly enqueues and completes 2 TRBs (data TRB + ZLP TRB).
TEST_P(Dwc3EndpointsTest, InterruptIn_ZlpTwoTrbCompletion) {
  TriggerConnection();

  const uint8_t ep_address = 0x82;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kInterrupt, 64);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue a 64-byte IN request with short_bit = true.
  QueueRequest(1, 0, 64, fdescriptor::EndpointType::kInterrupt, /*short_bit=*/true);
  WaitForState(ep_num, TransferState::kStartingSingle);

  // Verify that 2 TRBs (data TRB + ZLP TRB) were queued in the FIFO.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 2u);
    ASSERT_EQ(uep.server->active_reqs.size(), 1u);
    EXPECT_EQ(uep.server->active_reqs.front().total_trbs, 2u);
  });

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, TransferState::kActiveSingle);

  // Complete data TRB. Request should not complete yet because ZLP TRB is pending.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferComplete(drv, ep_num); });
  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(event_handler_.completion_count(), 0u);

  // Complete ZLP TRB.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferComplete(drv, ep_num); });
  WaitForState(ep_num, TransferState::kIdle);

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1u);
  EXPECT_EQ(completions[0].status, ZX_OK);
  EXPECT_EQ(completions[0].transfer_size, 64u);
}

// Tests rapid sequential queuing of small notification packets on Interrupt IN (CDC notification
// style).
TEST_P(Dwc3EndpointsTest, InterruptIn_RapidNotificationBurst) {
  TriggerConnection();

  const uint8_t ep_address = 0x82;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kInterrupt, 64);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  constexpr size_t kBurstCount = 6;
  for (size_t i = 0; i < kBurstCount; i++) {
    QueueRequest(1, i * 8, 8, fdescriptor::EndpointType::kInterrupt);
  }

  for (size_t i = 0; i < kBurstCount; i++) {
    WaitForState(ep_num, TransferState::kStartingSingle);
    dut_.RunInDriverContext([&](Dwc3& drv) {
      TriggerEpTransferStarted(drv, ep_num, kResourceId + static_cast<uint32_t>(i));
    });
    WaitForState(ep_num, TransferState::kActiveSingle);
    dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferComplete(drv, ep_num); });
  }

  WaitForState(ep_num, TransferState::kIdle);
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(kBurstCount);
  EXPECT_EQ(completions.size(), kBurstCount);
}

// Tests CancelAll while an Interrupt transfer is active, verifying that queued requests
// cancel immediately, active request aborts via DEPENDXFER, and state returns to kIdle.
TEST_P(Dwc3EndpointsTest, Interrupt_CancelAllWhileTransferActive) {
  TriggerConnection();

  const uint8_t ep_address = 0x82;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kInterrupt, 64);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 16, fdescriptor::EndpointType::kInterrupt);
  QueueRequest(1, 16, 16, fdescriptor::EndpointType::kInterrupt);
  QueueRequest(1, 32, 16, fdescriptor::EndpointType::kInterrupt);

  WaitForState(ep_num, TransferState::kStartingSingle);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, TransferState::kActiveSingle);

  bool cancel_replied = false;
  ep_client_->CancelAll().Then(
      [&](fidl::Result<fendpoint::Endpoint::CancelAll>& res) { cancel_replied = true; });

  // Unstarted requests 2 & 3 should be completed immediately with IO_NOT_PRESENT.
  std::vector<CompletionResult> completions1 = event_handler_.WaitForCompletions(2);
  EXPECT_EQ(completions1.size(), 2u);
  EXPECT_FALSE(cancel_replied);

  WaitForState(ep_num, TransferState::kCanceling);

  // Trigger TransferEnded event for active Request 1.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferEnded(drv, ep_num); });

  dut_.runtime().RunUntil([&]() { return cancel_replied; });
  EXPECT_TRUE(cancel_replied);
  std::vector<CompletionResult> completions2 = event_handler_.WaitForCompletions(1);
  EXPECT_EQ(completions2.size(), 1u);
  WaitForState(ep_num, TransferState::kIdle);
}

// Tests CancelAll while an Interrupt transfer is in kStartingSingle (before TransferStarted).
TEST_P(Dwc3EndpointsTest, Interrupt_CancelAllWhileTransferStarting) {
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kInterrupt, 64);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 64, fdescriptor::EndpointType::kInterrupt);
  QueueRequest(1, 64, 64, fdescriptor::EndpointType::kInterrupt);

  WaitForState(ep_num, TransferState::kStartingSingle);

  bool cancel_replied = false;
  ep_client_->CancelAll().Then(
      [&](fidl::Result<fendpoint::Endpoint::CancelAll>& res) { cancel_replied = true; });

  std::vector<CompletionResult> completions1 = event_handler_.WaitForCompletions(1);
  EXPECT_EQ(completions1.size(), 1u);
  WaitForState(ep_num, TransferState::kPendingCancel);
  EXPECT_FALSE(cancel_replied);

  // When TransferStarted arrives, it should transition to kCanceling and issue EndTransfer.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, TransferState::kCanceling);

  // TransferEnded completes the cancellation.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferEnded(drv, ep_num); });
  dut_.runtime().RunUntil([&]() { return cancel_replied; });
  EXPECT_TRUE(cancel_replied);
  std::vector<CompletionResult> completions2 = event_handler_.WaitForCompletions(1);
  EXPECT_EQ(completions2.size(), 1u);
  WaitForState(ep_num, TransferState::kIdle);
}

// Tests setting stall on an Interrupt endpoint, verifying that queued requests stall,
// and clearing stall cleanly resumes execution.
TEST_P(Dwc3EndpointsTest, Interrupt_StallAndClearHaltResumesQueue) {
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kInterrupt, 64);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Request 1 executes and completes.
  QueueRequest(1, 0, 64, fdescriptor::EndpointType::kInterrupt);
  WaitForState(ep_num, TransferState::kStartingSingle);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, TransferState::kActiveSingle);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferComplete(drv, ep_num); });
  WaitForState(ep_num, TransferState::kIdle);
  std::vector<CompletionResult> completions1 = event_handler_.WaitForCompletions(1);
  EXPECT_EQ(completions1.size(), 1u);

  // Stall endpoint via DCI.
  fidl::WireResult stall_res = dci_->EndpointSetStall(ep_address);
  ASSERT_OK(stall_res.status());
  ASSERT_TRUE(stall_res.value().is_ok());

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_TRUE(uep.ep.stalled);
  });

  // Queue Request 2 while stalled; it should remain in queued_reqs.
  QueueRequest(1, 64, 64, fdescriptor::EndpointType::kInterrupt);
  dut_.runtime().RunUntil([&]() {
    return dut_.RunInDriverContext<bool>(
        [&](Dwc3& drv) { return GetUserEndpoint(drv, ep_num).server->queued_reqs.size() == 1u; });
  });
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kIdle);
  });

  // Clear stall; Request 2 should immediately start.
  fidl::WireResult clear_res = dci_->EndpointClearStall(ep_address);
  ASSERT_OK(clear_res.status());
  ASSERT_TRUE(clear_res.value().is_ok());

  WaitForState(ep_num, TransferState::kStartingSingle);
  dut_.RunInDriverContext(
      [&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId + 1); });
  WaitForState(ep_num, TransferState::kActiveSingle);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferComplete(drv, ep_num); });

  WaitForState(ep_num, TransferState::kIdle);
  std::vector<CompletionResult> completions2 = event_handler_.WaitForCompletions(1);
  EXPECT_EQ(completions2.size(), 1u);
}

// Tests handling repeated TransferNotReady events during host polling without dropping requests.
TEST_P(Dwc3EndpointsTest, Interrupt_TransferNotReadyLoop) {
  TriggerConnection();

  const uint8_t ep_address = 0x82;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kInterrupt, 64);
  RegisterVmo(1, 4096);

  // Host polls while endpoint is empty (fires NotReady multiple times).
  for (int i = 0; i < 3; i++) {
    dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  }

  // Queue Request 1 and complete it.
  QueueRequest(1, 0, 16, fdescriptor::EndpointType::kInterrupt);
  WaitForState(ep_num, TransferState::kStartingSingle);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, TransferState::kActiveSingle);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferComplete(drv, ep_num); });
  WaitForState(ep_num, TransferState::kIdle);

  // Host polls again.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });

  // Queue Request 2 and complete it.
  QueueRequest(1, 16, 16, fdescriptor::EndpointType::kInterrupt);
  WaitForState(ep_num, TransferState::kStartingSingle);
  dut_.RunInDriverContext(
      [&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId + 1); });
  WaitForState(ep_num, TransferState::kActiveSingle);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferComplete(drv, ep_num); });
  WaitForState(ep_num, TransferState::kIdle);

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(2);
  EXPECT_EQ(completions.size(), 2u);
}

// Tests short packet reception on Interrupt OUT endpoint.
TEST_P(Dwc3EndpointsTest, InterruptOut_ShortPacketHandling) {
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kInterrupt, 64);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 64, fdescriptor::EndpointType::kInterrupt);
  WaitForState(ep_num, TransferState::kStartingSingle);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, TransferState::kActiveSingle);

  // Mock hardware receiving a short packet: 16 bytes transferred out of 64 (residual 48 bytes).
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferComplete(drv, ep_num, 48); });

  WaitForState(ep_num, TransferState::kIdle);
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1u);
  EXPECT_EQ(completions[0].status, ZX_OK);
  EXPECT_EQ(completions[0].transfer_size, 16u);
}

// Tests disabling an active Interrupt endpoint (simulating alternate setting switch).
TEST_P(Dwc3EndpointsTest, Interrupt_ReconfigureAltSettingTeardown) {
  TriggerConnection();

  const uint8_t ep_address = 0x82;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kInterrupt, 64);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 16, fdescriptor::EndpointType::kInterrupt);
  WaitForState(ep_num, TransferState::kStartingSingle);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, TransferState::kActiveSingle);

  // Disable endpoint (simulating alternate setting switch).
  fidl::WireResult disable_res = dci_->DisableEndpoint(ep_address);
  ASSERT_OK(disable_res.status());
  ASSERT_TRUE(disable_res.value().is_ok());

  WaitForState(ep_num, TransferState::kCanceling);

  // Trigger TransferEnded event to simulate hardware completion of EndTransfer.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferEnded(drv, ep_num); });

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1u);
  EXPECT_EQ(completions[0].status, ZX_ERR_IO_NOT_PRESENT);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_FALSE(uep.ep.enabled);
  });
}

}  // namespace dwc3
