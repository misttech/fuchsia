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
  EXPECT_EQ(trbs[0].control, expected);

  // Ongoing transfers do not use TRB_LST
  uint32_t expected_control = TRB_TRBCTL_NORMAL | TRB_IOC | TRB_HWO;
  if (!enqueue_many) {
    expected_control |= TRB_LST;
  }
  EXPECT_EQ(trbs[1].control, expected_control);
  EXPECT_EQ(TRB_BUFSIZ(trbs[1].status), 0u);
}
}  // namespace

// Non-parameterized base fixture for DWC3 endpoint tests.
class Dwc3EndpointsTestBase : public TestFixture<true> {
 public:
  static constexpr uint32_t kResourceId = 12;

  void SetUp() override {
    TestFixture::SetUp();
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
    ASSERT_OK(client_loop_.StartThread("client-loop"));
  }

  void TearDown() override {
    ep_client_ = {};
    dci_ = {};
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
    ASSERT_NO_FATAL_FAILURE(ConfigureEndpoint(ep_address, ep_type, max_packet_size));

    zx::result endpoints = fidl::CreateEndpoints<fendpoint::Endpoint>();
    ASSERT_OK(endpoints);
    auto [client_end, server_end] = std::move(*endpoints);

    fidl::WireResult conn_res = dci_->ConnectToEndpoint(ep_address, std::move(server_end));
    ASSERT_OK(conn_res.status());
    ASSERT_TRUE(conn_res.value().is_ok()) << "ConnectToEndpoint protocol failed: "
                                          << zx_status_get_string(conn_res.value().error_value());

    ep_client_.Bind(std::move(client_end), client_loop_.dispatcher(), &event_handler_);
  }

  // Configures (or, after a bus reset disabled it, re-configures) the endpoint over the DCI
  // protocol without touching the Endpoint channel bound by SetupEndpoint().
  void ConfigureEndpoint(uint8_t ep_address, fdescriptor::EndpointType ep_type,
                         uint16_t max_packet_size) {
    fdescriptor::wire::UsbEndpointDescriptor ep_desc{
        .b_length = sizeof(fdescriptor::wire::UsbEndpointDescriptor),
        .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
        .b_endpoint_address = ep_address,
        .bm_attributes = static_cast<uint8_t>(ep_type),
        .w_max_packet_size = max_packet_size,
        .b_interval = 0,
    };
    fdescriptor::wire::UsbSsEpCompDescriptor ss_comp_desc{
        .b_length = sizeof(fdescriptor::wire::UsbSsEpCompDescriptor),
        .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kSsEpCompanion),
        .b_max_burst = 0,
        .bm_attributes = 0,
        .w_bytes_per_interval = 0,
    };

    fidl::WireResult config_res = dci_->ConfigureEndpoint(ep_desc, ss_comp_desc);
    ASSERT_OK(config_res.status());
    ASSERT_TRUE(config_res.value().is_ok())
        << "ConfigureEndpoint protocol failed: "
        << zx_status_get_string(config_res.value().error_value());
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
    reqs.reserve(count);
    for (size_t i = 0; i < count; i++) {
      frequest::BufferRegion region;
      region.buffer(frequest::Buffer::WithVmoId(vmo_id));
      region.offset(offset + (size * i));
      region.size(size);

      std::vector<frequest::BufferRegion> regions;
      regions.reserve(1);
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

    // QueueRequests is a one-way (fire-and-forget) FIDL method; calling it on fidl::SharedClient
    // synchronously writes to the channel and returns fit::result<fidl::OneWayError>.
    fit::result result = ep_client_->QueueRequests({std::move(reqs)});
    ASSERT_TRUE(result.is_ok()) << "QueueRequests failed: "
                                << result.error_value().FormatDescription();
  }

  void WaitForState(uint8_t ep_num, TransferState expected_state) {
    dut_.runtime().RunUntil([&]() {
      return dut_.RunInDriverContext<bool>([&](Dwc3& drv) {
        return GetUserEndpoint(drv, ep_num).ep.transfer_state == expected_state;
      });
    });
  }

  void WaitForQueuedCount(uint8_t ep_num, size_t count) {
    dut_.runtime().RunUntil([&]() {
      return dut_.RunInDriverContext<bool>([&](Dwc3& drv) {
        return GetUserEndpoint(drv, ep_num).server->queued_reqs.size() == count;
      });
    });
  }

  void WaitForActiveCount(uint8_t ep_num, size_t count) {
    dut_.runtime().RunUntil([&]() {
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
      // Safety watchdog timeout: Under normal test execution, completions arrive
      // near-instantaneously via dispatcher events. 5 seconds provides protection against hanging
      // tests.
      bool success = completion_cond_.wait_for(lock, std::chrono::seconds(5),
                                               [&]() { return completions_.size() >= count; });
      if (!success) {
        ADD_FAILURE() << "WaitForCompletions timed out waiting for " << count << " completions";
        return {};
      }

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

  void SetEnableEnqueueManyTrbs(bool enable) {
    dut_.RunInDriverContext([&](Dwc3& drv) { drv.SetEnableEnqueueManyTrbs(enable); });
  }

  EventHandler event_handler_;
  fidl::SharedClient<fendpoint::Endpoint> ep_client_;
};

// Parameterized fixture over whether enqueueing multiple TRBs is enabled.
class Dwc3EndpointsTest : public Dwc3EndpointsTestBase, public ::testing::WithParamInterface<bool> {
 public:
  void SetUp() override {
    Dwc3EndpointsTestBase::SetUp();
    SetEnableEnqueueManyTrbs(GetParam());
  }
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
  EXPECT_OK(completions[0].status);
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
  EXPECT_OK(completions[0].status);
  EXPECT_EQ(completions[0].transfer_size, 512UL);
  EXPECT_OK(completions[1].status);
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
  auto cancel_result =
      std::make_shared<std::optional<fidl::Result<fendpoint::Endpoint::CancelAll>>>();
  auto cancel_completed = std::make_shared<libsync::Completion>();
  ep_client_->CancelAll().Then(
      [cancel_result, cancel_completed](fidl::Result<fendpoint::Endpoint::CancelAll>& res) {
        *cancel_result = std::move(res);
        cancel_completed->Signal();
      });

  WaitForState(ep_num, TransferState::kCanceling);
  EXPECT_FALSE(cancel_completed->signaled());

  // The state should be kCanceling, and active_reqs should not be empty yet.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.server->active_reqs.size(), enqueue_many ? 2u : 1u);
  });

  // Hardware emits Command Complete (End Transfer) to acknowledge End Transfer.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferEnded(drv, ep_num); });
  WaitForState(ep_num, TransferState::kIdle);

  dut_.runtime().RunUntil([&]() { return cancel_completed->signaled(); });
  ASSERT_TRUE(cancel_completed->signaled());
  ASSERT_TRUE(cancel_result->has_value());
  ASSERT_TRUE((*cancel_result)->is_ok());

  // Now, active_reqs should be empty.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.server->active_reqs.size(), 0u);
  });

  // Verify completions returned with cancellation error.
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(2);
  ASSERT_EQ(completions.size(), 2UL);
  EXPECT_EQ(completions[0].status, ZX_ERR_CANCELED);
  EXPECT_EQ(completions[1].status, ZX_ERR_CANCELED);
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

  // Issue CancelAll while active so that the FIDL completer is queued in cancel_completers.
  auto cancel_result =
      std::make_shared<std::optional<fidl::Result<fendpoint::Endpoint::CancelAll>>>();
  auto cancel_completed = std::make_shared<libsync::Completion>();
  ep_client_->CancelAll().Then(
      [cancel_result, cancel_completed](fidl::Result<fendpoint::Endpoint::CancelAll>& res) {
        *cancel_result = std::move(res);
        cancel_completed->Signal();
      });
  WaitForState(ep_num, TransferState::kCanceling);

  // Stop controller before hardware EndTransfer completes; software fallback must drain requests
  // and reply ZX_OK to pending CancelAll completers.
  fidl::WireResult res = dci_->StopController();
  ASSERT_OK(res.status());
  WaitForState(ep_num, TransferState::kIdle);

  dut_.runtime().RunUntil([&]() { return cancel_completed->signaled(); });
  ASSERT_TRUE(cancel_completed->signaled());
  ASSERT_TRUE(cancel_result->has_value());
  ASSERT_TRUE((*cancel_result)->is_ok());

  // Now, active_reqs and queued_reqs should be empty.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.server->active_reqs.size(), 0u);
    EXPECT_EQ(uep.server->queued_reqs.size(), 0u);
  });

  // Verify completions returned with cancellation error.
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(2);
  ASSERT_EQ(completions.size(), 2UL);
  if (enqueue_many) {
    EXPECT_EQ(completions[0].status, ZX_ERR_IO_NOT_PRESENT);
    EXPECT_EQ(completions[1].status, ZX_ERR_IO_NOT_PRESENT);
  } else {
    EXPECT_EQ(completions[0].status, ZX_ERR_CANCELED);
    EXPECT_EQ(completions[1].status, ZX_ERR_IO_NOT_PRESENT);
  }
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
  auto cancel_result =
      std::make_shared<std::optional<fidl::Result<fendpoint::Endpoint::CancelAll>>>();
  auto cancel_completed = std::make_shared<libsync::Completion>();
  ep_client_->CancelAll().Then(
      [cancel_result, cancel_completed](fidl::Result<fendpoint::Endpoint::CancelAll>& res) {
        *cancel_result = std::move(res);
        cancel_completed->Signal();
      });

  dut_.runtime().RunUntil([&]() { return cancel_completed->signaled(); });
  ASSERT_TRUE(cancel_completed->signaled());
  ASSERT_TRUE(cancel_result->has_value());
  ASSERT_TRUE((*cancel_result)->is_ok());
}

TEST_P(Dwc3EndpointsTest, CancelAllRequestsWhenPowerOff) {
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

  // Trigger started event so requests become active.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  auto expected_state = enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_state);

  // Simulate hardware power off (with a pre-existing pending_cancel_reason from an interrupted
  // cancel) and verify that CancelAll resets pending_cancel_reason alongside the queues and FIFO.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    uep.server->pending_cancel_reason = ZX_ERR_IO_NOT_PRESENT;
    Dwc3TestHelper::SetPowerOn(drv, false);
  });

  // Cancel all requests via client. It should reply immediately because power is off.
  auto cancel_result =
      std::make_shared<std::optional<fidl::Result<fendpoint::Endpoint::CancelAll>>>();
  auto cancel_completed = std::make_shared<libsync::Completion>();
  ep_client_->CancelAll().Then(
      [cancel_result, cancel_completed](fidl::Result<fendpoint::Endpoint::CancelAll>& res) {
        *cancel_result = std::move(res);
        cancel_completed->Signal();
      });

  dut_.runtime().RunUntil([&]() { return cancel_completed->signaled(); });
  ASSERT_TRUE(cancel_completed->signaled());
  ASSERT_TRUE(cancel_result->has_value());
  ASSERT_TRUE((*cancel_result)->is_ok());

  WaitForState(ep_num, TransferState::kIdle);

  // Now, active_reqs, queued_reqs, and pending_cancel_reason should be empty, and FIFO cleared.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.server->active_reqs.size(), 0u);
    EXPECT_EQ(uep.server->queued_reqs.size(), 0u);
    EXPECT_FALSE(uep.server->pending_cancel_reason.has_value());
    EXPECT_EQ(uep.fifo.GetActiveCount(), 0u);
  });

  // Verify completions returned with cancellation error.
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(2);
  ASSERT_EQ(completions.size(), 2u);
  EXPECT_EQ(completions[0].status, ZX_ERR_CANCELED);
  EXPECT_EQ(completions[1].status, ZX_ERR_CANCELED);
}

TEST_P(Dwc3EndpointsTest, CancelAllRequestsWhenIdle) {
  TriggerConnection();

  const uint8_t ep_address = 0x02;

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);

  // Endpoint is idle (no requests queued).
  // Cancel all requests via client. It should reply immediately because endpoint is idle.
  auto cancel_result =
      std::make_shared<std::optional<fidl::Result<fendpoint::Endpoint::CancelAll>>>();
  auto cancel_completed = std::make_shared<libsync::Completion>();
  ep_client_->CancelAll().Then(
      [cancel_result, cancel_completed](fidl::Result<fendpoint::Endpoint::CancelAll>& res) {
        *cancel_result = std::move(res);
        cancel_completed->Signal();
      });

  dut_.runtime().RunUntil([&]() { return cancel_completed->signaled(); });
  ASSERT_TRUE(cancel_completed->signaled());
  ASSERT_TRUE(cancel_result->has_value());
  ASSERT_TRUE((*cancel_result)->is_ok());
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
    const auto& trbs = uep.fifo.Read(2);

    AssertZlpUnchainedControlBits(trbs, enqueue_many);
  });

  // Trigger started event to initialize rsrc_id.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  TransferState expected_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_state);

  // Complete first TRB (data TRB).
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferInProgress(drv, ep_num); });
  dut_.runtime().RunUntilIdle();

  // The request should NOT be completed yet, because the ZLP TRB is still
  // pending.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 1u);
    EXPECT_EQ(uep.server->active_reqs.size(), 1u);
  });

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
  EXPECT_OK(completions[0].status);
  EXPECT_EQ(completions[0].transfer_size, 512UL);
}

TEST_F(Dwc3EndpointsTestBase, RingBufferWraparoundZlp) {
  SetEnableEnqueueManyTrbs(true);
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
    Dwc3TestHelper::AdvanceFifo(uep.fifo, advance_count);
    ASSERT_EQ(uep.fifo.GetActiveCount(), 0u);
    EXPECT_EQ(uep.fifo.WriteOffset(), uep.fifo.TotalSlots() - 1)
        << "Failed to advance write pointer to the wraparound boundary";
    EXPECT_EQ(uep.fifo.ReadOffset(), uep.fifo.TotalSlots() - 1)
        << "Failed to advance read pointer to the wraparound boundary";
  });

  // Enqueue the primary request that requires two TRBs and will trigger a ring buffer wraparound.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  QueueRequest(1, 512, 512, fdescriptor::EndpointType::kBulk, true);
  WaitForActiveCount(ep_num, 1);

  const dwc3_trb_t* initial_read = nullptr;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 2u);
    const auto& trbs = uep.fifo.Read(2);

    // Note: RingBufferWraparoundZlp is only ever executed with enqueue_many == true
    AssertZlpUnchainedControlBits(trbs, true);
    initial_read = uep.fifo.current_read();

    // Complete first (data) TRB to advance read_ across the Link TRB boundary to slot 0.
    TriggerEpTransferInProgress(drv, ep_num);
    const auto* wrapped_read = uep.fifo.current_read();

    // Distance should not be linear, proving it wrapped.
    EXPECT_LT(wrapped_read, initial_read);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 1u);

    // Complete second (ZLP) TRB so all queued TRBs are retired cleanly.
    TriggerEpTransferInProgress(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 0u);
  });
  WaitForActiveCount(ep_num, 0u);
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1UL);
  EXPECT_OK(completions[0].status);
  EXPECT_EQ(completions[0].transfer_size, 512UL);
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
    EXPECT_EQ(uep.fifo.GetActiveCount(), 2u);
    const auto& trbs = uep.fifo.Read(2);

    EXPECT_EQ(TRB_BUFSIZ(trbs[0].status), 4096UL);
    EXPECT_EQ(TRB_BUFSIZ(trbs[1].status), 0UL);
    AssertZlpUnchainedControlBits(trbs, enqueue_many);
  });

  // Trigger started event to initialize rsrc_id.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  TransferState expected_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_state);

  // Complete data TRB.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferInProgress(drv, ep_num); });
  dut_.runtime().RunUntilIdle();

  // Request pending completion of ZLP TRB.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 1u);
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
  EXPECT_EQ(completions[0].transfer_size, 4096UL);
}

TEST_F(Dwc3EndpointsTestBase, OutEndpointRingBufferWraparound) {
  SetEnableEnqueueManyTrbs(true);
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
    Dwc3TestHelper::AdvanceFifo(uep.fifo, advance_count);
    ASSERT_EQ(uep.fifo.GetActiveCount(), 0u);
    EXPECT_EQ(uep.fifo.WriteOffset(), uep.fifo.TotalSlots() - 1);
    EXPECT_EQ(uep.fifo.ReadOffset(), uep.fifo.TotalSlots() - 1);
  });

  // Enqueue OUT request sitting on wraparound boundary.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  QueueRequest(1, 512, 1024, fdescriptor::EndpointType::kBulk, false);
  WaitForActiveCount(ep_num, 1);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 1u);

    const auto* initial_read = uep.fifo.current_read();
    TriggerEpTransferInProgress(drv, ep_num);
    const auto* wrapped_read = uep.fifo.current_read();

    // Verify wraparound across slot 255 Link TRB back to index 0
    EXPECT_LT(wrapped_read, initial_read);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 0u);
  });
  WaitForActiveCount(ep_num, 0u);
}

TEST_F(Dwc3EndpointsTestBase, EndpointStallAndClear) {
  auto last_depcmd = std::make_shared<std::atomic<uint32_t>>(0);
  auto write_called = std::make_shared<std::atomic<bool>>(false);

  auto cleanup_callbacks = DeferClearDepcmdCallbacks(2);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    uep->ep.type = fdescriptor::EndpointType::kBulk;
    uep->ep.max_packet_size = 512;

    // Enable the endpoint
    Dwc3TestHelper::EpSetConfig(drv, uep->ep, true);
  });

  dut_.RunInEnvironmentTypeContext([last_depcmd, write_called](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(2).addr()];
    depcmd.SetWriteCallback([last_depcmd, write_called](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      last_depcmd->store(val);
      write_called->store(true);
    });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    EXPECT_TRUE(uep->ep.enabled);
    Dwc3TestHelper::EpSetStall(drv, uep->ep, true);
  });

  EXPECT_TRUE(write_called->load());
  EXPECT_EQ(DEPCMD::Get(2).FromValue(last_depcmd->load()).CMDTYP(), DEPCMD::DEPSSTALL);

  write_called->store(false);
  last_depcmd->store(0);
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    EXPECT_TRUE(uep->ep.enabled);
    Dwc3TestHelper::EpSetStall(drv, uep->ep, false);
  });

  EXPECT_TRUE(write_called->load());
  EXPECT_EQ(DEPCMD::Get(2).FromValue(last_depcmd->load()).CMDTYP(), DEPCMD::DEPCSTALL);
}

