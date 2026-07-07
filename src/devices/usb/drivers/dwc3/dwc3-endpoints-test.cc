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
  void TriggerConnection(fdescriptor::UsbSpeed speed = fdescriptor::UsbSpeed::kSuper) {
    TriggerConnectionPlugIn(speed);

    auto dci_service = dut_.Connect<fdci::UsbDciService::Device>();
    ASSERT_TRUE(dci_service.is_ok())
        << "Failed to connect to UsbDciService: " << dci_service.status_string();
    dci_.Bind(std::move(*dci_service));
  }

  void SetupEndpoint(uint8_t ep_address, uint8_t ep_type, uint16_t max_packet_size) {
    fdescriptor::wire::UsbEndpointDescriptor ep_desc{
        .b_length = sizeof(fdescriptor::wire::UsbEndpointDescriptor),
        .b_descriptor_type = USB_DT_ENDPOINT,
        .b_endpoint_address = ep_address,
        .bm_attributes = ep_type,
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

  void QueueRequest(uint8_t vmo_id, uint64_t offset, uint64_t size, uint8_t ep_type,
                    bool short_bit = false) {
    frequest::Buffer buffer = frequest::Buffer::WithVmoId(vmo_id);

    frequest::BufferRegion region;
    region.buffer(std::move(buffer));
    region.offset(offset);
    region.size(size);

    std::vector<frequest::BufferRegion> regions;
    regions.push_back(std::move(region));

    frequest::RequestInfo req_info =
        (ep_type == USB_ENDPOINT_BULK)
            ? frequest::RequestInfo::WithBulk(frequest::BulkRequestInfo{})
            : frequest::RequestInfo::WithInterrupt(frequest::InterruptRequestInfo{});

    frequest::Request req;
    req.data(std::move(regions));
    req.defer_completion(false);
    req.information(std::move(req_info));
    req.short_(short_bit);

    std::vector<frequest::Request> reqs;
    reqs.push_back(std::move(req));

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

    std::vector<CompletionResult> WaitForCompletions(
        size_t count, zx::duration timeout = zx::duration::infinite()) {
      std::unique_lock<std::mutex> lock(mutex_);
      if (timeout == zx::duration::infinite()) {
        completion_cond_.wait(lock, [&]() { return completions_.size() >= count; });
      } else {
        completion_cond_.wait_for(lock, std::chrono::nanoseconds(timeout.to_nsecs()),
                                  [&]() { return completions_.size() >= count; });
      }

      size_t take = std::min(count, completions_.size());
      std::vector<CompletionResult> res(std::make_move_iterator(completions_.begin()),
                                        std::make_move_iterator(completions_.begin() + take));
      completions_.erase(completions_.begin(), completions_.begin() + take);
      return res;
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
  SetupEndpoint(ep_address, USB_ENDPOINT_INTERRUPT, 64);
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
  QueueRequest(1, 0, 64, USB_ENDPOINT_INTERRUPT);
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

  SetupEndpoint(ep_address, USB_ENDPOINT_BULK, 512);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue first request.
  QueueRequest(1, 0, 512, USB_ENDPOINT_BULK);
  auto expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 1u);
  });

  // Queue second request.
  QueueRequest(1, 512, 512, USB_ENDPOINT_BULK);
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

  SetupEndpoint(ep_address, USB_ENDPOINT_BULK, 512);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue two requests.
  QueueRequest(1, 0, 512, USB_ENDPOINT_BULK);
  QueueRequest(1, 512, 512, USB_ENDPOINT_BULK);
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

  // Cancel all requests via client.
  fidl::WireResult result = ep_client_.wire_sync()->CancelAll();
  ASSERT_OK(result.status());
  ASSERT_TRUE(result->is_ok()) << zx_status_get_string(result->error_value());
  WaitForState(ep_num, TransferState::kCanceling);

  // The state should be kCanceling, and active_reqs should not be empty yet.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.server->active_reqs.size(), enqueue_many ? 2u : 1u);
  });

  // Hardware emits Command Complete (End Transfer) to acknowledge End Transfer.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferEnded(drv, ep_num); });
  WaitForState(ep_num, TransferState::kIdle);

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

TEST_P(Dwc3EndpointsTest, InputEndpointZlpComplete) {
  TriggerConnection();

  // 0x82 is an INPUT (IN) endpoint.
  const uint8_t ep_address = 0x82;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  // Configure endpoint as Bulk IN with max packet size 512.
  SetupEndpoint(ep_address, USB_ENDPOINT_BULK, 512);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue a request with short_bit = true, size = 512 (multiple of max packet
  // size).
  QueueRequest(1, 0, 512, USB_ENDPOINT_BULK, /*short_bit=*/true);

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

  // Verify that no completions are received.
  std::vector<CompletionResult> pending_completions =
      event_handler_.WaitForCompletions(1, zx::msec(50));
  EXPECT_TRUE(pending_completions.empty());

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

}  // namespace dwc3
