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
    EXPECT_EQ(result->vmos.size(), 1UL);
    EXPECT_EQ(result->vmos[0].id(), vmo_id);
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
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kIdle);
    EXPECT_FALSE(uep.ep.got_not_ready);
  });

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_TRUE(uep.ep.got_not_ready);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kIdle);
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
    EXPECT_EQ(uep.fifo.GetActiveCount(), 1u);
  });

  // Host sends Transfer Complete event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferComplete(drv, ep_num); });
  WaitForState(ep_num, TransferState::kIdle);

  // State should be back to kIdle.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 0u);
  });

  // Verify completion is received.
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1UL);
  EXPECT_EQ(completions[0].status, ZX_OK);
  EXPECT_EQ(completions[0].transfer_size, 64UL);
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
    EXPECT_EQ(uep.fifo.GetActiveCount(), 1u);
  });

  // Queue second request.
  QueueRequest(1, 512, 512, fdescriptor::EndpointType::kBulk);
  WaitForQueuedCount(ep_num, 1u);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.ep.transfer_state, expected_starting_state);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 1u);
    EXPECT_EQ(uep.server->queued_reqs.size(), 1u);
    EXPECT_EQ(uep.server->active_reqs.size(), 1u);
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
      EXPECT_EQ(uep.ep.rsrc_id, kResourceId);
      EXPECT_EQ(uep.fifo.GetActiveCount(), 2u);
      EXPECT_EQ(uep.server->queued_reqs.size(), 0u);
    });

    // Complete request 1.
    dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferInProgress(drv, ep_num); });
    WaitForActiveCount(ep_num, 1u);

    dut_.RunInDriverContext([&](Dwc3& drv) {
      auto& uep = GetUserEndpoint(drv, ep_num);
      EXPECT_EQ(uep.ep.transfer_state, TransferState::kActiveOngoing);
      EXPECT_EQ(uep.fifo.GetActiveCount(), 1u);
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
      EXPECT_EQ(uep.fifo.GetActiveCount(), 1u);
      EXPECT_EQ(uep.server->queued_reqs.size(), 0u);
      EXPECT_EQ(uep.server->active_reqs.size(), 1u);
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
    EXPECT_EQ(uep.ep.transfer_state, expected_final_state);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 0u);
    EXPECT_EQ(uep.server->active_reqs.size(), 0u);
  });

  // Verify completions.
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(2);
  ASSERT_EQ(completions.size(), 2UL);
  EXPECT_EQ(completions[0].status, ZX_OK);
  EXPECT_EQ(completions[0].transfer_size, 512UL);
  EXPECT_EQ(completions[1].status, ZX_OK);
  EXPECT_EQ(completions[1].transfer_size, 512UL);
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
    EXPECT_EQ(uep.ep.transfer_state, expected_state);
    if (enqueue_many) {
      EXPECT_EQ(uep.server->active_reqs.size(), 2u);
      EXPECT_EQ(uep.server->queued_reqs.size(), 0u);
    } else {
      EXPECT_EQ(uep.server->active_reqs.size(), 1u);
      EXPECT_EQ(uep.server->queued_reqs.size(), 1u);
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
    EXPECT_EQ(uep.server->active_reqs.size(), enqueue_many ? 2u : 1u);
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
    EXPECT_EQ(uep.server->active_reqs.size(), 0u);
  });

  // Verify completions returned with cancellation error.
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(2);
  ASSERT_EQ(completions.size(), 2UL);
  EXPECT_EQ(completions[0].status, ZX_ERR_IO_NOT_PRESENT);
  EXPECT_EQ(completions[1].status, ZX_ERR_IO_NOT_PRESENT);
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
    EXPECT_EQ(uep.ep.transfer_state, expected_state);
    if (enqueue_many) {
      EXPECT_EQ(uep.server->active_reqs.size(), 2u);
      EXPECT_EQ(uep.server->queued_reqs.size(), 0u);
    } else {
      EXPECT_EQ(uep.server->active_reqs.size(), 1u);
      EXPECT_EQ(uep.server->queued_reqs.size(), 1u);
    }
  });

  // Stop controller.
  fidl::WireResult res = dci_->StopController();
  ASSERT_OK(res.status());
  WaitForState(ep_num, TransferState::kIdle);

  // Now, active_reqs and queued_reqs should be empty.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.server->active_reqs.size(), 0u);
    EXPECT_EQ(uep.server->queued_reqs.size(), 0u);
  });

  // Verify completions returned with cancellation error.
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(2);
  ASSERT_EQ(completions.size(), 2UL);
  EXPECT_EQ(completions[0].status, ZX_ERR_IO_NOT_PRESENT);
  EXPECT_EQ(completions[1].status, ZX_ERR_IO_NOT_PRESENT);
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
    EXPECT_EQ(uep.fifo.GetActiveCount(), 2u);
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
    EXPECT_EQ(uep.fifo.GetActiveCount(), 1u);
  });

  dut_.runtime().RunUntilIdle();
  // Verify that no completions are received.
  EXPECT_EQ(event_handler_.completion_count(), 0u);

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
  EXPECT_EQ(completions[0].status, ZX_OK);
  EXPECT_EQ(completions[0].transfer_size, 512UL);
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
  EXPECT_EQ(DEPCMD::Get(2).FromValue(last_depcmd).CMDTYP(), DEPCMD::DEPSSTALL);

  write_called = false;
  last_depcmd = 0;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    EXPECT_TRUE(uep->ep.enabled);
    Dwc3TestHelper::EpSetStall(drv, uep->ep, false);
  });

  EXPECT_TRUE(write_called);
  EXPECT_EQ(DEPCMD::Get(2).FromValue(last_depcmd).CMDTYP(), DEPCMD::DEPCSTALL);
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
  EXPECT_EQ(dalepena_val & (1 << 2), 0u);
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

  EXPECT_EQ(ZX_OK, WaitForPhy());

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
  EXPECT_EQ(completion_status, ZX_OK);

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
  EXPECT_EQ(completion_status, ZX_ERR_CANCELED);

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
  EXPECT_EQ(completion_status, ZX_ERR_CANCELED);

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
  EXPECT_EQ(completion_status, ZX_OK);
  EXPECT_EQ(completed_length, 256UL);

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
    EXPECT_EQ(TRB_BUFSIZ(trb->status), 0UL);
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
  EXPECT_EQ(completion_status, ZX_OK);

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
  EXPECT_EQ(queued_size, 1UL) << "Request was not buffered in queued_reqs!";

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
  EXPECT_EQ(completion_status, ZX_OK);

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
  EXPECT_EQ(completion_status, ZX_ERR_CANCELED);

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

}  // namespace dwc3