TEST_F(Dwc3EndpointsTestBase, EndpointConfiguration) {
  auto depcfg_called = std::make_shared<std::atomic<bool>>(false);
  auto depxfercfg_called = std::make_shared<std::atomic<bool>>(false);
  auto dalepena_called = std::make_shared<std::atomic<bool>>(false);

  auto cleanup_depcmd = DeferClearDepcmdCallbacks(2);
  auto cleanup_dalepena = fit::defer([this]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[DALEPENA::Get().addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    uep->ep.type = fdescriptor::EndpointType::kBulk;
    uep->ep.max_packet_size = 512;
  });

  dut_.RunInEnvironmentTypeContext(
      [depcfg_called, depxfercfg_called, dalepena_called](Environment& env) {
        auto& depcmd = env.reg_region()[DEPCMD::Get(2).addr()];
        depcmd.SetWriteCallback([depcfg_called, depxfercfg_called](uint64_t val_raw) {
          uint32_t val = static_cast<uint32_t>(val_raw);
          auto cmd = DEPCMD::Get(2).FromValue(val);
          if (cmd.CMDTYP() == DEPCMD::DEPCFG) {
            depcfg_called->store(true);
          } else if (cmd.CMDTYP() == DEPCMD::DEPXFERCFG) {
            depxfercfg_called->store(true);
          }
        });

        auto& dalepena = env.reg_region()[DALEPENA::Get().addr()];
        dalepena.SetWriteCallback(
            [dalepena_called](uint64_t val_raw) { dalepena_called->store(true); });
      });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    Dwc3TestHelper::EpSetConfig(drv, uep->ep, true);
  });

  EXPECT_TRUE(depcfg_called->load());
  EXPECT_TRUE(depxfercfg_called->load());
  EXPECT_TRUE(dalepena_called->load());
}

TEST_F(Dwc3EndpointsTestBase, EndpointReset) {
  auto dalepena_called = std::make_shared<std::atomic<bool>>(false);
  auto dalepena_val = std::make_shared<std::atomic<uint32_t>>(0);

  SetUpAndPowerOnEndpoints();

  auto cleanup_callbacks = fit::defer([&]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[DALEPENA::Get().addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    ASSERT_NE(uep, nullptr);
    uep->ep.type = fdescriptor::EndpointType::kBulk;
    uep->ep.max_packet_size = 512;

    // Enable it first
    Dwc3TestHelper::EpSetConfig(drv, uep->ep, true);

    // Set some flags
    Dwc3TestHelper::SetGotNotReady(drv, 2, true);
    Dwc3TestHelper::SetEpRsrcId(drv, 2, 5);
    Dwc3TestHelper::SetEpTransferState(drv, 2, Dwc3TestHelper::TransferState::kActiveSingle);
  });

  dut_.RunInEnvironmentTypeContext([dalepena_called, dalepena_val](Environment& env) {
    auto& dalepena = env.reg_region()[DALEPENA::Get().addr()];
    dalepena.SetWriteCallback([dalepena_called, dalepena_val](uint64_t val_raw) {
      dalepena_called->store(true);
      dalepena_val->store(static_cast<uint32_t>(val_raw));
    });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    Dwc3TestHelper::EpReset(drv, uep->ep);

    // Verify flags are reset
    EXPECT_FALSE(Dwc3TestHelper::GetGotNotReady(drv, 2));
  });

  EXPECT_TRUE(dalepena_called->load());
  EXPECT_EQ(dalepena_val->load() & (1 << 2), 0u);
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

  auto requests = CreateVmoBuffer(sync_client, max_packet_size, max_packet_size);
  QueueRequestsAndWaitForStartTransfer(ep_addr, sync_client, std::move(requests));

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

  auto requests = CreateVmoBuffer(sync_client, max_packet_size, max_packet_size);
  QueueRequestsAndWaitForStartTransfer(ep_addr, sync_client, std::move(requests));

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

  auto requests = CreateVmoBuffer(sync_client, max_packet_size, max_packet_size);
  QueueRequestsAndWaitForStartTransfer(ep_addr, sync_client, std::move(requests));

  // Deliver the started event to make the endpoint active with resource ID.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    constexpr uint8_t kMockRsrcId = 5;
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, ep_addr, kMockRsrcId);
  });

  // Call CancelAll via FIDL to cancel requests during an active transfer.
  auto cancel_result = sync_client->CancelAll();
  ASSERT_TRUE(cancel_result.is_ok())
      << "CancelAll failed: " << cancel_result.error_value().FormatDescription();

  // Wait for request completion (EpServer::CancelAll completes cancelled requests with
  // ZX_ERR_CANCELED).
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

  auto requests = CreateVmoBuffer(sync_client, max_packet_size, max_packet_size);
  QueueRequestsAndWaitForStartTransfer(ep_addr, sync_client, std::move(requests));

  // Deliver the started event to make the endpoint active with resource ID.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    constexpr uint8_t kMockRsrcId = 5;
    Dwc3TestHelper::HandleEpTransferStartedEvent(drv, ep_addr, kMockRsrcId);
  });

  // Verify DEPENDXFER was NOT sent
  auto dependxfer_sent = std::make_shared<std::atomic<bool>>(false);
  auto cleanup_callbacks = DeferClearDepcmdCallbacks(ep_addr);
  dut_.RunInEnvironmentTypeContext([dependxfer_sent, ep_addr](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(ep_addr).addr()];
    depcmd.SetWriteCallback([dependxfer_sent, ep_addr](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      if (DEPCMD::Get(ep_addr).FromValue(val).CMDTYP() == DEPCMD::DEPENDXFER) {
        dependxfer_sent->store(true);
      }
    });
  });

  // Call DisableEndpoint (via UserEpReset in test helper)
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, ep_addr);
    Dwc3TestHelper::UserEpReset(drv, *uep);
  });

  // Verify DEPENDXFER was NOT sent
  EXPECT_FALSE(dependxfer_sent->load());

  // Explicitly dispatch the asynchronous request completion event from the sync client.
  // EpServer::CancelAll completes cancelled requests with ZX_ERR_IO_NOT_PRESENT upon disable.
  ASSERT_TRUE(sync_client.HandleOneEvent(event_handler).ok());
  EXPECT_TRUE(completed);
  EXPECT_EQ(completion_status, ZX_ERR_IO_NOT_PRESENT);

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
    uep->ep.type = fdescriptor::EndpointType::kBulk;
    uep->ep.max_packet_size = 512;
    uep->ep.enabled = true;
    uep->ep.got_not_ready = true;
    ASSERT_TRUE(Dwc3TestHelper::InitFifo(drv, 2).is_ok());

    auto* dispatcher = Dwc3TestHelper::GetDispatcher(drv);
    uep->server->Connect(dispatcher, std::move(endpoints->server));
  });

  fidl::SyncClient<fuchsia_hardware_usb_endpoint::Endpoint> sync_client{
      std::move(endpoints->client)};

  auto requests = CreateVmoBuffer(sync_client, 512, 512, 1);
  QueueRequestsAndWaitForStartTransfer(2, sync_client, std::move(requests));

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
    uep->ep.type = fdescriptor::EndpointType::kBulk;
    uep->ep.max_packet_size = 512;
    uep->ep.enabled = true;
    uep->ep.got_not_ready = true;
    ASSERT_TRUE(Dwc3TestHelper::InitFifo(drv, 3).is_ok());  // Use IN endpoint 3

    auto* dispatcher = Dwc3TestHelper::GetDispatcher(drv);
    uep->server->Connect(dispatcher, std::move(endpoints->server));
  });

  fidl::SyncClient<fuchsia_hardware_usb_endpoint::Endpoint> sync_client{
      std::move(endpoints->client)};

  auto requests = CreateVmoBuffer(sync_client, 512, 0, 1);
  QueueRequestsAndWaitForStartTransfer(3, sync_client, std::move(requests));

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
    Dwc3TestHelper::HandleEpTransferCompleteEvent(drv, 3);
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
    uep->ep.type = fdescriptor::EndpointType::kBulk;
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

  auto requests = CreateVmoBuffer(sync_client, 512, 512, 1);

  // 2. Queue request while endpoint is disabled!
  auto result = sync_client->QueueRequests({std::move(requests)});
  ASSERT_TRUE(result.is_ok()) << "QueueRequests failed: " << result.error_value().status_string();

  dut_.runtime().RunUntilIdle();

  // 3. VERIFY SILICON BUFFERING:
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

  // 4. Simulate Host enabling the endpoint (e.g. SET_CONFIGURATION complete)
  // - Enable it in the driver and call UserEpQueueNext()
  auto completion = std::make_shared<libsync::Completion>();
  auto cleanup_callbacks = DeferClearDepcmdCallbacks(2);

  dut_.RunInEnvironmentTypeContext([completion](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(2).addr()];
    depcmd.SetWriteCallback([completion](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      if (DEPCMD::Get(2).FromValue(val).CMDTYP() ==
          DEPCMD::DEPSTRTXFER) {  // DEPSTRTXFER (DMA starts)
        completion->Signal();
      }
    });
    depcmd.SetReadCallback([]() -> uint32_t { return 0; });
  });

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 2);
    // Configure and enable! This natively and automatically drains the software queue!
    Dwc3TestHelper::EpSetConfig(drv, uep->ep, true);
  });

  // 5. VERIFY RESUMPTION:
  // - Assert that DMA successfully starts on the buffered request!
  dut_.runtime().RunUntil([&]() { return completion->signaled(); });
  ASSERT_TRUE(completion->signaled());

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

// Verifies that resetting an endpoint with an active cancel in progress completes outstanding
// requests without leaking FIFO or resource tracking when a trailing completion event arrives.
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
    uep->ep.type = fdescriptor::EndpointType::kBulk;
    uep->ep.max_packet_size = 512;
    uep->ep.enabled = true;
    uep->ep.got_not_ready = true;
    ASSERT_TRUE(Dwc3TestHelper::InitFifo(drv, 7).is_ok());

    auto* dispatcher = Dwc3TestHelper::GetDispatcher(drv);
    uep->server->Connect(dispatcher, std::move(endpoints->server));
  });

  fidl::SyncClient<fuchsia_hardware_usb_endpoint::Endpoint> sync_client{
      std::move(endpoints->client)};

  auto requests = CreateVmoBuffer(sync_client, 512, 512, 1);
  QueueRequestsAndWaitForStartTransfer(7, sync_client, std::move(requests));

  // 2. Force an EndTransfer operation with a simulated busy hardware return to mark
  // ep->pending_cancel = true. This is done by calling CancelAll before
  // HandleEpTransferStartedEvent, which leaves ep.rsrc_id at kInvalidResourceId.
  auto cancel_result = sync_client->CancelAll();
  ASSERT_TRUE(cancel_result.is_ok())
      << "CancelAll failed: " << cancel_result.error_value().FormatDescription();

  // 3. Forcefully execute the endpoint clear/disable track (UserEpReset)
  // which completes the request with ZX_ERR_IO_NOT_PRESENT and wipes the tracking pointer.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto* uep = Dwc3TestHelper::GetUserEndpoint(drv, 7);
    Dwc3TestHelper::UserEpReset(drv, *uep);
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
  auto cancel_replied = std::make_shared<std::atomic<bool>>(false);
  ep_client_->CancelAll().Then([cancel_replied](fidl::Result<fendpoint::Endpoint::CancelAll>& res) {
    cancel_replied->store(true);
  });

  WaitForState(ep_num, TransferState::kCanceling);
  EXPECT_FALSE(cancel_replied->load());

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
  auto cancel_replied = std::make_shared<std::atomic<bool>>(false);
  ep_client_->CancelAll().Then([cancel_replied](fidl::Result<fendpoint::Endpoint::CancelAll>& res) {
    cancel_replied->store(true);
  });

  WaitForState(ep_num, TransferState::kCanceling);
  EXPECT_FALSE(cancel_replied->load());

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

// Tests that completing a single-transfer request via TransferComplete correctly
// returns the endpoint state machine to kIdle and starts the next queued request.
TEST_P(Dwc3EndpointsTest, SingleTransfer_SequentialCompletionAndNextStart) {
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

  // Complete request 1 via TransferComplete.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferComplete(drv, ep_num); });

  // Verify that request 2 is dequeued and begins starting.
  WaitForState(ep_num, TransferState::kStartingSingle);
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 1u);
    EXPECT_EQ(uep.server->active_reqs.size(), 1u);
    EXPECT_EQ(uep.server->queued_reqs.size(), 0u);
  });

  // Start and complete request 2 via TransferComplete.
  dut_.RunInDriverContext(
      [&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId + 1); });
  WaitForState(ep_num, TransferState::kActiveSingle);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferComplete(drv, ep_num); });

  WaitForState(ep_num, TransferState::kIdle);
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(2);
  EXPECT_EQ(completions.size(), 2u);
}

// Tests that when the active queue drains to 0 via TransferComplete, the endpoint
// returns to kIdle and subsequent calls to QueueRequests start transfers cleanly.
TEST_P(Dwc3EndpointsTest, SingleTransfer_ActiveQueueDrainThenRequeue) {
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

  // Complete request 1 via TransferComplete so active count drains to 0.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferComplete(drv, ep_num); });

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
// correctly enqueues and completes 2 TRBs (data TRB via TransferInProgress + ZLP TRB via
// TransferComplete).
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

  // Complete intermediate data TRB via TransferInProgress.
  // Request should not complete yet and state remains kActiveSingle because ZLP TRB is pending.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferInProgress(drv, ep_num); });
  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(event_handler_.completion_count(), 0u);
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kActiveSingle);
    ASSERT_EQ(uep.server->active_reqs.size(), 1u);
    EXPECT_EQ(uep.server->active_reqs.front().completed_trbs, 1u);
    EXPECT_EQ(uep.server->active_reqs.front().completed_bytes, 64u);
  });

  // Complete terminal ZLP TRB via TransferComplete.
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

  auto cancel_replied = std::make_shared<std::atomic<bool>>(false);
  ep_client_->CancelAll().Then([cancel_replied](fidl::Result<fendpoint::Endpoint::CancelAll>& res) {
    cancel_replied->store(true);
  });

  // Unstarted requests 2 & 3 should be completed immediately with IO_NOT_PRESENT.
  std::vector<CompletionResult> completions1 = event_handler_.WaitForCompletions(2);
  EXPECT_EQ(completions1.size(), 2u);
  EXPECT_FALSE(cancel_replied->load());

  WaitForState(ep_num, TransferState::kCanceling);

  // Trigger TransferEnded event for active Request 1.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferEnded(drv, ep_num); });

  dut_.runtime().RunUntil([&]() { return cancel_replied->load(); });
  EXPECT_TRUE(cancel_replied->load());
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

  auto cancel_replied = std::make_shared<std::atomic<bool>>(false);
  ep_client_->CancelAll().Then([cancel_replied](fidl::Result<fendpoint::Endpoint::CancelAll>& res) {
    cancel_replied->store(true);
  });

  std::vector<CompletionResult> completions1 = event_handler_.WaitForCompletions(1);
  EXPECT_EQ(completions1.size(), 1u);
  WaitForState(ep_num, TransferState::kPendingCancel);
  EXPECT_FALSE(cancel_replied->load());

  // When TransferStarted arrives, it should transition to kCanceling and issue EndTransfer.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, TransferState::kCanceling);

  // TransferEnded completes the cancellation.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferEnded(drv, ep_num); });
  dut_.runtime().RunUntil([&]() { return cancel_replied->load(); });
  EXPECT_TRUE(cancel_replied->load());
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

// Verifies that calling CancelAll on a stalled/halted endpoint drains in-flight requests with
// ZX_ERR_CANCELED once the hardware EndTransfer sequence completes.
TEST_P(Dwc3EndpointsTest, CancelAllStrandedWhenHalted) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue a request.
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);

  auto expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);

  // Trigger started event to move to active.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  auto expected_state = enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_state);

  // Call SetStall(true) on the endpoint.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    Dwc3TestHelper::EpSetStall(drv, uep.ep, true);
  });

  // Initiate CancelAll
  auto cancel_result =
      std::make_shared<std::optional<fidl::Result<fendpoint::Endpoint::CancelAll>>>();
  auto cancel_completed = std::make_shared<libsync::Completion>();
  ep_client_->CancelAll().Then(
      [cancel_result, cancel_completed](fidl::Result<fendpoint::Endpoint::CancelAll>& res) {
        *cancel_result = std::move(res);
        cancel_completed->Signal();
      });

  WaitForState(ep_num, TransferState::kCanceling);
  EXPECT_FALSE(cancel_completed->signaled());

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferEnded(drv, ep_num); });

  WaitForState(ep_num, TransferState::kIdle);
  dut_.runtime().RunUntil([&]() { return cancel_completed->signaled(); });
  ASSERT_TRUE(cancel_completed->signaled());
  ASSERT_TRUE(cancel_result->has_value());
  ASSERT_TRUE((*cancel_result)->is_ok());

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1UL);
  EXPECT_EQ(completions[0].status, ZX_ERR_CANCELED);
}

// Verifies that calling CancelAll on a stalled endpoint while in idle state immediately drains
// queued requests with ZX_ERR_CANCELED without waiting for hardware EndTransfer.
TEST_F(Dwc3EndpointsTestBase, CancelAllStalledWhileIdle) {
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  // Stall the endpoint before any transfer starts.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    Dwc3TestHelper::EpSetStall(drv, uep.ep, true);
  });

  // Queue a request.
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  WaitForQueuedCount(ep_num, 1);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.server->queued_reqs.size(), 1u);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kIdle);
  });

  // CancelAll should complete immediately using natural async bindings.
  auto cancel_result =
      std::make_shared<std::optional<fidl::Result<fendpoint::Endpoint::CancelAll>>>();
  auto cancel_completed = std::make_shared<libsync::Completion>();
  ep_client_->CancelAll().Then(
      [cancel_result, cancel_completed](fidl::Result<fendpoint::Endpoint::CancelAll>& res) {
        *cancel_result = std::move(res);
        cancel_completed->Signal();
      });

  dut_.runtime().RunUntil([&]() { return cancel_completed->signaled(); });
  ASSERT_TRUE(cancel_completed->signaled());
  ASSERT_TRUE(cancel_result->has_value());
  ASSERT_TRUE((*cancel_result)->is_ok());

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1UL);
  EXPECT_EQ(completions[0].status, ZX_ERR_CANCELED);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_TRUE(uep.server->queued_reqs.empty());
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kIdle);
  });
}

// Verifies that requests queued while an endpoint is stalled remain in queued_reqs and resume
// execution automatically once the stall is cleared.
TEST_P(Dwc3EndpointsTest, QueueRequestsWhileStalledResumeOnClearStall) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Stall the endpoint.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    Dwc3TestHelper::EpSetStall(drv, uep.ep, true);
  });

  // Queue request while stalled.
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  WaitForQueuedCount(ep_num, 1);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.server->queued_reqs.size(), 1u);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kIdle);
  });

  // Clear stall via ClearStall request.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    Dwc3TestHelper::EpSetStall(drv, uep.ep, false);
    Dwc3TestHelper::UserEpQueueNext(drv, uep);
  });

  auto expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  auto expected_state = enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_state);

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
  EXPECT_EQ(completions[0].transfer_size, 512UL);
}

// Verifies that when queuing requests with unregistered VMO IDs alongside valid requests,
// requests with unregistered VMO IDs complete with ZX_ERR_NOT_FOUND without disrupting valid
// requests.
// DISABLED: Requires invalid VMO ID validation and error handling in the driver (addressed in
// subsequent driver CL).
TEST_P(Dwc3EndpointsTest, DISABLED_UnregisteredVmoTransferCompletesWithNotFound) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);

  // Register a valid VMO ID 2.
  RegisterVmo(2, 4096);

  // Note: we purposely do NOT call RegisterVmo for VMO ID 1,
  // so any request with vmo_id=1 will fail `get_iter` since the VMO ID is unregistered.

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue a request with an unregistered VMO ID (e.g. 1).
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);

  // Queue a request with a valid VMO ID (2).
  QueueRequest(2, 0, 512, fdescriptor::EndpointType::kBulk);

  // The driver will process these in UserEpQueueNextSingle or UserEpQueueNextOngoing
  // depending on `enqueue_many`. The first should fail `get_iter()` and complete with an
  // error, while the second should succeed.

  // Wait for the valid endpoint transfer to complete.
  if (enqueue_many) {
    WaitForState(ep_num, TransferState::kStartingOngoing);
    dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
    WaitForState(ep_num, TransferState::kActiveOngoing);
    dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferInProgress(drv, ep_num); });
    WaitForActiveCount(ep_num, 0u);
  } else {
    // For single transfer, wait for it to be active and then complete it.
    WaitForState(ep_num, TransferState::kStartingSingle);
    dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
    WaitForState(ep_num, TransferState::kActiveSingle);
    dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferComplete(drv, ep_num); });
    WaitForState(ep_num, TransferState::kIdle);
  }

  // We should receive 2 completions: one ZX_ERR_NOT_FOUND, one ZX_OK.
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(2);
  ASSERT_EQ(completions.size(), 2UL);
  EXPECT_EQ(completions[0].status, ZX_ERR_NOT_FOUND);
  EXPECT_EQ(completions[0].transfer_size, 0UL);
  EXPECT_OK(completions[1].status);
  EXPECT_EQ(completions[1].transfer_size, 512UL);

  // Verify that the internal software queues correctly popped both requests, leaving a clean
  // state.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_TRUE(uep.server->queued_reqs.empty());
    EXPECT_TRUE(uep.server->active_reqs.empty());
  });

  auto cancel_result =
      std::make_shared<std::optional<fidl::Result<fendpoint::Endpoint::CancelAll>>>();
  auto cancel_completed = std::make_shared<libsync::Completion>();
  ep_client_->CancelAll().Then(
      [cancel_result, cancel_completed](fidl::Result<fendpoint::Endpoint::CancelAll>& res) {
        *cancel_result = std::move(res);
        cancel_completed->Signal();
      });

  if (enqueue_many) {
    WaitForState(ep_num, TransferState::kCanceling);
    dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferEnded(drv, ep_num); });
    WaitForState(ep_num, TransferState::kIdle);
  }

  dut_.runtime().RunUntil([&]() { return cancel_completed->signaled(); });
  ASSERT_TRUE(cancel_completed->signaled());
  ASSERT_TRUE(cancel_result->has_value());
  ASSERT_TRUE((*cancel_result)->is_ok());

  // There should be no stranded completions returned by CancelAll.
  EXPECT_EQ(event_handler_.completion_count(), 0UL);
}

// Verifies that queuing a request with an unregistered VMO ID immediately completes with
// ZX_ERR_NOT_FOUND and does not trigger a hardware kick or leave the endpoint in a non-idle state.
// DISABLED: Requires invalid VMO ID validation and error handling in the driver (addressed in
// subsequent driver CL).
TEST_F(Dwc3EndpointsTestBase, DISABLED_UnregisteredVmoIdImmediatelyCompletesNotFound) {
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);
  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue a request with an unregistered VMO ID (e.g. 1).
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  dut_.runtime().RunUntilIdle();

  // We should receive 1 completion: ZX_ERR_NOT_FOUND.
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1UL);
  EXPECT_EQ(completions[0].status, ZX_ERR_NOT_FOUND);

  // The transfer state should remain kIdle since enqueued was 0, so no hardware kick.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kIdle);
  });
}

// Verifies that a zero-length transfer on an IN endpoint correctly generates a zero-length TRB
// and completes with ZX_OK and 0 bytes transferred.
// DISABLED: Requires driver support for 0-byte IN TRB generation.
TEST_P(Dwc3EndpointsTest, DISABLED_ZeroLengthInTransfer) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x82;  // IN endpoint
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue a 0-length request
  QueueRequest(1, 0, 0, fdescriptor::EndpointType::kBulk);

  TransferState expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);

  // Verify TRB length is 0
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    dwc3_trb_t* trb = uep.fifo.current_read();
    EXPECT_EQ(TRB_BUFSIZ(trb->status), 0UL);
  });

  // Trigger started event to initialize rsrc_id.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  TransferState expected_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_state);

  // Complete TRB
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
  EXPECT_EQ(completions[0].transfer_size, 0UL);
}

// Verifies that a request with no data buffer regions specified is treated as a valid zero-length
// transfer on an IN endpoint and completes with ZX_OK and 0 bytes transferred.
// DISABLED: Requires driver support for 0-byte IN TRB generation with empty data fields.
TEST_P(Dwc3EndpointsTest, DISABLED_ZeroLengthTransferNoDataField) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x82;  // IN endpoint
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue a request with NO data field
  frequest::Request req;
  req.defer_completion(false);
  req.information(frequest::RequestInfo::WithBulk(frequest::BulkRequestInfo{}));
  req.short_(false);

  std::vector<frequest::Request> reqs;
  reqs.reserve(1);
  reqs.push_back(std::move(req));
  // Note: QueueRequests is a one-way FIDL method returning fit::result<fidl::OneWayError>
  // synchronously.
  fit::result result = ep_client_->QueueRequests({std::move(reqs)});
  ASSERT_TRUE(result.is_ok()) << "QueueRequests failed: "
                              << result.error_value().FormatDescription();

  TransferState expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);

  // Verify TRB length is 0
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    dwc3_trb_t* trb = uep.fifo.current_read();
    EXPECT_EQ(TRB_BUFSIZ(trb->status), 0UL);
  });

  // Trigger started event to initialize rsrc_id.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  TransferState expected_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_state);

  // Complete TRB
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
  EXPECT_EQ(completions[0].transfer_size, 0UL);
}

// Verifies that a buffer region with an omitted size field is rejected with ZX_ERR_INVALID_ARGS.
// DISABLED: Requires buffer region size validation in the driver.
TEST_F(Dwc3EndpointsTestBase, DISABLED_ImplicitBufferRegionSize) {
  TriggerConnection();

  const uint8_t ep_address = 0x82;  // IN endpoint
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue a request with explicit VMO ID but omitted region size
  frequest::BufferRegion region;
  region.buffer(frequest::Buffer::WithVmoId(1));
  region.offset(0);

  std::vector<frequest::BufferRegion> regions;
  regions.reserve(1);
  regions.push_back(std::move(region));

  frequest::Request req;
  req.data(std::move(regions));
  req.defer_completion(false);
  req.information(frequest::RequestInfo::WithBulk(frequest::BulkRequestInfo{}));
  req.short_(false);

  std::vector<frequest::Request> reqs;
  reqs.reserve(1);
  reqs.push_back(std::move(req));
  // Note: QueueRequests is a one-way FIDL method returning fit::result<fidl::OneWayError>
  // synchronously.
  fit::result result = ep_client_->QueueRequests({std::move(reqs)});
  ASSERT_TRUE(result.is_ok()) << "QueueRequests failed: "
                              << result.error_value().FormatDescription();
  dut_.runtime().RunUntilIdle();

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1UL);
  EXPECT_EQ(completions[0].status, ZX_ERR_INVALID_ARGS);
}

// Verifies that multi-region requests combining zero-length and non-zero-length buffer regions
// are rejected with ZX_ERR_INVALID_ARGS.
TEST_F(Dwc3EndpointsTestBase, MixedMultiRegionTransfer) {
  TriggerConnection();

  const uint8_t ep_address = 0x82;  // IN endpoint
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);
  RegisterVmo(2, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue a request with [0B, 512B]
  frequest::BufferRegion region1;
  region1.buffer(frequest::Buffer::WithVmoId(1));
  region1.offset(0);
  region1.size(0);

  frequest::BufferRegion region2;
  region2.buffer(frequest::Buffer::WithVmoId(2));
  region2.offset(0);
  region2.size(512);

  std::vector<frequest::BufferRegion> regions;
  regions.reserve(2);
  regions.push_back(std::move(region1));
  regions.push_back(std::move(region2));

  frequest::Request req;
  req.data(std::move(regions));
  req.defer_completion(false);
  req.information(frequest::RequestInfo::WithBulk(frequest::BulkRequestInfo{}));
  req.short_(false);

  std::vector<frequest::Request> reqs;
  reqs.reserve(1);
  reqs.push_back(std::move(req));
  // Note: QueueRequests is a one-way FIDL method returning fit::result<fidl::OneWayError>
  // synchronously.
  fit::result result = ep_client_->QueueRequests({std::move(reqs)});
  ASSERT_TRUE(result.is_ok()) << "QueueRequests failed: "
                              << result.error_value().FormatDescription();
  dut_.runtime().RunUntilIdle();

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1UL);
  EXPECT_STATUS(completions[0].status, ZX_ERR_INVALID_ARGS);
}

// Verifies that OUT transfers with sizes that are not multiples of the endpoint's maximum
// packet size are rejected with ZX_ERR_INVALID_ARGS to prevent hardware short packet errors.
TEST_F(Dwc3EndpointsTestBase, InvalidOutTransferSize) {
  TriggerConnection();
  const uint8_t ep_address = 0x02;  // OUT endpoint
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);
  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);

  RegisterVmo(1, 4096);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue 513 bytes (not multiple of 512 max packet size)
  QueueRequest(1, 0, 513, fdescriptor::EndpointType::kBulk);
  dut_.runtime().RunUntilIdle();

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1UL);
  EXPECT_EQ(completions[0].status, ZX_ERR_INVALID_ARGS);
}

// Verifies that transfers requesting offsets and sizes extending beyond the registered VMO's
// bounds are rejected with ZX_ERR_OUT_OF_RANGE.
// DISABLED: Requires registered VMO bounds checking in the driver.
TEST_F(Dwc3EndpointsTestBase, DISABLED_OutOfRangeVmoTransfer) {
  TriggerConnection();
  const uint8_t ep_address = 0x82;  // IN endpoint
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);
  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);

  RegisterVmo(1, 4096);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue request with size exceeding remaining VMO space
  // Offset 4000, size 512 (4000 + 512 = 4512 > 4096)
  QueueRequest(1, 4000, 512, fdescriptor::EndpointType::kBulk);
  dut_.runtime().RunUntilIdle();

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1UL);
  EXPECT_EQ(completions[0].status, ZX_ERR_OUT_OF_RANGE);
}

// Verifies that a scatter-gather request across multiple buffer regions successfully
// allocates chained TRBs with proper control bits and completes successfully.
TEST_P(Dwc3EndpointsTest, ScatterGatherBulkTransfer) {
  const bool enqueue_many = GetParam();
  TriggerConnection();
  const uint8_t ep_address = 0x82;  // IN endpoint
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);
  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);

  RegisterVmo(1, 4096);
  RegisterVmo(2, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue request with multiple non-zero-length regions (512B each).
  frequest::BufferRegion region1;
  region1.buffer(frequest::Buffer::WithVmoId(1));
  region1.offset(0);
  region1.size(512);

  frequest::BufferRegion region2;
  region2.buffer(frequest::Buffer::WithVmoId(2));
  region2.offset(0);
  region2.size(512);

  std::vector<frequest::BufferRegion> regions;
  regions.reserve(2);
  regions.push_back(std::move(region1));
  regions.push_back(std::move(region2));

  frequest::Request req;
  req.data(std::move(regions));
  req.defer_completion(false);
  req.information(frequest::RequestInfo::WithBulk(frequest::BulkRequestInfo{}));
  req.short_(false);

  std::vector<frequest::Request> reqs;
  reqs.reserve(1);
  reqs.push_back(std::move(req));

  fit::result result = ep_client_->QueueRequests({std::move(reqs)});
  ASSERT_TRUE(result.is_ok()) << "QueueRequests failed: "
                              << result.error_value().FormatDescription();

  TransferState expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);
  WaitForActiveCount(ep_num, 1u);

  // Verify that the FIFO allocated 2 TRBs with proper chained control bits.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    UserEndpoint& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 2u);

    const std::vector<dwc3_trb_t> trbs = uep.fifo.Read(2);
    ASSERT_EQ(trbs.size(), 2u);

    // TRB 0 must have TRB_CHN set, and not TRB_IOC.
    EXPECT_TRUE(trbs[0].control & TRB_HWO);
    EXPECT_TRUE(trbs[0].control & TRB_CHN);
    EXPECT_FALSE(trbs[0].control & TRB_IOC);
    EXPECT_EQ(TRB_BUFSIZ(trbs[0].status), 512u);

    // TRB 1 must have TRB_IOC set, and not TRB_CHN.
    EXPECT_TRUE(trbs[1].control & TRB_HWO);
    EXPECT_FALSE(trbs[1].control & TRB_CHN);
    EXPECT_TRUE(trbs[1].control & TRB_IOC);
    if (!enqueue_many) {
      EXPECT_TRUE(trbs[1].control & TRB_LST);
    }
    EXPECT_EQ(TRB_BUFSIZ(trbs[1].status), 512u);
  });

  // Start the transfer.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  TransferState expected_active_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_active_state);

  // Complete the chained TD (TRB 0 + TRB 1) in a single event at the TRB_IOC boundary.
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
  EXPECT_EQ(completions[0].transfer_size, 1024UL);
}

// Verifies scatter-gather on an interrupt endpoint which always uses single-transfer mode.
TEST_P(Dwc3EndpointsTest, ScatterGatherInterruptTransfer) {
  TriggerConnection();
  const uint8_t ep_address = 0x82;  // IN endpoint
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);
  SetupEndpoint(ep_address, fdescriptor::EndpointType::kInterrupt, 64);

  RegisterVmo(1, 4096);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  frequest::BufferRegion region1;
  region1.buffer(frequest::Buffer::WithVmoId(1));
  region1.offset(0);
  region1.size(64);

  frequest::BufferRegion region2;
  region2.buffer(frequest::Buffer::WithVmoId(1));
  region2.offset(64);
  region2.size(64);

  std::vector<frequest::BufferRegion> regions;
  regions.reserve(2);
  regions.push_back(std::move(region1));
  regions.push_back(std::move(region2));

  frequest::Request req;
  req.data(std::move(regions));
  req.defer_completion(false);
  req.information(frequest::RequestInfo::WithInterrupt(frequest::InterruptRequestInfo{}));
  req.short_(false);

  std::vector<frequest::Request> reqs;
  reqs.reserve(1);
  reqs.push_back(std::move(req));

  fit::result result = ep_client_->QueueRequests({std::move(reqs)});
  ASSERT_TRUE(result.is_ok()) << "QueueRequests failed: "
                              << result.error_value().FormatDescription();

  WaitForState(ep_num, TransferState::kStartingSingle);
  WaitForActiveCount(ep_num, 1u);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    UserEndpoint& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 2u);

    const std::vector<dwc3_trb_t> trbs = uep.fifo.Read(2);
    ASSERT_EQ(trbs.size(), 2u);

    EXPECT_TRUE(trbs[0].control & TRB_HWO);
    EXPECT_TRUE(trbs[0].control & TRB_CHN);
    EXPECT_FALSE(trbs[0].control & TRB_IOC);
    EXPECT_EQ(TRB_BUFSIZ(trbs[0].status), 64u);

    EXPECT_TRUE(trbs[1].control & TRB_HWO);
    EXPECT_FALSE(trbs[1].control & TRB_CHN);
    EXPECT_TRUE(trbs[1].control & TRB_IOC);
    EXPECT_TRUE(trbs[1].control & TRB_LST);
    EXPECT_EQ(TRB_BUFSIZ(trbs[1].status), 64u);
  });

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, TransferState::kActiveSingle);

  // Complete the chained TD (TRB 0 + TRB 1) in a single event at the TRB_IOC boundary.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferComplete(drv, ep_num); });
  WaitForState(ep_num, TransferState::kIdle);

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1UL);
  EXPECT_OK(completions[0].status);
  EXPECT_EQ(completions[0].transfer_size, 128UL);
}

// Verifies scatter-gather on an IN endpoint requiring a trailing Zero-Length Packet (ZLP).
TEST_P(Dwc3EndpointsTest, ScatterGatherWithZlp) {
  const bool enqueue_many = GetParam();
  TriggerConnection();
  const uint8_t ep_address = 0x82;  // IN endpoint
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);
  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);

  RegisterVmo(1, 4096);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Total size is 256 + 256 = 512 (multiple of MPS) with short_bit set to true.
  frequest::BufferRegion region1;
  region1.buffer(frequest::Buffer::WithVmoId(1));
  region1.offset(0);
  region1.size(256);

  frequest::BufferRegion region2;
  region2.buffer(frequest::Buffer::WithVmoId(1));
  region2.offset(256);
  region2.size(256);

  std::vector<frequest::BufferRegion> regions;
  regions.reserve(2);
  regions.push_back(std::move(region1));
  regions.push_back(std::move(region2));

  frequest::Request req;
  req.data(std::move(regions));
  req.defer_completion(false);
  req.information(frequest::RequestInfo::WithBulk(frequest::BulkRequestInfo{}));
  req.short_(true);

  std::vector<frequest::Request> reqs;
  reqs.reserve(1);
  reqs.push_back(std::move(req));

  fit::result result = ep_client_->QueueRequests({std::move(reqs)});
  ASSERT_TRUE(result.is_ok()) << "QueueRequests failed: "
                              << result.error_value().FormatDescription();

  TransferState expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);
  WaitForActiveCount(ep_num, 1u);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    UserEndpoint& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 3u);

    const std::vector<dwc3_trb_t> trbs = uep.fifo.Read(3);
    ASSERT_EQ(trbs.size(), 3u);

    // TRB 0: first data chunk (chained).
    EXPECT_TRUE(trbs[0].control & TRB_HWO);
    EXPECT_TRUE(trbs[0].control & TRB_CHN);
    EXPECT_FALSE(trbs[0].control & TRB_IOC);
    EXPECT_EQ(TRB_BUFSIZ(trbs[0].status), 256u);

    // TRB 1: terminal data chunk (unchained before ZLP, with IOC).
    EXPECT_TRUE(trbs[1].control & TRB_HWO);
    EXPECT_FALSE(trbs[1].control & TRB_CHN);
    EXPECT_TRUE(trbs[1].control & TRB_IOC);
    EXPECT_EQ(TRB_BUFSIZ(trbs[1].status), 256u);

    // TRB 2: ZLP (zero length, with IOC).
    EXPECT_TRUE(trbs[2].control & TRB_HWO);
    EXPECT_FALSE(trbs[2].control & TRB_CHN);
    EXPECT_TRUE(trbs[2].control & TRB_IOC);
    if (!enqueue_many) {
      EXPECT_TRUE(trbs[2].control & TRB_LST);
    }
    EXPECT_EQ(TRB_BUFSIZ(trbs[2].status), 0u);
  });

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  TransferState expected_active_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_active_state);

  // Complete the chained data TD (TRB 0 + TRB 1).
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferInProgress(drv, ep_num); });
  dut_.runtime().RunUntilIdle();
  dut_.RunInDriverContext([&](Dwc3& drv) {
    UserEndpoint& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 1u);
    EXPECT_EQ(uep.server->active_reqs.size(), 1u);
  });
  EXPECT_EQ(event_handler_.completion_count(), 0u);

  // Complete TRB 2 (ZLP).
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
  EXPECT_EQ(completions[0].transfer_size, 512UL);
}

// Verifies that if the FIFO does not have enough available slots for a
// multi-region scatter-gather request, the FIFO is not mutated and the request
// stays in queued_reqs until space frees up.
TEST_P(Dwc3EndpointsTest, ScatterGatherRollbackOnFifoFull) {
  const bool enqueue_many = GetParam();
  TriggerConnection();
  const uint8_t ep_address = 0x82;  // IN endpoint
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);
  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);

  RegisterVmo(1, 4096);
  RegisterVmo(2, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // If enqueue_many is true, start the ongoing transfer first so that
  // UserEpQueueNextOngoing does not call uep.fifo.Reset() (which only happens
  // when start_transfer is true).
  if (enqueue_many) {
    QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
    WaitForState(ep_num, TransferState::kStartingOngoing);
    dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
    WaitForState(ep_num, TransferState::kActiveOngoing);
    dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferInProgress(drv, ep_num); });
    WaitForActiveCount(ep_num, 0u);
    std::vector<CompletionResult> init_completions = event_handler_.WaitForCompletions(1);
    ASSERT_EQ(init_completions.size(), 1UL);
    EXPECT_OK(init_completions[0].status);
  }

  dwc3_trb_t* remaining_slot = nullptr;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    UserEndpoint& uep = GetUserEndpoint(drv, ep_num);
    while (uep.fifo.AvailableSlots() > 1) {
      uep.fifo.AdvanceWrite();
    }
    EXPECT_EQ(uep.fifo.AvailableSlots(), 1u);
    dwc3_trb_t* fifo_base = uep.fifo.current_read() - uep.fifo.ReadOffset();
    remaining_slot =
        fifo_base + ((uep.fifo.ReadOffset() + uep.fifo.GetActiveCount()) % uep.fifo.TotalSlots());
    remaining_slot->control = 0u;
  });

  frequest::BufferRegion region1;
  region1.buffer(frequest::Buffer::WithVmoId(1));
  region1.offset(0);
  region1.size(512);

  frequest::BufferRegion region2;
  region2.buffer(frequest::Buffer::WithVmoId(2));
  region2.offset(0);
  region2.size(512);

  std::vector<frequest::BufferRegion> regions;
  regions.reserve(2);
  regions.push_back(std::move(region1));
  regions.push_back(std::move(region2));

  frequest::Request req;
  req.data(std::move(regions));
  req.defer_completion(false);
  req.information(frequest::RequestInfo::WithBulk(frequest::BulkRequestInfo{}));
  req.short_(false);

  std::vector<frequest::Request> reqs;
  reqs.reserve(1);
  reqs.push_back(std::move(req));

  fit::result result = ep_client_->QueueRequests({std::move(reqs)});
  ASSERT_TRUE(result.is_ok()) << "QueueRequests failed: "
                              << result.error_value().FormatDescription();
  WaitForQueuedCount(ep_num, 1u);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    UserEndpoint& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.server->queued_reqs.size(), 1u);
    EXPECT_EQ(uep.server->active_reqs.size(), 0u);
    EXPECT_EQ(uep.fifo.AvailableSlots(), 1u);
    EXPECT_EQ(remaining_slot->control, 0u);
  });

  // Clear dummy TRBs in the FIFO to free up slots and queue the request.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    UserEndpoint& uep = GetUserEndpoint(drv, ep_num);
    uep.fifo.Clear();
    EXPECT_GE(uep.fifo.AvailableSlots(), 2u);
    Dwc3TestHelper::UserEpQueueNext(drv, uep);
  });

  if (!enqueue_many) {
    WaitForState(ep_num, TransferState::kStartingSingle);
  }
  WaitForActiveCount(ep_num, 1u);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    UserEndpoint& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.server->queued_reqs.size(), 0u);
    EXPECT_EQ(uep.server->active_reqs.size(), 1u);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 2u);
  });

  if (!enqueue_many) {
    dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
    WaitForState(ep_num, TransferState::kActiveSingle);
  }

  // Complete the chained TD (TRB 0 + TRB 1) in a single event.
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
  EXPECT_EQ(completions[0].transfer_size, 1024UL);
}

// Verifies scatter-gather on a Bulk OUT endpoint where individual regions have
// non-MPS lengths whose total transfer size is a multiple of max_packet_size.
TEST_P(Dwc3EndpointsTest, ScatterGatherBulkOutTransfer) {
  const bool enqueue_many = GetParam();
  TriggerConnection();
  const uint8_t ep_address = 0x02;  // OUT endpoint
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);
  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);

  RegisterVmo(1, 4096);
  RegisterVmo(2, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue request with two regions (200B + 312B = 512B total).
  frequest::BufferRegion region1;
  region1.buffer(frequest::Buffer::WithVmoId(1));
  region1.offset(0);
  region1.size(200);

  frequest::BufferRegion region2;
  region2.buffer(frequest::Buffer::WithVmoId(2));
  region2.offset(0);
  region2.size(312);

  std::vector<frequest::BufferRegion> regions;
  regions.reserve(2);
  regions.push_back(std::move(region1));
  regions.push_back(std::move(region2));

  frequest::Request req;
  req.data(std::move(regions));
  req.defer_completion(false);
  req.information(frequest::RequestInfo::WithBulk(frequest::BulkRequestInfo{}));
  req.short_(false);

  std::vector<frequest::Request> reqs;
  reqs.reserve(1);
  reqs.push_back(std::move(req));

  fit::result result = ep_client_->QueueRequests({std::move(reqs)});
  ASSERT_TRUE(result.is_ok()) << "QueueRequests failed: "
                              << result.error_value().FormatDescription();

  TransferState expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);
  WaitForActiveCount(ep_num, 1u);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    UserEndpoint& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 2u);

    const std::vector<dwc3_trb_t> trbs = uep.fifo.Read(2);
    ASSERT_EQ(trbs.size(), 2u);

    // TRB 0 must have TRB_CHN set, and not TRB_IOC.
    EXPECT_TRUE(trbs[0].control & TRB_HWO);
    EXPECT_EQ(static_cast<bool>(trbs[0].control & TRB_CSP), enqueue_many);
    EXPECT_TRUE(trbs[0].control & TRB_CHN);
    EXPECT_FALSE(trbs[0].control & TRB_IOC);
    EXPECT_EQ(TRB_BUFSIZ(trbs[0].status), 200u);

    // TRB 1 must have TRB_IOC set, and not TRB_CHN.
    EXPECT_TRUE(trbs[1].control & TRB_HWO);
    EXPECT_EQ(static_cast<bool>(trbs[1].control & TRB_CSP), enqueue_many);
    EXPECT_FALSE(trbs[1].control & TRB_CHN);
    EXPECT_TRUE(trbs[1].control & TRB_IOC);
    if (!enqueue_many) {
      EXPECT_TRUE(trbs[1].control & TRB_LST);
    }
    EXPECT_EQ(TRB_BUFSIZ(trbs[1].status), 312u);
  });

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  TransferState expected_active_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_active_state);

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
  EXPECT_EQ(completions[0].transfer_size, 512UL);
}

// Verifies that when a Bulk OUT scatter-gather transfer receives an early short
// packet on an intermediate TRB, UserEpCompleteTransfers fast-forwards the FIFO
// past trailing TRBs of the request, clears TRB_HWO, and allows subsequent
// transfers to execute cleanly.
TEST_P(Dwc3EndpointsTest, ScatterGatherBulkOutShortPacketFastForwardsFifo) {
  const bool enqueue_many = GetParam();
  TriggerConnection();
  const uint8_t ep_address = 0x02;  // OUT endpoint
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);
  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);

  RegisterVmo(1, 4096);
  RegisterVmo(2, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  frequest::BufferRegion region1;
  region1.buffer(frequest::Buffer::WithVmoId(1));
  region1.offset(0);
  region1.size(512);

  frequest::BufferRegion region2;
  region2.buffer(frequest::Buffer::WithVmoId(2));
  region2.offset(0);
  region2.size(512);

  std::vector<frequest::BufferRegion> regions;
  regions.reserve(2);
  regions.push_back(std::move(region1));
  regions.push_back(std::move(region2));

  frequest::Request req;
  req.data(std::move(regions));
  req.defer_completion(false);
  req.information(frequest::RequestInfo::WithBulk(frequest::BulkRequestInfo{}));
  req.short_(false);

  std::vector<frequest::Request> reqs;
  reqs.reserve(1);
  reqs.push_back(std::move(req));

  fit::result result = ep_client_->QueueRequests({std::move(reqs)});
  ASSERT_TRUE(result.is_ok()) << "QueueRequests failed: "
                              << result.error_value().FormatDescription();

  TransferState expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);
  WaitForActiveCount(ep_num, 1u);

  dwc3_trb_t* trb1 = nullptr;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    UserEndpoint& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 2u);
    dwc3_trb_t* fifo_base = uep.fifo.current_read() - uep.fifo.ReadOffset();
    trb1 = fifo_base + ((uep.fifo.ReadOffset() + 1) % uep.fifo.TotalSlots());
    EXPECT_NE(trb1->control & TRB_HWO, 0u);
  });

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  TransferState expected_active_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_active_state);

  // Simulate a 256-byte short packet on TRB 0 (residual = 512 - 256 = 256).
  dut_.RunInDriverContext([&](Dwc3& drv) {
    if (enqueue_many) {
      TriggerEpTransferInProgress(drv, ep_num, 256);
    } else {
      TriggerEpTransferComplete(drv, ep_num, 256);
    }
  });

  if (enqueue_many) {
    WaitForActiveCount(ep_num, 0u);
  } else {
    WaitForState(ep_num, TransferState::kIdle);
  }

  std::vector<CompletionResult> completions1 = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions1.size(), 1UL);
  EXPECT_OK(completions1[0].status);
  EXPECT_EQ(completions1[0].transfer_size, 256UL);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    UserEndpoint& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 0u);
    EXPECT_TRUE(uep.server->active_reqs.empty());
    EXPECT_EQ(trb1->control & TRB_HWO, 0u);
  });

  // Queue and complete a subsequent 512-byte Bulk OUT request.
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  if (!enqueue_many) {
    WaitForState(ep_num, TransferState::kStartingSingle);
    dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
    WaitForState(ep_num, TransferState::kActiveSingle);
  }
  WaitForActiveCount(ep_num, 1u);

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

  std::vector<CompletionResult> completions2 = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions2.size(), 1UL);
  EXPECT_OK(completions2[0].status);
  EXPECT_EQ(completions2[0].transfer_size, 512UL);
}

// Verifies that when a Bulk OUT scatter-gather transfer receives a short packet
// whose length aligns with the exact size of an intermediate TRB (residual == 0
// with TRB_SPR set), UserEpCompleteTransfers still fast-forwards the FIFO past
// trailing TRBs of the request, clears TRB_HWO, and allows subsequent transfers
// to execute cleanly.
TEST_P(Dwc3EndpointsTest, ScatterGatherBulkOutShortPacketOnTrbBoundaryFastForwardsFifo) {
  const bool enqueue_many = GetParam();
  TriggerConnection();
  const uint8_t ep_address = 0x02;  // OUT endpoint
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);
  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);

  RegisterVmo(1, 4096);
  RegisterVmo(2, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  frequest::BufferRegion region1;
  region1.buffer(frequest::Buffer::WithVmoId(1));
  region1.offset(0);
  region1.size(200);

  frequest::BufferRegion region2;
  region2.buffer(frequest::Buffer::WithVmoId(2));
  region2.offset(0);
  region2.size(312);

  std::vector<frequest::BufferRegion> regions;
  regions.reserve(2);
  regions.push_back(std::move(region1));
  regions.push_back(std::move(region2));

  frequest::Request req;
  req.data(std::move(regions));
  req.defer_completion(false);
  req.information(frequest::RequestInfo::WithBulk(frequest::BulkRequestInfo{}));
  req.short_(false);

  std::vector<frequest::Request> reqs;
  reqs.reserve(1);
  reqs.push_back(std::move(req));

  fit::result result = ep_client_->QueueRequests({std::move(reqs)});
  ASSERT_TRUE(result.is_ok()) << "QueueRequests failed: "
                              << result.error_value().FormatDescription();

  TransferState expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);
  WaitForActiveCount(ep_num, 1u);

  dwc3_trb_t* trb1 = nullptr;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    UserEndpoint& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 2u);
    dwc3_trb_t* fifo_base = uep.fifo.current_read() - uep.fifo.ReadOffset();
    trb1 = fifo_base + ((uep.fifo.ReadOffset() + 1) % uep.fifo.TotalSlots());
    EXPECT_NE(trb1->control & TRB_HWO, 0u);
  });

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  TransferState expected_active_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_active_state);

  // Simulate a 200-byte short packet that fills TRB 0 completely (residual = 0,
  // TRB_SPR set).
  dut_.RunInDriverContext([&](Dwc3& drv) {
    if (enqueue_many) {
      TriggerEpTransferInProgress(drv, ep_num, /*residual=*/0, /*spr=*/true);
    } else {
      TriggerEpTransferComplete(drv, ep_num, /*residual=*/0, /*spr=*/true);
    }
  });

  if (enqueue_many) {
    WaitForActiveCount(ep_num, 0u);
  } else {
    WaitForState(ep_num, TransferState::kIdle);
  }

  std::vector<CompletionResult> completions1 = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions1.size(), 1UL);
  EXPECT_OK(completions1[0].status);
  EXPECT_EQ(completions1[0].transfer_size, 200UL);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    UserEndpoint& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 0u);
    EXPECT_TRUE(uep.server->active_reqs.empty());
    EXPECT_EQ(trb1->control & TRB_HWO, 0u);
  });

  // Queue and complete a subsequent 512-byte Bulk OUT request.
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  if (!enqueue_many) {
    WaitForState(ep_num, TransferState::kStartingSingle);
    dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
    WaitForState(ep_num, TransferState::kActiveSingle);
  }
  WaitForActiveCount(ep_num, 1u);

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

  std::vector<CompletionResult> completions2 = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions2.size(), 1UL);
  EXPECT_OK(completions2[0].status);
  EXPECT_EQ(completions2[0].transfer_size, 512UL);
}

// Verifies that a scatter-gather request requiring more TRBs than the total
// FIFO capacity (TotalSlots() - 1 = 254) is immediately rejected with
// ZX_ERR_OUT_OF_RANGE and popped from queued_reqs without wedging subsequent
// requests.
TEST_P(Dwc3EndpointsTest, ScatterGatherRejectsRequestExceedingTotalFifoCapacity) {
  const bool enqueue_many = GetParam();
  TriggerConnection();
  const uint8_t ep_address = 0x82;  // IN endpoint
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);
  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);

  RegisterVmo(1, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue a request with 255 regions (requiring 255 TRBs > TotalSlots() - 1).
  std::vector<frequest::BufferRegion> regions;
  regions.reserve(255);
  for (size_t i = 0; i < 255; ++i) {
    frequest::BufferRegion region;
    region.buffer(frequest::Buffer::WithVmoId(1));
    region.offset(0);
    region.size(512);
    regions.push_back(std::move(region));
  }

  frequest::Request req;
  req.data(std::move(regions));
  req.defer_completion(false);
  req.information(frequest::RequestInfo::WithBulk(frequest::BulkRequestInfo{}));
  req.short_(false);

  std::vector<frequest::Request> reqs;
  reqs.reserve(1);
  reqs.push_back(std::move(req));

  fit::result result = ep_client_->QueueRequests({std::move(reqs)});
  ASSERT_TRUE(result.is_ok()) << "QueueRequests failed: "
                              << result.error_value().FormatDescription();
  dut_.runtime().RunUntilIdle();

  std::vector<CompletionResult> completions1 = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions1.size(), 1UL);
  EXPECT_EQ(completions1[0].status, ZX_ERR_OUT_OF_RANGE);
  EXPECT_EQ(completions1[0].transfer_size, 0UL);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    UserEndpoint& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_TRUE(uep.server->queued_reqs.empty());
    EXPECT_TRUE(uep.server->active_reqs.empty());
    EXPECT_EQ(uep.fifo.GetActiveCount(), 0u);
  });

  // Verify that the endpoint is not wedged by queueing and completing a valid
  // 512B request.
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  TransferState expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);
  WaitForActiveCount(ep_num, 1u);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  TransferState expected_active_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_active_state);

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

  std::vector<CompletionResult> completions2 = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions2.size(), 1UL);
  EXPECT_OK(completions2[0].status);
  EXPECT_EQ(completions2[0].transfer_size, 512UL);
}

// Verifies that zero-length requests on OUT (Host-to-Device) endpoints are rejected with
// ZX_ERR_INVALID_ARGS to prevent hardware Babble FIFO overruns.
TEST_F(Dwc3EndpointsTestBase, RejectZeroLengthOutTransfer) {
  TriggerConnection();
  const uint8_t ep_address = 0x02;  // OUT endpoint
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);
  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);

  RegisterVmo(1, 4096);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue zero-length OUT transfer
  QueueRequest(1, 0, 0, fdescriptor::EndpointType::kBulk);
  dut_.runtime().RunUntilIdle();

  // 1. Verify completion status is ZX_ERR_INVALID_ARGS first (guarantees driver processed the
  // request)
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1UL);
  EXPECT_EQ(completions[0].status, ZX_ERR_INVALID_ARGS);

  // 2. Request should have been rejected BEFORE active_reqs or queued_reqs were populated
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_TRUE(uep.server->active_reqs.empty());
    EXPECT_TRUE(uep.server->queued_reqs.empty());
  });
}

// Verifies that when an early short packet terminates a multi-TRB transfer, any trailing
// unexecuted TRBs (such as appended ZLPs) are reclaimed and have TRB_HWO cleared.
// DISABLED: Requires trailing TRB reclamation on short packet logic in driver.
TEST_P(Dwc3EndpointsTest, DISABLED_ReapTrailingTrbsOnShortPacket) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x82;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue a request with short_bit = true, which will generate 2 TRBs.
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk, /*short_bit=*/true);

  TransferState expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);
  WaitForActiveCount(ep_num, 2u);

  dwc3_trb_t* current_trb = nullptr;
  dwc3_trb_t* stranded_trb = nullptr;

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 2u);
    current_trb = uep.fifo.current_read();
    dwc3_trb_t* fifo_base = current_trb - uep.fifo.ReadOffset();
    stranded_trb = fifo_base + ((uep.fifo.ReadOffset() + 1) % uep.fifo.TotalSlots());
  });

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  TransferState expected_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_state);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    // Clear ownership of the first TRB and simulate a short packet (e.g. 256 bytes residual)
    current_trb->control &= ~TRB_HWO;
    current_trb->status = TRB_BUFSIZ(256);
    EXPECT_OK(zx_cache_flush(current_trb, sizeof(*current_trb),
                             ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE));

    if (enqueue_many) {
      Dwc3TestHelper::HandleEpTransferInProgressEvent(drv, ep_num);
      uep.server->SendCompletions();
    } else {
      Dwc3TestHelper::HandleEpTransferCompleteEvent(drv, ep_num);
    }
  });
  dut_.runtime().RunUntilIdle();

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1UL);
  EXPECT_OK(completions[0].status);
  EXPECT_EQ(completions[0].transfer_size, 256UL);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 0u);
    // Properly flush/invalidate the stranded TRB to fetch from physical memory
    EXPECT_OK(zx_cache_flush(stranded_trb, sizeof(*stranded_trb),
                             ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE));

    // TRB_HWO must be cleared from the un-executed TRBs that were reaped back to software
    EXPECT_FALSE(stranded_trb->control & TRB_HWO);
  });
}

// Verifies that when an early short packet terminates a multi-TRB transfer, any trailing
// unexecuted TRBs are reclaimed and the subsequent queued request executes and completes cleanly.
// DISABLED: Requires trailing TRB reclamation on short packet logic in driver.
TEST_P(Dwc3EndpointsTest, DISABLED_ShortPacketReapsTrailingTrbsAndExecutesNextRequest) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x82;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);
  RegisterVmo(2, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue Request 1 (512B with short_bit = true, generating 2 TRBs).
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk, /*short_bit=*/true);

  // Queue Request 2 (512B, generating 1 TRB).
  QueueRequest(2, 0, 512, fdescriptor::EndpointType::kBulk, /*short_bit=*/false);

  TransferState expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);
  WaitForActiveCount(ep_num, enqueue_many ? 3u : 2u);

  dwc3_trb_t* first_trb = nullptr;
  dwc3_trb_t* stranded_trb = nullptr;
  dwc3_trb_t* second_req_trb = nullptr;

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), enqueue_many ? 3u : 2u);
    first_trb = uep.fifo.current_read();
    dwc3_trb_t* fifo_base = first_trb - uep.fifo.ReadOffset();
    stranded_trb = fifo_base + ((uep.fifo.ReadOffset() + 1) % uep.fifo.TotalSlots());
    if (enqueue_many) {
      second_req_trb = fifo_base + ((uep.fifo.ReadOffset() + 2) % uep.fifo.TotalSlots());
    }
  });

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  TransferState expected_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_state);

  // Simulate short packet on Request 1 (256 bytes transferred).
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    first_trb->control &= ~TRB_HWO;
    first_trb->status = TRB_BUFSIZ(256);
    EXPECT_OK(zx_cache_flush(first_trb, sizeof(*first_trb),
                             ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE));

    if (enqueue_many) {
      Dwc3TestHelper::HandleEpTransferInProgressEvent(drv, ep_num);
      uep.server->SendCompletions();
    } else {
      Dwc3TestHelper::HandleEpTransferCompleteEvent(drv, ep_num);
    }
  });
  dut_.runtime().RunUntilIdle();

  // Wait for Request 1 completion.
  std::vector<CompletionResult> completions1 = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions1.size(), 1UL);
  EXPECT_OK(completions1[0].status);
  EXPECT_EQ(completions1[0].transfer_size, 256UL);

  if (!enqueue_many) {
    WaitForState(ep_num, TransferState::kStartingSingle);
    dut_.RunInDriverContext(
        [&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId + 1); });
    WaitForState(ep_num, TransferState::kActiveSingle);
    dut_.RunInDriverContext([&](Dwc3& drv) {
      auto& uep = GetUserEndpoint(drv, ep_num);
      second_req_trb = uep.fifo.current_read();
    });
  }

  // Complete Request 2 (512 bytes).
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    second_req_trb->control &= ~TRB_HWO;
    second_req_trb->status = TRB_BUFSIZ(0);
    EXPECT_OK(zx_cache_flush(second_req_trb, sizeof(*second_req_trb),
                             ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE));

    if (enqueue_many) {
      Dwc3TestHelper::HandleEpTransferInProgressEvent(drv, ep_num);
      uep.server->SendCompletions();
    } else {
      Dwc3TestHelper::HandleEpTransferCompleteEvent(drv, ep_num);
    }
  });
  dut_.runtime().RunUntilIdle();

  // Wait for Request 2 completion.
  std::vector<CompletionResult> completions2 = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions2.size(), 1UL);
  EXPECT_OK(completions2[0].status);
  EXPECT_EQ(completions2[0].transfer_size, 512UL);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 0u);
    EXPECT_OK(zx_cache_flush(stranded_trb, sizeof(*stranded_trb),
                             ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE));
    EXPECT_FALSE(stranded_trb->control & TRB_HWO);
  });
}

// Verifies that a multi-TRB transfer where the host terminates with 0 bytes transferred
// reclaims all trailing TRBs and completes with ZX_OK and 0 bytes.
// DISABLED: Requires trailing TRB reclamation on short packet logic in driver.
TEST_P(Dwc3EndpointsTest, DISABLED_ShortPacketZeroBytesTransferredReapsTrailingTrbs) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x82;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue Request 1 (512B with short_bit = true -> 2 TRBs).
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk, /*short_bit=*/true);

  TransferState expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);
  WaitForActiveCount(ep_num, 2u);

  dwc3_trb_t* first_trb = nullptr;
  dwc3_trb_t* stranded_trb = nullptr;

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 2u);
    first_trb = uep.fifo.current_read();
    dwc3_trb_t* first = first_trb - uep.fifo.ReadOffset();
    stranded_trb = first + ((uep.fifo.ReadOffset() + 1) % uep.fifo.TotalSlots());
  });

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  TransferState expected_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_state);

  // Simulate 0 bytes transferred (residual = 512).
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    first_trb->control &= ~TRB_HWO;
    first_trb->status = TRB_BUFSIZ(512);
    EXPECT_OK(zx_cache_flush(first_trb, sizeof(*first_trb),
                             ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE));

    if (enqueue_many) {
      Dwc3TestHelper::HandleEpTransferInProgressEvent(drv, ep_num);
      uep.server->SendCompletions();
    } else {
      Dwc3TestHelper::HandleEpTransferCompleteEvent(drv, ep_num);
    }
  });
  dut_.runtime().RunUntilIdle();

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1UL);
  EXPECT_OK(completions[0].status);
  EXPECT_EQ(completions[0].transfer_size, 0UL);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 0u);
    EXPECT_OK(zx_cache_flush(stranded_trb, sizeof(*stranded_trb),
                             ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE));
    EXPECT_FALSE(stranded_trb->control & TRB_HWO);
  });
}

// Verifies that Bulk OUT TRBs do not set continuous stream flags.
// DISABLED: Requires omitting TRB_CSP on discrete Bulk OUT descriptors in driver.
TEST_P(Dwc3EndpointsTest, DISABLED_VerifyBulkOutTrbFormatting) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;  // Bulk OUT
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);

  TransferState expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  TransferState expected_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_state);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_GT(uep.fifo.GetActiveCount(), 0u);
    const auto& trbs = uep.fifo.Read(uep.fifo.GetActiveCount());

    for (const auto& trb : trbs) {
      EXPECT_EQ(trb.control & TRB_CSP, 0u);
    }

    auto* fifo_base = uep.fifo.current_read() - uep.fifo.ReadOffset();
    // TrbFifo::Init decrements last_ by 1 to reserve the final slot of the buffer allocation
    // for the Link TRB, so TotalSlots() (last_ - first_) returns the number of data slots (255).
    // The Link TRB resides at index 255 (fifo_base + TotalSlots()).
    auto* link_trb = fifo_base + uep.fifo.TotalSlots();

    EXPECT_EQ(TRB_TRBCTL(link_trb->control), static_cast<uint32_t>(TRB_TRBCTL_LINK));
    EXPECT_NE(link_trb->control & TRB_CHN, 0u);
  });
}

// Tests continuous multi-request streaming on ongoing Bulk endpoints, verifying that
// TransferInProgress events safely complete individual requests in FIFO order while
// the transfer state remains kActiveOngoing.
TEST_P(Dwc3EndpointsTest, OngoingBulk_MultiRequestStreamingAndTransferInProgress) {
  const bool enqueue_many = GetParam();
  if (!enqueue_many) {
    GTEST_SKIP() << "Ongoing transfers are only applicable for enqueue_many mode.";
  }
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue 3 requests with distinct sizes (multiples of max packet size for OUT endpoints)
  // to strictly verify FIFO retirement ordering.
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  QueueRequest(1, 512, 1024, fdescriptor::EndpointType::kBulk);
  QueueRequest(1, 1536, 1536, fdescriptor::EndpointType::kBulk);

  WaitForState(ep_num, TransferState::kStartingOngoing);
  WaitForQueuedCount(ep_num, 2u);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, TransferState::kActiveOngoing);
  WaitForActiveCount(ep_num, 3u);

  // Complete first request via TransferInProgress.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferInProgress(drv, ep_num); });
  WaitForActiveCount(ep_num, 2u);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kActiveOngoing);
    EXPECT_EQ(uep.ep.rsrc_id, kResourceId);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 2u);
    EXPECT_EQ(uep.server->active_reqs.size(), 2u);
    EXPECT_TRUE(uep.server->queued_reqs.empty());
  });

  std::vector<CompletionResult> completions1 = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions1.size(), 1u);
  EXPECT_OK(completions1[0].status);
  EXPECT_EQ(completions1[0].transfer_size, 512u);

  // Complete remaining 2 requests.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferInProgress(drv, ep_num); });
  WaitForActiveCount(ep_num, 1u);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferInProgress(drv, ep_num); });
  WaitForActiveCount(ep_num, 0u);

  std::vector<CompletionResult> completions2 = event_handler_.WaitForCompletions(2);
  ASSERT_EQ(completions2.size(), 2u);
  EXPECT_OK(completions2[0].status);
  EXPECT_EQ(completions2[0].transfer_size, 1024u);
  EXPECT_OK(completions2[1].status);
  EXPECT_EQ(completions2[1].transfer_size, 1536u);

  // State remains kActiveOngoing and resource ID is preserved even when active queue drains to 0.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kActiveOngoing);
    EXPECT_EQ(uep.ep.rsrc_id, kResourceId);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 0u);
    EXPECT_TRUE(uep.server->active_reqs.empty());
    EXPECT_TRUE(uep.server->queued_reqs.empty());
  });
}

// Tests that when active requests in an ongoing Bulk transfer drain to 0, subsequent requests
// are queued and updated via CmdEpUpdateTransfer without resetting to kIdle or re-starting.
TEST_P(Dwc3EndpointsTest, OngoingBulk_ActiveQueueDrainThenRequeueWithUpdateTransfer) {
  const bool enqueue_many = GetParam();
  if (!enqueue_many) {
    GTEST_SKIP() << "Ongoing transfers are only applicable for enqueue_many mode.";
  }
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  // Host sends Not Ready event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  WaitForState(ep_num, TransferState::kStartingOngoing);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, TransferState::kActiveOngoing);
  WaitForActiveCount(ep_num, 1u);

  // Drain active queue to 0.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferInProgress(drv, ep_num); });
  WaitForActiveCount(ep_num, 0u);

  std::vector<CompletionResult> completions1 = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions1.size(), 1u);
  EXPECT_OK(completions1[0].status);
  EXPECT_EQ(completions1[0].transfer_size, 512u);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kActiveOngoing);
    EXPECT_EQ(uep.ep.rsrc_id, kResourceId);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 0u);
    EXPECT_TRUE(uep.server->active_reqs.empty());
    EXPECT_TRUE(uep.server->queued_reqs.empty());
  });

  auto update_transfer_called = std::make_shared<std::atomic<bool>>(false);
  auto cleanup_callbacks = DeferClearDepcmdCallbacks(ep_num);

  dut_.RunInEnvironmentTypeContext([ep_num, update_transfer_called](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(ep_num).addr()];
    depcmd.SetWriteCallback([ep_num, update_transfer_called](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      if (DEPCMD::Get(ep_num).FromValue(val).CMDTYP() == DEPCMD::DEPUPDXFER) {
        update_transfer_called->store(true);
      }
    });
  });

  // Re-queue new request while endpoint is still active.
  QueueRequest(1, 512, 512, fdescriptor::EndpointType::kBulk);
  WaitForActiveCount(ep_num, 1u);
  EXPECT_TRUE(update_transfer_called->load());

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kActiveOngoing);
    EXPECT_EQ(uep.ep.rsrc_id, kResourceId);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 1u);
    EXPECT_EQ(uep.server->active_reqs.size(), 1u);
    EXPECT_TRUE(uep.server->queued_reqs.empty());
  });

  // Complete re-queued request.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferInProgress(drv, ep_num); });
  WaitForActiveCount(ep_num, 0u);

  std::vector<CompletionResult> completions2 = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions2.size(), 1u);
  EXPECT_OK(completions2[0].status);
  EXPECT_EQ(completions2[0].transfer_size, 512u);

  // Verify that the endpoint remains in kActiveOngoing with clean queues at the end.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kActiveOngoing);
    EXPECT_EQ(uep.ep.rsrc_id, kResourceId);
    EXPECT_EQ(uep.fifo.GetActiveCount(), 0u);
    EXPECT_TRUE(uep.server->active_reqs.empty());
    EXPECT_TRUE(uep.server->queued_reqs.empty());
  });
}

// Verifies that StopController halts active transfers and disables DALEPENA while the controller is
// still running, before ResetHw (CSFTRST) resets the core.
TEST_P(Dwc3EndpointsTest, StopControllerResetsEndpointsBeforeResettingHardware) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  TransferState expected_starting =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  TransferState expected_active =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_active);

  auto dctl_written = std::make_shared<std::atomic<bool>>(false);
  auto dalepena_cleared_before_dctl = std::make_shared<std::atomic<bool>>(false);
  auto dalepena_written_after_dctl = std::make_shared<std::atomic<bool>>(false);

  vbus_high_.store(false);
  auto cleanup_dctl = DeferClearDctlCallback();
  SetDctlCallback([dctl_written]() { dctl_written->store(true); });

  dut_.RunInEnvironmentTypeContext([dctl_written, dalepena_cleared_before_dctl,
                                    dalepena_written_after_dctl, ep_num](Environment& env) {
    auto& dalepena = env.reg_region()[DALEPENA::Get().addr()];
    dalepena.SetWriteCallback([dctl_written, dalepena_cleared_before_dctl,
                               dalepena_written_after_dctl, ep_num](uint64_t val_raw) {
      uint32_t val = static_cast<uint32_t>(val_raw);
      if (dctl_written->load()) {
        dalepena_written_after_dctl->store(true);
      } else if ((val & (1u << ep_num)) == 0) {
        dalepena_cleared_before_dctl->store(true);
      }
    });
  });

  auto cleanup_dalepena = fit::defer([this]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[DALEPENA::Get().addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  fidl::WireResult stop_res = dci_->StopController();
  ASSERT_OK(stop_res.status());
  ASSERT_TRUE(stop_res.value().is_ok());

  EXPECT_TRUE(dctl_written->load());
  EXPECT_TRUE(dalepena_cleared_before_dctl->load());
  EXPECT_FALSE(dalepena_written_after_dctl->load());

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1u);
  EXPECT_EQ(completions[0].status, ZX_ERR_IO_NOT_PRESENT);
}

// Verifies that if DEPEVT_XFER_COMPLETE arrives while an endpoint is in kCanceling (waiting for
// DEPENDXFER command completion), HandleEpTransferCompleteEvent harvests the completed TRB without
// clobbering transfer_state to kIdle or starting queued requests before HandleEpTransferEndedEvent.
TEST_P(Dwc3EndpointsTest,
       TransferCompleteDuringCancelingPreservesCancelingStateUntilTransferEnded) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);
  RegisterVmo(2, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  TransferState expected_starting =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  TransferState expected_active =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_active);

  auto dalepena_disabled = std::make_shared<std::atomic<bool>>(false);
  dut_.RunInEnvironmentTypeContext([dalepena_disabled, ep_num](Environment& env) {
    env.reg_region()[DALEPENA::Get().addr()].SetWriteCallback(
        [dalepena_disabled, ep_num](uint64_t val_raw) {
          if ((static_cast<uint32_t>(val_raw) & (1u << ep_num)) == 0) {
            dalepena_disabled->store(true);
          }
        });
  });
  auto cleanup_dalepena = fit::defer([this]() {
    dut_.RunInEnvironmentTypeContext([](Environment& env) {
      env.reg_region()[DALEPENA::Get().addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  // Disable endpoint so it enters kCanceling while DEPENDXFER is in flight.
  fidl::WireResult disable_res = dci_->DisableEndpoint(ep_address);
  ASSERT_OK(disable_res.status());
  ASSERT_TRUE(disable_res.value().is_ok());
  WaitForState(ep_num, TransferState::kCanceling);
  EXPECT_FALSE(dalepena_disabled->load());

  // Simulate DEPEVT_XFER_COMPLETE arriving in the event ring right before DEPEVT_CMD_CMPLT.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    dwc3_trb_t* trb = uep.fifo.current_read();
    trb->control &= ~TRB_HWO;
    trb->status = TRB_BUFSIZ(0);
    Dwc3TestHelper::HandleEpTransferCompleteEvent(drv, ep_num);
    uep.server->SendCompletions();

    // Endpoint must remain in kCanceling until HandleEpTransferEndedEvent arrives.
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kCanceling);
  });

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1u);
  EXPECT_OK(completions[0].status);
  EXPECT_EQ(completions[0].transfer_size, 512u);

  // Now deliver the DEPENDXFER command completion event.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferEnded(drv, ep_num); });
  WaitForState(ep_num, TransferState::kIdle);
  EXPECT_TRUE(dalepena_disabled->load());
}

TEST_P(Dwc3EndpointsTest, ConfigureEndpointWhileCancelingWaitsForTransferEnded) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);
  RegisterVmo(2, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  WaitForActiveCount(ep_num, 1);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle);

  // DisableEndpoint places the endpoint into kCanceling with enabled == false.
  fidl::WireResult disable_res = dci_->DisableEndpoint(ep_address);
  ASSERT_OK(disable_res.status());
  ASSERT_TRUE(disable_res.value().is_ok());
  WaitForState(ep_num, TransferState::kCanceling);

  // Issue ConfigureEndpoint while DEPEVT_CMD_CMPLT (HandleEpTransferEndedEvent) is still in flight.
  // ConfigureEndpoint must wait for the in-flight cancellation to complete before re-initializing
  // the TRB FIFO and enabling the endpoint.
  fdescriptor::wire::UsbEndpointDescriptor desc{
      .b_length = sizeof(fdescriptor::wire::UsbEndpointDescriptor),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = ep_address,
      .bm_attributes = static_cast<uint8_t>(fdescriptor::EndpointType::kBulk),
      .w_max_packet_size = 512,
      .b_interval = 0,
  };
  std::atomic<bool> configure_done{false};
  std::thread config_thread([&]() {
    fidl::WireResult config_res = dci_->ConfigureEndpoint(desc, {});
    EXPECT_OK(config_res.status());
    EXPECT_TRUE(config_res.value().is_ok());
    configure_done.store(true);
  });

  // Wait until ConfigureEndpoint has registered its on_idle_callbacks continuation.
  dut_.runtime().RunUntil([&]() {
    return dut_.RunInDriverContext<bool>([&](Dwc3& drv) {
      auto& uep = GetUserEndpoint(drv, ep_num);
      return !uep.on_idle_callbacks.empty();
    });
  });
  EXPECT_FALSE(configure_done.load());
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kCanceling);
    EXPECT_FALSE(uep.ep.enabled);
  });

  // Deliver the DEPENDXFER completion event; this completes the canceled request and unblocks
  // ConfigureEndpoint.
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferEnded(drv, ep_num); });
  config_thread.join();
  EXPECT_TRUE(configure_done.load());

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1u);
  EXPECT_EQ(completions[0].status, ZX_ERR_IO_NOT_PRESENT);

  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kIdle);
    EXPECT_TRUE(uep.ep.enabled);
  });
}

TEST_P(Dwc3EndpointsTest, RemoveRequestsWhileCancelingDrainsAndIgnoresStaleTransferEnded) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);
  RegisterVmo(2, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  WaitForActiveCount(ep_num, 1);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle);

  // DisableEndpoint places the endpoint into kCanceling with enabled == false.
  fidl::WireResult disable_res = dci_->DisableEndpoint(ep_address);
  ASSERT_OK(disable_res.status());
  ASSERT_TRUE(disable_res.value().is_ok());
  WaitForState(ep_num, TransferState::kCanceling);

  // Teardown (RemoveRequests, as run by StopActiveTransfers()) reaches the endpoint before
  // DEPEVT_CMD_CMPLT arrives; this returns the request immediately and increments
  // stale_cancels_to_ignore.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    Dwc3TestHelper::RemoveRequests(drv, uep, ZX_ERR_IO_NOT_PRESENT);
    Dwc3TestHelper::EpReset(drv, uep.ep);
  });

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1u);
  EXPECT_EQ(completions[0].status, ZX_ERR_IO_NOT_PRESENT);
  WaitForState(ep_num, TransferState::kIdle);

  // Re-configure the endpoint and start a new transfer, then begin canceling it BEFORE the stale
  // DEPEVT_CMD_CMPLT from the prior cancellation arrives.
  fdescriptor::wire::UsbEndpointDescriptor desc{
      .b_length = sizeof(fdescriptor::wire::UsbEndpointDescriptor),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = ep_address,
      .bm_attributes = static_cast<uint8_t>(fdescriptor::EndpointType::kBulk),
      .w_max_packet_size = 512,
      .b_interval = 0,
  };
  fidl::WireResult config_res = dci_->ConfigureEndpoint(desc, {});
  ASSERT_OK(config_res.status());
  ASSERT_TRUE(config_res.value().is_ok());

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();
  QueueRequest(2, 0, 512, fdescriptor::EndpointType::kBulk);
  WaitForActiveCount(ep_num, 1);
  dut_.RunInDriverContext(
      [&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId + 1); });
  WaitForState(ep_num, enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle);

  fidl::WireResult disable_res2 = dci_->DisableEndpoint(ep_address);
  ASSERT_OK(disable_res2.status());
  ASSERT_TRUE(disable_res2.value().is_ok());
  WaitForState(ep_num, TransferState::kCanceling);

  // The first DEPEVT_CMD_CMPLT is the stale event from the old cancellation and must be ignored via
  // stale_cancels_to_ignore, leaving the new cancellation in kCanceling.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    TriggerEpTransferEnded(drv, ep_num);
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kCanceling);
    EXPECT_EQ(uep.server->active_reqs.size(), 1u);
  });

  // The second DEPEVT_CMD_CMPLT completes the new cancellation.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    TriggerEpTransferEnded(drv, ep_num);
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kIdle);
    EXPECT_FALSE(uep.ep.enabled);

    // Also verify EpReset when power_on_ is false clears software flags.
    Dwc3TestHelper::SetPowerOn(drv, false);
    uep.ep.stalled = true;
    uep.ep.got_not_ready = true;
    Dwc3TestHelper::EpReset(drv, uep.ep);
    EXPECT_FALSE(uep.ep.enabled);
    EXPECT_FALSE(uep.ep.stalled);
    EXPECT_FALSE(uep.ep.got_not_ready);
    Dwc3TestHelper::SetPowerOn(drv, true);
  });

  std::vector<CompletionResult> completions2 = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions2.size(), 1u);
  EXPECT_EQ(completions2[0].status, ZX_ERR_IO_NOT_PRESENT);
}

TEST_P(Dwc3EndpointsTest, HandleResetEventWhileActiveWaitsForEndpointIdleBeforeEp0Start) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  WaitForActiveCount(ep_num, 1);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle);

  // Trigger HandleResetEvent while ep_num is actively transferring.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::HandleResetEvent(drv);
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kCanceling);
    // Ep0Start() must wait until all user endpoints finish canceling and reach kIdle.
    EXPECT_FALSE(Dwc3TestHelper::IsEpEnabled(drv, 0));
    EXPECT_FALSE(Dwc3TestHelper::IsEpEnabled(drv, 1));
  });

  // Once DEPEVT_CMD_CMPLT arrives for ep_num, it transitions to kIdle and Ep0Start() runs.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    TriggerEpTransferEnded(drv, ep_num);
    EXPECT_TRUE(Dwc3TestHelper::IsXferIdle(drv, ep_num));
    EXPECT_TRUE(Dwc3TestHelper::IsEpEnabled(drv, 0));
    EXPECT_TRUE(Dwc3TestHelper::IsEpEnabled(drv, 1));
  });

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1u);
  EXPECT_EQ(completions[0].status, ZX_ERR_IO_NOT_PRESENT);
}

TEST_P(Dwc3EndpointsTest, HandleResetEventWhenEndTransferTimesOutDrainsActiveRequestsAndStartsEp0) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  WaitForActiveCount(ep_num, 1);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle);

  // Simulate DEPCMD on ep_num timing out with CMDACT (bit 10) stuck at 1 during HandleResetEvent,
  // while EP0 (0 and 1) commands succeed normally.
  auto ep_depcmd_writes = std::make_shared<std::atomic<uint32_t>>(0);
  auto ep_depcmd_val = std::make_shared<std::atomic<uint32_t>>(0);
  auto dalepena_val =
      std::make_shared<std::atomic<uint32_t>>((1u << 0) | (1u << 1) | (1u << ep_num));
  auto ep_dalepena_disabled = std::make_shared<std::atomic<bool>>(false);
  dut_.RunInEnvironmentTypeContext([ep_num, ep_depcmd_writes, ep_depcmd_val, dalepena_val,
                                    ep_dalepena_disabled](Environment& env) {
    auto& reg = env.reg_region()[DEPCMD::Get(ep_num).addr()];
    reg.SetReadCallback([ep_depcmd_val]() -> uint64_t { return ep_depcmd_val->load(); });
    reg.SetWriteCallback([ep_depcmd_writes, ep_depcmd_val](uint64_t val) {
      ep_depcmd_writes->fetch_add(1);
      ep_depcmd_val->store(static_cast<uint32_t>(val));
    });
    auto& dalepena_reg = env.reg_region()[DALEPENA::Get().addr()];
    dalepena_reg.SetReadCallback([dalepena_val]() -> uint64_t { return dalepena_val->load(); });
    dalepena_reg.SetWriteCallback(
        [ep_num, ep_depcmd_writes, dalepena_val, ep_dalepena_disabled](uint64_t v) {
          uint32_t val = static_cast<uint32_t>(v);
          dalepena_val->store(val);
          if (ep_depcmd_writes->load() > 0 && (val & (1u << ep_num)) == 0) {
            ep_dalepena_disabled->store(true);
          }
        });
  });
  auto cleanup_depcmd = fit::defer([this, ep_num]() {
    dut_.RunInEnvironmentTypeContext([ep_num](Environment& env) {
      auto& reg = env.reg_region()[DEPCMD::Get(ep_num).addr()];
      reg.SetReadCallback([]() -> uint64_t { return 0; });
      reg.SetWriteCallback([](uint64_t) {});
      env.reg_region()[DALEPENA::Get().addr()].SetReadCallback([]() -> uint64_t { return 0; });
      env.reg_region()[DALEPENA::Get().addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  // Trigger HandleResetEvent while DEPCMD(ep_num).CMDACT stays stuck at 1.
  dut_.RunInDriverContext([](Dwc3& drv) { Dwc3TestHelper::HandleResetEvent(drv); });
  dut_.runtime().RunUntilIdle();

  // DALEPENA must not be cleared for ep_num while DEPCMD(ep_num).CMDACT is stuck at 1,
  // ep_num must immediately transition to kIdle and drain its active request in software,
  // and Ep0Start() must still proceed normally and enable EP0 (endpoints 0 and 1).
  EXPECT_EQ(ep_depcmd_writes->load(), 1u);
  EXPECT_FALSE(ep_dalepena_disabled->load());
  dut_.RunInDriverContext([ep_num](Dwc3& drv) {
    ASSERT_TRUE(Dwc3TestHelper::IsXferIdle(drv, ep_num));
    EXPECT_TRUE(Dwc3TestHelper::IsEpEnabled(drv, 0));
    EXPECT_TRUE(Dwc3TestHelper::IsEpEnabled(drv, 1));
  });

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1u);
  EXPECT_EQ(completions[0].status, ZX_ERR_IO_NOT_PRESENT);
}

// No UsbDciInterface is bound here, so there is no usb-peripheral to unconfigure the function
// owning ep_num: the disconnect must fall back to ending the transfer synchronously
// (StopActiveTransfers()) before the core is reset, and must still return the request.
TEST_P(Dwc3EndpointsTest, DisconnectWithoutDciEndsLeftoverTransferSynchronouslyAndClearsDalepena) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  WaitForActiveCount(ep_num, 1);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle);

  auto ep_depcmd_writes = std::make_shared<std::atomic<uint32_t>>(0);
  auto ep_depcmd_last_val = std::make_shared<std::atomic<uint32_t>>(0);
  auto dalepena_val =
      std::make_shared<std::atomic<uint32_t>>((1u << 0) | (1u << 1) | (1u << ep_num));
  auto ep_dalepena_disabled_after_end_transfer = std::make_shared<std::atomic<bool>>(false);
  dut_.RunInEnvironmentTypeContext([ep_num, ep_depcmd_writes, ep_depcmd_last_val, dalepena_val,
                                    ep_dalepena_disabled_after_end_transfer](Environment& env) {
    auto& reg = env.reg_region()[DEPCMD::Get(ep_num).addr()];
    reg.SetReadCallback([]() -> uint64_t { return 0; });
    reg.SetWriteCallback([ep_depcmd_writes, ep_depcmd_last_val](uint64_t val) {
      ep_depcmd_writes->fetch_add(1);
      ep_depcmd_last_val->store(static_cast<uint32_t>(val));
    });
    auto& dalepena_reg = env.reg_region()[DALEPENA::Get().addr()];
    dalepena_reg.SetReadCallback([dalepena_val]() -> uint64_t { return dalepena_val->load(); });
    dalepena_reg.SetWriteCallback([ep_num, ep_depcmd_writes, dalepena_val,
                                   ep_dalepena_disabled_after_end_transfer](uint64_t v) {
      uint32_t val = static_cast<uint32_t>(v);
      dalepena_val->store(val);
      if (ep_depcmd_writes->load() > 0 && (val & (1u << ep_num)) == 0) {
        ep_dalepena_disabled_after_end_transfer->store(true);
      }
    });
  });
  auto cleanup_depcmd = fit::defer([this, ep_num]() {
    dut_.RunInEnvironmentTypeContext([ep_num](Environment& env) {
      auto& reg = env.reg_region()[DEPCMD::Get(ep_num).addr()];
      reg.SetReadCallback([]() -> uint64_t { return 0; });
      reg.SetWriteCallback([](uint64_t) {});
      env.reg_region()[DALEPENA::Get().addr()].SetReadCallback([]() -> uint64_t { return 0; });
      env.reg_region()[DALEPENA::Get().addr()].SetWriteCallback([](uint64_t) {});
    });
  });

  // Trigger disconnect while ep_num is actively transferring.
  dut_.RunInEnvironmentTypeContext([](Environment& env) { env.usb_phy().completion()->Reset(); });
  dut_.RunInEnvironmentTypeContext(
      [](Environment& env) { env.usb_phy().TriggerConnection(false); });
  dut_.runtime().RunUntilIdle();
  ASSERT_OK(WaitForPhy());

  // Verify DEPCMD::DEPENDXFER was issued synchronously (CMDIOC == 0) and DALEPENA bit was
  // cleared immediately while power_on_ was still true.
  EXPECT_EQ(ep_depcmd_writes->load(), 1u);
  auto depcmd = DEPCMD::Get(ep_num).FromValue(ep_depcmd_last_val->load());
  EXPECT_EQ(depcmd.CMDTYP(), DEPCMD::DEPENDXFER);
  EXPECT_EQ(depcmd.CMDIOC(), 0u);
  EXPECT_TRUE(ep_dalepena_disabled_after_end_transfer->load());

  dut_.RunInDriverContext([ep_num](Dwc3& drv) {
    EXPECT_TRUE(Dwc3TestHelper::IsXferIdle(drv, ep_num));
    EXPECT_FALSE(Dwc3TestHelper::IsEpEnabled(drv, ep_num));
  });

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1u);
  EXPECT_EQ(completions[0].status, ZX_ERR_IO_NOT_PRESENT);
}

// Cable unplug with usb-peripheral bound while the function driver's endpoint is still
// transferring. The link is already gone, so dwc3 ends the transfer synchronously (DEPENDXFER with
// CMDIOC=0), returns the request and resets the core *before* asking usb-peripheral to disconnect.
// The core stays powered for that drain but is_active() is false, so the function driver's
// CancelAll and DisableEndpoint complete immediately against the idle endpoint instead of waiting
// for an event the dead link can no longer deliver. Only once SetConnected(false) completes is the
// core powered down, the PHY told and the connection watcher re-armed.
TEST_P(Dwc3EndpointsTest, DisconnectEndsTransferBeforeDciDrainAndCancelAllRepliesImmediately) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  WaitForActiveCount(ep_num, 1);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle);

  // Stand in for usb-peripheral: hold SetConnected(false) until the test releases it.
  FakeUsbDciInterface fake_dci;
  fake_dci.SetDeferSetConnectedFalse(true);
  libsync::Completion set_connected_false_received;
  fake_dci.SetSetConnectedCallback([&](bool connected) {
    if (!connected) {
      set_connected_false_received.Signal();
    }
  });
  std::optional<fidl::ServerBindingRef<fdci::UsbDciInterface>> dci_binding;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto [client_end, server_end] = fidl::Endpoints<fdci::UsbDciInterface>::Create();
    dci_binding = fidl::BindServer(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                   std::move(server_end), &fake_dci);
    Dwc3TestHelper::BindDciInterface(drv, std::move(client_end));
  });
  auto unbind_dci = fit::defer([&]() {
    dut_.RunInDriverContext([&](Dwc3& drv) { dci_binding->Unbind(); });
    dut_.runtime().RunUntilIdle();
  });

  // Observe DEPENDXFER commands on ep_num and core soft resets.
  auto end_transfer_writes = std::make_shared<std::atomic<uint32_t>>(0);
  auto end_transfer_last_val = std::make_shared<std::atomic<uint32_t>>(0);
  auto reset_count = std::make_shared<std::atomic<uint32_t>>(0);
  dut_.RunInEnvironmentTypeContext(
      [this, ep_num, end_transfer_writes, end_transfer_last_val, reset_count](Environment& env) {
        auto& depcmd = env.reg_region()[DEPCMD::Get(ep_num).addr()];
        depcmd.SetReadCallback([]() -> uint64_t { return 0; });
        depcmd.SetWriteCallback([ep_num, end_transfer_writes, end_transfer_last_val](uint64_t val) {
          if (DEPCMD::Get(ep_num).FromValue(static_cast<uint32_t>(val)).CMDTYP() ==
              DEPCMD::DEPENDXFER) {
            end_transfer_writes->fetch_add(1);
            end_transfer_last_val->store(static_cast<uint32_t>(val));
          }
        });
        env.reg_region()[DCTL::Get().addr()].SetWriteCallback([this, reset_count](uint64_t val) {
          if (DCTL::Get().FromValue(static_cast<uint32_t>(val)).CSFTRST() == 1) {
            reset_count->fetch_add(1);
          }
          Write_DCTL(static_cast<uint32_t>(val));
        });
      });
  auto restore_regs = fit::defer([this, ep_num]() {
    dut_.RunInEnvironmentTypeContext([this, ep_num](Environment& env) {
      auto& depcmd = env.reg_region()[DEPCMD::Get(ep_num).addr()];
      depcmd.SetReadCallback([]() -> uint64_t { return 0; });
      depcmd.SetWriteCallback([](uint64_t) {});
      env.reg_region()[DCTL::Get().addr()].SetWriteCallback(
          [this](uint64_t val) { Write_DCTL(static_cast<uint32_t>(val)); });
    });
  });

  size_t disconnected_reports_before = 0;
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    disconnected_reports_before = env.usb_phy().disconnected_reports();
    env.usb_phy().completion()->Reset();
  });

  // Unplug: the PHY reports disconnected.
  dut_.RunInEnvironmentTypeContext(
      [](Environment& env) { env.usb_phy().TriggerConnection(false); });
  dut_.runtime().RunUntilIdle();
  ASSERT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() { return set_connected_false_received.signaled(); }, zx::duration::infinite()));
  dut_.runtime().RunUntilIdle();
  ASSERT_TRUE(fake_dci.has_deferred_set_connected());

  // Before usb-peripheral was even asked, the transfer was ended synchronously (CMDIOC=0), the
  // request returned and -- without an external reset controller -- the core soft-reset. The core
  // stays powered for the drain, but is_active() is false; the PHY has not been told and the
  // watcher has not been re-armed.
  EXPECT_EQ(end_transfer_writes->load(), 1u);
  EXPECT_EQ(DEPCMD::Get(ep_num).FromValue(end_transfer_last_val->load()).CMDIOC(), 0u);
  EXPECT_EQ(reset_count->load(), 1u);
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1u);
  EXPECT_EQ(completions[0].status, ZX_ERR_IO_NOT_PRESENT);
  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_TRUE(Dwc3TestHelper::GetPowerOn(drv));
    EXPECT_FALSE(Dwc3TestHelper::IsActive(drv));
    EXPECT_TRUE(Dwc3TestHelper::IsXferIdle(drv, ep_num));
    EXPECT_FALSE(Dwc3TestHelper::IsEpEnabled(drv, ep_num));
  });
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    EXPECT_EQ(env.usb_phy().disconnected_reports(), disconnected_reports_before);
    EXPECT_FALSE(env.usb_phy().has_completer());
  });

  // The function driver unconfigures during the drain: CancelAll replies at once instead of
  // parking in cancel_completers, DisableEndpoint succeeds, and no further DEPENDXFER is issued.
  auto cancel_replied = std::make_shared<std::atomic<bool>>(false);
  auto cancel_ok = std::make_shared<std::atomic<bool>>(false);
  ep_client_->CancelAll().Then(
      [cancel_replied, cancel_ok](fidl::Result<fendpoint::Endpoint::CancelAll>& res) {
        cancel_ok->store(res.is_ok());
        cancel_replied->store(true);
      });
  ASSERT_TRUE(dut_.runtime().RunWithTimeoutOrUntil([&]() { return cancel_replied->load(); },
                                                   zx::duration::infinite()));
  EXPECT_TRUE(cancel_ok->load());
  fidl::WireResult disable_res = dci_->DisableEndpoint(ep_address);
  ASSERT_OK(disable_res.status());
  EXPECT_TRUE(disable_res.value().is_ok());
  EXPECT_EQ(end_transfer_writes->load(), 1u);
  EXPECT_EQ(reset_count->load(), 1u);
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    ASSERT_TRUE(uep.server.has_value());
    EXPECT_TRUE(uep.server->cancel_completers.empty());
  });

  // usb-peripheral is done: the core is powered down, the connection lease released, and only
  // then is the PHY told and the hanging get re-armed.
  dut_.RunInDriverContext([&](Dwc3& drv) { fake_dci.CompleteDeferredSetConnected(ZX_OK); });
  dut_.runtime().RunUntilIdle();
  ASSERT_OK(WaitForPhy());

  EXPECT_EQ(end_transfer_writes->load(), 1u);
  EXPECT_EQ(reset_count->load(), 1u);
  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_FALSE(Dwc3TestHelper::GetPowerOn(drv));
    EXPECT_FALSE(Dwc3TestHelper::IsEpEnabled(drv, ep_num));
    EXPECT_TRUE(Dwc3TestHelper::IsXferIdle(drv, ep_num));
  });
  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        size_t reports = 0;
        dut_.RunInEnvironmentTypeContext(
            [&](Environment& env) { reports = env.usb_phy().disconnected_reports(); });
        return reports == disconnected_reports_before + 1;
      },
      zx::duration::infinite()));
}

// Cable unplug while a DEPSTRTXFER is still in flight (kStartingSingle / kStartingOngoing, no
// resource id yet). There is nothing to end in hardware: the request is returned right away and
// the stale start is recorded, then cleared again by StopEvents() since the event ring is stopped
// and the core reset before the start event could ever be consumed. The function driver's
// CancelAll / DisableEndpoint during the drain still reply immediately, and -- the part that would
// hang if the stale count leaked -- the endpoint can be configured again on the next connection.
TEST_P(Dwc3EndpointsTest, DisconnectWhileStartingReturnsRequestAndClearsStaleStartForReconnect) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue a request but never deliver DEPEVT_XFER_STARTED: the endpoint stays in kStarting*.
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  WaitForActiveCount(ep_num, 1);
  WaitForState(ep_num,
               enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle);

  FakeUsbDciInterface fake_dci;
  fake_dci.SetDeferSetConnectedFalse(true);
  libsync::Completion set_connected_false_received;
  fake_dci.SetSetConnectedCallback([&](bool connected) {
    if (!connected) {
      set_connected_false_received.Signal();
    }
  });
  std::optional<fidl::ServerBindingRef<fdci::UsbDciInterface>> dci_binding;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto [client_end, server_end] = fidl::Endpoints<fdci::UsbDciInterface>::Create();
    dci_binding = fidl::BindServer(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                   std::move(server_end), &fake_dci);
    Dwc3TestHelper::BindDciInterface(drv, std::move(client_end));
  });
  auto unbind_dci = fit::defer([&]() {
    dut_.RunInDriverContext([&](Dwc3& drv) { dci_binding->Unbind(); });
    dut_.runtime().RunUntilIdle();
  });

  auto end_transfer_writes = std::make_shared<std::atomic<uint32_t>>(0);
  dut_.RunInEnvironmentTypeContext([ep_num, end_transfer_writes](Environment& env) {
    auto& depcmd = env.reg_region()[DEPCMD::Get(ep_num).addr()];
    depcmd.SetReadCallback([]() -> uint64_t { return 0; });
    depcmd.SetWriteCallback([ep_num, end_transfer_writes](uint64_t val) {
      if (DEPCMD::Get(ep_num).FromValue(static_cast<uint32_t>(val)).CMDTYP() ==
          DEPCMD::DEPENDXFER) {
        end_transfer_writes->fetch_add(1);
      }
    });
  });
  auto restore_regs = fit::defer([this, ep_num]() {
    dut_.RunInEnvironmentTypeContext([ep_num](Environment& env) {
      auto& depcmd = env.reg_region()[DEPCMD::Get(ep_num).addr()];
      depcmd.SetReadCallback([]() -> uint64_t { return 0; });
      depcmd.SetWriteCallback([](uint64_t) {});
    });
  });

  dut_.RunInEnvironmentTypeContext([](Environment& env) { env.usb_phy().completion()->Reset(); });
  dut_.RunInEnvironmentTypeContext(
      [](Environment& env) { env.usb_phy().TriggerConnection(false); });
  dut_.runtime().RunUntilIdle();
  ASSERT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() { return set_connected_false_received.signaled(); }, zx::duration::infinite()));
  dut_.runtime().RunUntilIdle();
  ASSERT_TRUE(fake_dci.has_deferred_set_connected());

  // No resource id, so no DEPENDXFER: the request is simply returned, the endpoint is idle and
  // disabled, and the stale start recorded by RemoveRequests() has already been cleared by
  // StopEvents().
  EXPECT_EQ(end_transfer_writes->load(), 0u);
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1u);
  EXPECT_EQ(completions[0].status, ZX_ERR_IO_NOT_PRESENT);
  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_TRUE(Dwc3TestHelper::GetPowerOn(drv));
    EXPECT_FALSE(Dwc3TestHelper::IsActive(drv));
    EXPECT_TRUE(Dwc3TestHelper::IsXferIdle(drv, ep_num));
    EXPECT_FALSE(Dwc3TestHelper::IsEpEnabled(drv, ep_num));
    EXPECT_EQ(GetUserEndpoint(drv, ep_num).ep.stale_starts_to_end, 0u);
  });

  // The function driver unconfigures during the drain without waiting for any event.
  auto cancel_replied = std::make_shared<std::atomic<bool>>(false);
  auto cancel_ok = std::make_shared<std::atomic<bool>>(false);
  ep_client_->CancelAll().Then(
      [cancel_replied, cancel_ok](fidl::Result<fendpoint::Endpoint::CancelAll>& res) {
        cancel_ok->store(res.is_ok());
        cancel_replied->store(true);
      });
  ASSERT_TRUE(dut_.runtime().RunWithTimeoutOrUntil([&]() { return cancel_replied->load(); },
                                                   zx::duration::infinite()));
  EXPECT_TRUE(cancel_ok->load());
  fidl::WireResult disable_res = dci_->DisableEndpoint(ep_address);
  ASSERT_OK(disable_res.status());
  EXPECT_TRUE(disable_res.value().is_ok());
  EXPECT_EQ(end_transfer_writes->load(), 0u);

  dut_.RunInDriverContext([&](Dwc3& drv) { fake_dci.CompleteDeferredSetConnected(ZX_OK); });
  dut_.runtime().RunUntilIdle();
  ASSERT_OK(WaitForPhy());
  dut_.RunInDriverContext([&](Dwc3& drv) { EXPECT_FALSE(Dwc3TestHelper::GetPowerOn(drv)); });

  // Plug back in: the endpoint must be configurable again, i.e. WaitForEndpointIdle() must not
  // be left waiting on a start event that was discarded with the previous connection.
  TriggerConnectionPlugIn(fdescriptor::UsbSpeed::kSuper);
  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_TRUE(Dwc3TestHelper::IsActive(drv));
    EXPECT_EQ(GetUserEndpoint(drv, ep_num).ep.stale_starts_to_end, 0u);
  });
  fdescriptor::wire::UsbEndpointDescriptor ep_desc{
      .b_length = sizeof(fdescriptor::wire::UsbEndpointDescriptor),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kEndpoint),
      .b_endpoint_address = ep_address,
      .bm_attributes = static_cast<uint8_t>(fdescriptor::EndpointType::kBulk),
      .w_max_packet_size = 512,
      .b_interval = 0,
  };
  fdescriptor::wire::UsbSsEpCompDescriptor ss_comp_desc{
      .b_length = sizeof(fdescriptor::wire::UsbSsEpCompDescriptor),
      .b_descriptor_type = fidl::ToUnderlying(fdescriptor::DescriptorType::kSsEpCompanion),
      .b_max_burst = 0,
      .bm_attributes = 0,
      .w_bytes_per_interval = 0,
  };
  fidl::WireResult config_res = dci_->ConfigureEndpoint(ep_desc, ss_comp_desc);
  ASSERT_OK(config_res.status());
  ASSERT_TRUE(config_res.value().is_ok()) << "ConfigureEndpoint after reconnect failed: "
                                          << zx_status_get_string(config_res.value().error_value());
  dut_.RunInDriverContext(
      [&](Dwc3& drv) { EXPECT_TRUE(Dwc3TestHelper::IsEpEnabled(drv, ep_num)); });
}

// DEVT_USB_RESET while a DEPSTRTXFER is still in flight on a user endpoint (kStarting*, no
// resource id yet). The graceful cancel can only be issued once the start event delivers the
// resource id, so the endpoint goes kStarting* -> kPendingCancel -> (DEPEVT_XFER_STARTED ->
// DEPENDXFER) -> kCanceling -> (DEPEVT_CMD_CMPLT) -> kIdle, and Ep0Start() must stay parked behind
// the idle barrier across both events.
TEST_P(Dwc3EndpointsTest, HandleResetEventWhileStartingWaitsForStartAndEndBeforeEp0Start) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  // Queue a request but do not deliver DEPEVT_XFER_STARTED yet.
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  WaitForActiveCount(ep_num, 1);
  WaitForState(ep_num,
               enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle);

  // Count DEPENDXFER on ep_num and DEPSTRTXFER on EP0 OUT (issued by Ep0Start() -> Ep0QueueSetup())
  // from here on.
  auto end_transfer_writes = std::make_shared<std::atomic<uint32_t>>(0);
  auto end_transfer_last_val = std::make_shared<std::atomic<uint32_t>>(0);
  auto ep0_start_transfer_writes = std::make_shared<std::atomic<uint32_t>>(0);
  auto cleanup_ep_depcmd = DeferClearDepcmdCallbacks(ep_num);
  auto cleanup_ep0_depcmd = DeferClearDepcmdCallbacks(0);
  dut_.RunInEnvironmentTypeContext([ep_num, end_transfer_writes, end_transfer_last_val,
                                    ep0_start_transfer_writes](Environment& env) {
    auto& ep_depcmd = env.reg_region()[DEPCMD::Get(ep_num).addr()];
    ep_depcmd.SetReadCallback([]() -> uint64_t { return 0; });
    ep_depcmd.SetWriteCallback([ep_num, end_transfer_writes, end_transfer_last_val](uint64_t val) {
      if (DEPCMD::Get(ep_num).FromValue(static_cast<uint32_t>(val)).CMDTYP() ==
          DEPCMD::DEPENDXFER) {
        end_transfer_writes->fetch_add(1);
        end_transfer_last_val->store(static_cast<uint32_t>(val));
      }
    });
    auto& ep0_depcmd = env.reg_region()[DEPCMD::Get(0).addr()];
    ep0_depcmd.SetReadCallback([]() -> uint64_t { return 0; });
    ep0_depcmd.SetWriteCallback([ep0_start_transfer_writes](uint64_t val) {
      if (DEPCMD::Get(0).FromValue(static_cast<uint32_t>(val)).CMDTYP() == DEPCMD::DEPSTRTXFER) {
        ep0_start_transfer_writes->fetch_add(1);
      }
    });
  });

  // Bus reset: there is no resource id to end yet, so the cancel is deferred to the start event
  // and Ep0Start() parks behind the idle barrier.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::HandleResetEvent(drv);
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kPendingCancel);
    EXPECT_EQ(uep.on_idle_callbacks.size(), 1u);
    EXPECT_FALSE(Dwc3TestHelper::IsEpEnabled(drv, 0));
    EXPECT_FALSE(Dwc3TestHelper::IsEpEnabled(drv, 1));
  });
  EXPECT_EQ(end_transfer_writes->load(), 0u);
  EXPECT_EQ(ep0_start_transfer_writes->load(), 0u);
  EXPECT_EQ(event_handler_.completion_count(), 0u);

  // The start event finally delivers the resource id: the deferred cancel is issued (DEPENDXFER
  // with CMDIOC=1) and the endpoint moves to kCanceling, but EP0 still waits for DEPEVT_CMD_CMPLT.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    TriggerEpTransferStarted(drv, ep_num, kResourceId);
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kCanceling);
    EXPECT_EQ(uep.ep.rsrc_id, kResourceId);
    EXPECT_EQ(uep.on_idle_callbacks.size(), 1u);
    EXPECT_FALSE(Dwc3TestHelper::IsEpEnabled(drv, 0));
    EXPECT_FALSE(Dwc3TestHelper::IsEpEnabled(drv, 1));
  });
  EXPECT_EQ(end_transfer_writes->load(), 1u);
  EXPECT_EQ(DEPCMD::Get(ep_num).FromValue(end_transfer_last_val->load()).CMDIOC(), 1u);
  EXPECT_EQ(ep0_start_transfer_writes->load(), 0u);
  EXPECT_EQ(event_handler_.completion_count(), 0u);

  // DEPEVT_CMD_CMPLT for the cancel: the request is returned, the endpoint is idle and only now
  // does the barrier release Ep0Start().
  dut_.RunInDriverContext([&](Dwc3& drv) {
    TriggerEpTransferEnded(drv, ep_num);
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_TRUE(Dwc3TestHelper::IsXferIdle(drv, ep_num));
    EXPECT_TRUE(uep.on_idle_callbacks.empty());
    EXPECT_EQ(uep.ep.stale_starts_to_end, 0u);
    EXPECT_EQ(uep.ep.stale_cancels_to_ignore, 0u);
    EXPECT_TRUE(Dwc3TestHelper::IsEpEnabled(drv, 0));
    EXPECT_TRUE(Dwc3TestHelper::IsEpEnabled(drv, 1));
  });
  EXPECT_EQ(end_transfer_writes->load(), 1u);
  EXPECT_EQ(ep0_start_transfer_writes->load(), 1u);

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1u);
  EXPECT_EQ(completions[0].status, ZX_ERR_IO_NOT_PRESENT);
}

// DEVT_CONNECTION_DONE arriving while a user endpoint is still canceling after DEVT_USB_RESET.
// Both handlers park behind the same idle barrier; when DEPEVT_CMD_CMPLT finally arrives the
// reset's callback starts EP0 exactly once and the connection-done callback only applies the
// negotiated EP0 max packet size on top, instead of starting EP0 a second time.
TEST_P(Dwc3EndpointsTest, HandleConnectionDoneEventWhileCancelingWaitsForIdleAndStartsEp0Once) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  WaitForActiveCount(ep_num, 1);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle);

  // Count DEPSTRTXFER on EP0 OUT (one per Ep0Start()) from here on, and have DSTS report a
  // high-speed connection so the connection-done callback has a visible effect: it must shrink the
  // EP0 max packet size from kEp0MaxPacketSize (512) to 64.
  auto ep0_start_transfer_writes = std::make_shared<std::atomic<uint32_t>>(0);
  auto cleanup_ep0_depcmd = DeferClearDepcmdCallbacks(0);
  auto restore_dsts = DeferClearDstsCallback();
  dut_.RunInEnvironmentTypeContext([ep0_start_transfer_writes](Environment& env) {
    auto& ep0_depcmd = env.reg_region()[DEPCMD::Get(0).addr()];
    ep0_depcmd.SetReadCallback([]() -> uint64_t { return 0; });
    ep0_depcmd.SetWriteCallback([ep0_start_transfer_writes](uint64_t val) {
      if (DEPCMD::Get(0).FromValue(static_cast<uint32_t>(val)).CMDTYP() == DEPCMD::DEPSTRTXFER) {
        ep0_start_transfer_writes->fetch_add(1);
      }
    });
    env.reg_region()[DSTS::Get().addr()].SetReadCallback([]() -> uint32_t {
      return DSTS::Get().FromValue(0).set_CONNECTSPD(DSTS::CONNECTSPD_HIGH).reg_value();
    });
  });

  // Bus reset: the endpoint starts canceling and Ep0Start() parks behind the idle barrier.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::HandleResetEvent(drv);
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kCanceling);
    EXPECT_EQ(uep.on_idle_callbacks.size(), 1u);
    EXPECT_FALSE(Dwc3TestHelper::IsEpEnabled(drv, 0));
  });

  // Connection done before the cancel completes: it parks behind the same barrier, so EP0 is still
  // neither started nor reconfigured.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    TriggerConnectionDone(drv);
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kCanceling);
    EXPECT_EQ(uep.on_idle_callbacks.size(), 2u);
    EXPECT_FALSE(Dwc3TestHelper::IsEpEnabled(drv, 0));
    EXPECT_FALSE(Dwc3TestHelper::IsEpEnabled(drv, 1));
    EXPECT_EQ(Dwc3TestHelper::GetEp0OutMaxPacketSize(drv), uint16_t{512});
  });
  EXPECT_EQ(ep0_start_transfer_writes->load(), 0u);
  EXPECT_EQ(event_handler_.completion_count(), 0u);

  // DEPEVT_CMD_CMPLT releases both callbacks in order: EP0 is started once and then resized for
  // the high-speed connection.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    TriggerEpTransferEnded(drv, ep_num);
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_TRUE(Dwc3TestHelper::IsXferIdle(drv, ep_num));
    EXPECT_TRUE(uep.on_idle_callbacks.empty());
    EXPECT_TRUE(Dwc3TestHelper::IsEpEnabled(drv, 0));
    EXPECT_TRUE(Dwc3TestHelper::IsEpEnabled(drv, 1));
    EXPECT_EQ(Dwc3TestHelper::GetEp0OutMaxPacketSize(drv), uint16_t{64});
  });
  EXPECT_EQ(ep0_start_transfer_writes->load(), 1u);

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1u);
  EXPECT_EQ(completions[0].status, ZX_ERR_IO_NOT_PRESENT);
}

// Cable unplug while Ep0Start() is still parked behind the idle barrier left by DEVT_USB_RESET.
// QuiesceCore() -> StopActiveTransfers() must abort the barrier (idle == false) before it drains
// the canceling endpoint, so reaching kIdle during the drain does not start EP0 on a dead link, and
// the canceling endpoint is drained without a second DEPENDXFER.
TEST_P(Dwc3EndpointsTest, DisconnectWhileIdleBarrierPendingAbortsBarrierWithoutStartingEp0) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  WaitForActiveCount(ep_num, 1);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle);

  auto end_transfer_writes = std::make_shared<std::atomic<uint32_t>>(0);
  auto ep0_start_transfer_writes = std::make_shared<std::atomic<uint32_t>>(0);
  auto cleanup_ep_depcmd = DeferClearDepcmdCallbacks(ep_num);
  auto cleanup_ep0_depcmd = DeferClearDepcmdCallbacks(0);
  dut_.RunInEnvironmentTypeContext([ep_num, end_transfer_writes,
                                    ep0_start_transfer_writes](Environment& env) {
    auto& ep_depcmd = env.reg_region()[DEPCMD::Get(ep_num).addr()];
    ep_depcmd.SetReadCallback([]() -> uint64_t { return 0; });
    ep_depcmd.SetWriteCallback([ep_num, end_transfer_writes](uint64_t val) {
      if (DEPCMD::Get(ep_num).FromValue(static_cast<uint32_t>(val)).CMDTYP() ==
          DEPCMD::DEPENDXFER) {
        end_transfer_writes->fetch_add(1);
      }
    });
    auto& ep0_depcmd = env.reg_region()[DEPCMD::Get(0).addr()];
    ep0_depcmd.SetReadCallback([]() -> uint64_t { return 0; });
    ep0_depcmd.SetWriteCallback([ep0_start_transfer_writes](uint64_t val) {
      if (DEPCMD::Get(0).FromValue(static_cast<uint32_t>(val)).CMDTYP() == DEPCMD::DEPSTRTXFER) {
        ep0_start_transfer_writes->fetch_add(1);
      }
    });
  });

  // Bus reset: graceful cancel (DEPENDXFER with CMDIOC=1), Ep0Start() parked. Park a probe behind
  // the same barrier to observe how it is released.
  auto probe_calls = std::make_shared<std::atomic<uint32_t>>(0);
  auto probe_idle = std::make_shared<std::atomic<bool>>(true);
  dut_.RunInDriverContext([&](Dwc3& drv) {
    Dwc3TestHelper::HandleResetEvent(drv);
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_EQ(uep.ep.transfer_state, TransferState::kCanceling);
    EXPECT_EQ(uep.on_idle_callbacks.size(), 1u);
    Dwc3TestHelper::WaitForAllUserEndpointsIdle(drv, [probe_calls, probe_idle](bool idle) {
      probe_calls->fetch_add(1);
      probe_idle->store(idle);
    });
    EXPECT_EQ(uep.on_idle_callbacks.size(), 2u);
    EXPECT_FALSE(Dwc3TestHelper::IsEpEnabled(drv, 0));
  });
  EXPECT_EQ(end_transfer_writes->load(), 1u);
  EXPECT_EQ(probe_calls->load(), 0u);

  // Unplug before DEPEVT_CMD_CMPLT arrives. No UsbDciInterface is bound, so the disconnect runs to
  // completion (core reset and powered down) without a DCI drain.
  dut_.RunInEnvironmentTypeContext([](Environment& env) { env.usb_phy().completion()->Reset(); });
  dut_.RunInEnvironmentTypeContext(
      [](Environment& env) { env.usb_phy().TriggerConnection(false); });
  dut_.runtime().RunUntilIdle();
  ASSERT_OK(WaitForPhy());

  // The barrier was aborted (idle == false) exactly once, EP0 was never started, the canceling
  // endpoint was drained in software (no second DEPENDXFER) and the stale cancel it recorded was
  // cleared again when events were stopped.
  EXPECT_EQ(probe_calls->load(), 1u);
  EXPECT_FALSE(probe_idle->load());
  EXPECT_EQ(ep0_start_transfer_writes->load(), 0u);
  EXPECT_EQ(end_transfer_writes->load(), 1u);
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_FALSE(Dwc3TestHelper::GetPowerOn(drv));
    EXPECT_FALSE(Dwc3TestHelper::IsEpEnabled(drv, 0));
    EXPECT_FALSE(Dwc3TestHelper::IsEpEnabled(drv, 1));
    EXPECT_TRUE(Dwc3TestHelper::IsXferIdle(drv, ep_num));
    EXPECT_FALSE(Dwc3TestHelper::IsEpEnabled(drv, ep_num));
    EXPECT_TRUE(uep.on_idle_callbacks.empty());
    EXPECT_EQ(uep.ep.stale_cancels_to_ignore, 0u);
    EXPECT_EQ(uep.ep.stale_starts_to_end, 0u);
    ASSERT_TRUE(uep.server.has_value());
    EXPECT_TRUE(uep.server->cancel_completers.empty());
  });

  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1u);
  EXPECT_EQ(completions[0].status, ZX_ERR_IO_NOT_PRESENT);
}

// A DEPENDXFER that times out (DEPCMD.CMDACT stuck) during DEVT_USB_RESET owes no DEPEVT_CMD_CMPLT,
// so it must not be recorded as a stale cancel. Otherwise the next real cancel on the endpoint
// would have its completion swallowed, leaving it stuck in kCanceling with the function driver's
// CancelAll parked forever. Re-configure the endpoint after the reset, run a second transfer and
// cancel it over the Endpoint protocol: its DEPEVT_CMD_CMPLT must retire the transfer and reply.
TEST_P(Dwc3EndpointsTest, CancelAllAfterEndTransferTimeoutDoesNotSwallowNextCancelCompletion) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x02;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  WaitForActiveCount(ep_num, 1);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle);

  // Make DEPCMD(ep_num) read back whatever was last written, so CMDACT stays set and
  // WaitForCmdAct() times out on the DEPENDXFER issued by the reset.
  auto end_transfer_writes = std::make_shared<std::atomic<uint32_t>>(0);
  auto end_transfer_last_val = std::make_shared<std::atomic<uint32_t>>(0);
  auto ep_depcmd_val = std::make_shared<std::atomic<uint32_t>>(0);
  auto cleanup_ep_depcmd = DeferClearDepcmdCallbacks(ep_num);
  auto install_depcmd = [&](bool cmdact_stuck) {
    dut_.RunInEnvironmentTypeContext([ep_num, cmdact_stuck, end_transfer_writes,
                                      end_transfer_last_val, ep_depcmd_val](Environment& env) {
      auto& ep_depcmd = env.reg_region()[DEPCMD::Get(ep_num).addr()];
      if (cmdact_stuck) {
        ep_depcmd.SetReadCallback([ep_depcmd_val]() -> uint64_t { return ep_depcmd_val->load(); });
      } else {
        ep_depcmd.SetReadCallback([]() -> uint64_t { return 0; });
      }
      ep_depcmd.SetWriteCallback(
          [ep_num, end_transfer_writes, end_transfer_last_val, ep_depcmd_val](uint64_t val) {
            ep_depcmd_val->store(static_cast<uint32_t>(val));
            if (DEPCMD::Get(ep_num).FromValue(static_cast<uint32_t>(val)).CMDTYP() ==
                DEPCMD::DEPENDXFER) {
              end_transfer_writes->fetch_add(1);
              end_transfer_last_val->store(static_cast<uint32_t>(val));
            }
          });
    });
  };
  install_depcmd(/*cmdact_stuck=*/true);

  // Bus reset: the DEPENDXFER times out, the endpoint is drained in software, no stale cancel is
  // recorded and EP0 starts right away.
  dut_.RunInDriverContext([](Dwc3& drv) { Dwc3TestHelper::HandleResetEvent(drv); });
  dut_.runtime().RunUntilIdle();
  EXPECT_EQ(end_transfer_writes->load(), 1u);
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    ASSERT_TRUE(Dwc3TestHelper::IsXferIdle(drv, ep_num));
    EXPECT_FALSE(Dwc3TestHelper::IsEpEnabled(drv, ep_num));
    EXPECT_EQ(uep.ep.stale_cancels_to_ignore, 0u);
    EXPECT_EQ(uep.ep.stale_starts_to_end, 0u);
    EXPECT_TRUE(Dwc3TestHelper::IsEpEnabled(drv, 0));
    EXPECT_TRUE(Dwc3TestHelper::IsEpEnabled(drv, 1));
  });
  std::vector<CompletionResult> completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1u);
  EXPECT_EQ(completions[0].status, ZX_ERR_IO_NOT_PRESENT);

  // Commands complete normally again; the function driver re-configures the endpoint after the
  // reset and queues a new transfer.
  install_depcmd(/*cmdact_stuck=*/false);
  ASSERT_NO_FATAL_FAILURE(ConfigureEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512));
  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_TRUE(Dwc3TestHelper::IsEpEnabled(drv, ep_num));
    TriggerEpTransferNotReady(drv, ep_num, 0);
  });
  dut_.runtime().RunUntilIdle();
  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  WaitForActiveCount(ep_num, 1);
  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  WaitForState(ep_num, enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle);

  // A real cancel over the Endpoint protocol: DEPENDXFER with CMDIOC=1, reply parked until the
  // completion event.
  auto cancel_replied = std::make_shared<std::atomic<bool>>(false);
  auto cancel_ok = std::make_shared<std::atomic<bool>>(false);
  ep_client_->CancelAll().Then(
      [cancel_replied, cancel_ok](fidl::Result<fendpoint::Endpoint::CancelAll>& res) {
        cancel_ok->store(res.is_ok());
        cancel_replied->store(true);
      });
  WaitForState(ep_num, TransferState::kCanceling);
  EXPECT_EQ(end_transfer_writes->load(), 2u);
  EXPECT_EQ(DEPCMD::Get(ep_num).FromValue(end_transfer_last_val->load()).CMDIOC(), 1u);
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto& uep = GetUserEndpoint(drv, ep_num);
    ASSERT_TRUE(uep.server.has_value());
    EXPECT_EQ(uep.server->cancel_completers.size(), 1u);
  });
  EXPECT_FALSE(cancel_replied->load());

  // Its DEPEVT_CMD_CMPLT must be consumed as the real cancel completion, not ignored as a stale one
  // leaked by the earlier timeout: the endpoint goes idle, the request is returned and the parked
  // CancelAll replies.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    TriggerEpTransferEnded(drv, ep_num);
    auto& uep = GetUserEndpoint(drv, ep_num);
    EXPECT_TRUE(Dwc3TestHelper::IsXferIdle(drv, ep_num));
    EXPECT_TRUE(uep.server->cancel_completers.empty());
    EXPECT_EQ(uep.ep.stale_cancels_to_ignore, 0u);
  });
  ASSERT_TRUE(dut_.runtime().RunWithTimeoutOrUntil([&]() { return cancel_replied->load(); },
                                                   zx::duration::infinite()));
  EXPECT_TRUE(cancel_ok->load());
  completions = event_handler_.WaitForCompletions(1);
  ASSERT_EQ(completions.size(), 1u);
  EXPECT_EQ(completions[0].status, ZX_ERR_CANCELED);
}

// usb-peripheral does not answer SetConnected(false) for longer than the drain watchdog allows.
// Nothing recovers from that yet: the stall is only reported (log + Inspect counter) while the
// drain keeps holding the core powered, the PHY untold and the watcher disarmed. Once the late
// reply does arrive the normal power-down still happens and the stall is not reported again.
TEST_F(Dwc3EndpointsTestBase, DisconnectDrainStallIsReportedAndLateReplyStillPowersDown) {
  TriggerConnection();

  FakeUsbDciInterface fake_dci;
  fake_dci.SetDeferSetConnectedFalse(true);
  libsync::Completion set_connected_false_received;
  fake_dci.SetSetConnectedCallback([&](bool connected) {
    if (!connected) {
      set_connected_false_received.Signal();
    }
  });
  std::optional<fidl::ServerBindingRef<fdci::UsbDciInterface>> dci_binding;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    auto [client_end, server_end] = fidl::Endpoints<fdci::UsbDciInterface>::Create();
    dci_binding = fidl::BindServer(fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                   std::move(server_end), &fake_dci);
    Dwc3TestHelper::BindDciInterface(drv, std::move(client_end));
    // Make the watchdog fire almost immediately instead of after the production delay.
    Dwc3TestHelper::SetDciDisconnectDrainStallDelay(drv, zx::msec(1));
    EXPECT_EQ(Dwc3TestHelper::GetDciDisconnectStallCount(drv), 0u);
  });
  auto unbind_dci = fit::defer([&]() {
    dut_.RunInDriverContext([&](Dwc3& drv) { dci_binding->Unbind(); });
    dut_.runtime().RunUntilIdle();
  });

  size_t disconnected_reports_before = 0;
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    disconnected_reports_before = env.usb_phy().disconnected_reports();
    env.usb_phy().completion()->Reset();
  });

  // Unplug; usb-peripheral holds the reply.
  dut_.RunInEnvironmentTypeContext(
      [](Environment& env) { env.usb_phy().TriggerConnection(false); });
  dut_.runtime().RunUntilIdle();
  ASSERT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() { return set_connected_false_received.signaled(); }, zx::duration::infinite()));
  dut_.runtime().RunUntilIdle();
  ASSERT_TRUE(fake_dci.has_deferred_set_connected());

  // The watchdog reports the stall exactly once while the drain is still held.
  ASSERT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        return dut_.RunInDriverContext<bool>(
            [](Dwc3& drv) { return Dwc3TestHelper::GetDciDisconnectStallCount(drv) == 1; });
      },
      zx::duration::infinite()));
  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_TRUE(Dwc3TestHelper::GetPowerOn(drv));
    EXPECT_FALSE(Dwc3TestHelper::IsActive(drv));
  });
  dut_.RunInEnvironmentTypeContext([&](Environment& env) {
    EXPECT_EQ(env.usb_phy().disconnected_reports(), disconnected_reports_before);
    EXPECT_FALSE(env.usb_phy().has_completer());
  });

  // The late reply still completes the drain: core powered down, PHY told, watcher re-armed, and
  // the stall is not counted a second time.
  dut_.RunInDriverContext([&](Dwc3& drv) { fake_dci.CompleteDeferredSetConnected(ZX_OK); });
  dut_.runtime().RunUntilIdle();
  ASSERT_OK(WaitForPhy());
  dut_.RunInDriverContext([&](Dwc3& drv) {
    EXPECT_FALSE(Dwc3TestHelper::GetPowerOn(drv));
    EXPECT_EQ(Dwc3TestHelper::GetDciDisconnectStallCount(drv), 1u);
  });
  EXPECT_TRUE(dut_.runtime().RunWithTimeoutOrUntil(
      [&]() {
        size_t reports = 0;
        dut_.RunInEnvironmentTypeContext(
            [&](Environment& env) { reports = env.usb_phy().disconnected_reports(); });
        return reports == disconnected_reports_before + 1;
      },
      zx::duration::infinite()));
}

TEST_F(Dwc3EndpointsTestBase, Ep0ResetWhileStartingSingleEndsStaleStartAndActivatesNewSetup) {
  TriggerConnection();

  // TriggerConnection() calls StartController() -> StartPeripheralMode() -> Ep0Start() ->
  // Ep0QueueSetup(), leaving EP0 OUT (ep_num = 0) in kStartingSingle waiting for its
  // DEPEVT_CMD_CMPLT(DEPSTRTXFER) event.
  dut_.RunInDriverContext([](Dwc3& drv) {
    EXPECT_EQ(Dwc3TestHelper::GetEpTransferState(drv, 0), TransferState::kStartingSingle);
    EXPECT_TRUE(Dwc3TestHelper::IsEpEnabled(drv, 0));
  });

  // Trigger a USB reset before the first DEPSTRTXFER completion arrives. Ep0Reset() records the
  // in-flight start in stale_starts_to_end and Ep0Start() immediately queues a new SETUP transfer,
  // putting EP0 OUT back into kStartingSingle.
  dut_.RunInDriverContext([](Dwc3& drv) {
    Dwc3TestHelper::HandleResetEvent(drv);
    EXPECT_EQ(Dwc3TestHelper::GetEpTransferState(drv, 0), TransferState::kStartingSingle);
    EXPECT_TRUE(Dwc3TestHelper::IsEpEnabled(drv, 0));
  });

  auto ep0_end_xfer_count = std::make_shared<std::atomic<uint32_t>>(0);
  auto ep0_last_end_xfer_cmd = std::make_shared<std::atomic<uint32_t>>(0);
  auto cleanup_depcmd = DeferClearDepcmdCallbacks(0);
  dut_.RunInEnvironmentTypeContext([ep0_end_xfer_count, ep0_last_end_xfer_cmd](Environment& env) {
    auto& reg = env.reg_region()[DEPCMD::Get(0).addr()];
    reg.SetReadCallback([]() -> uint64_t { return 0; });
    reg.SetWriteCallback([ep0_end_xfer_count, ep0_last_end_xfer_cmd](uint64_t val) {
      uint32_t cmd_val = static_cast<uint32_t>(val);
      if (DEPCMD::Get(0).FromValue(cmd_val).CMDTYP() == DEPCMD::DEPENDXFER) {
        ep0_end_xfer_count->fetch_add(1);
        ep0_last_end_xfer_cmd->store(cmd_val);
      }
    });
  });

  // Deliver the stale DEPSTRTXFER completion (rsrc_id = 3) from before the reset. It must be
  // ended synchronously (CMDIOC == 0) and leave the new SETUP transfer in kStartingSingle.
  constexpr uint32_t kStaleRsrcId = 3;
  constexpr uint32_t kNewRsrcId = 4;
  dut_.RunInDriverContext([&](Dwc3& drv) {
    TriggerEpTransferStarted(drv, 0, kStaleRsrcId);
    EXPECT_EQ(Dwc3TestHelper::GetEpTransferState(drv, 0), TransferState::kStartingSingle);
    EXPECT_TRUE(Dwc3TestHelper::IsEpEnabled(drv, 0));
  });
  EXPECT_EQ(ep0_end_xfer_count->load(), 1u);
  auto stale_end_cmd = DEPCMD::Get(0).FromValue(ep0_last_end_xfer_cmd->load());
  EXPECT_EQ(stale_end_cmd.COMMANDPARAM(), kStaleRsrcId);
  EXPECT_EQ(stale_end_cmd.CMDIOC(), 0u);

  // Deliver the new DEPSTRTXFER completion (rsrc_id = 4) for the post-reset SETUP transfer.
  // It must transition EP0 OUT to kActiveSingle with kNewRsrcId without issuing DEPENDXFER.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    TriggerEpTransferStarted(drv, 0, kNewRsrcId);
    EXPECT_EQ(Dwc3TestHelper::GetEpTransferState(drv, 0), TransferState::kActiveSingle);
    EXPECT_EQ(Dwc3TestHelper::GetEpRsrcId(drv, 0), kNewRsrcId);
  });
  EXPECT_EQ(ep0_end_xfer_count->load(), 1u);
}

TEST_P(Dwc3EndpointsTest, InputEndpointBabbleUnderflow) {
  const bool enqueue_many = GetParam();
  TriggerConnection();

  const uint8_t ep_address = 0x82;
  const uint8_t ep_num = UsbAddressToEpNum(ep_address);

  SetupEndpoint(ep_address, fdescriptor::EndpointType::kBulk, 512);
  RegisterVmo(1, 4096);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferNotReady(drv, ep_num, 0); });
  dut_.runtime().RunUntilIdle();

  QueueRequest(1, 0, 512, fdescriptor::EndpointType::kBulk);
  TransferState expected_starting_state =
      enqueue_many ? TransferState::kStartingOngoing : TransferState::kStartingSingle;
  WaitForState(ep_num, expected_starting_state);

  dut_.RunInDriverContext([&](Dwc3& drv) { TriggerEpTransferStarted(drv, ep_num, kResourceId); });
  TransferState expected_state =
      enqueue_many ? TransferState::kActiveOngoing : TransferState::kActiveSingle;
  WaitForState(ep_num, expected_state);

  // Complete the TRB with residual (600) > expected (512) to simulate a babble/underflow condition.
  dut_.RunInDriverContext([&](Dwc3& drv) {
    if (enqueue_many) {
      TriggerEpTransferInProgress(drv, ep_num, 600);
    } else {
      TriggerEpTransferComplete(drv, ep_num, 600);
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
  EXPECT_EQ(completions[0].transfer_size, 0UL);
}

namespace {
INSTANTIATE_TEST_SUITE_P(Dwc3EndpointsTestCases, Dwc3EndpointsTest, testing::Bool());
}  // namespace

}  // namespace dwc3
