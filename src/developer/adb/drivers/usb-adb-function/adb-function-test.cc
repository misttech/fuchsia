// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "adb-function.h"

#include <fidl/fuchsia.hardware.adb/cpp/fidl.h>
#include <lib/async-loop/default.h>
#include <lib/async/default.h>
#include <lib/driver/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/fidl/cpp/wire/status.h>
#include <lib/inspect/testing/cpp/inspect.h>
#include <lib/sync/completion.h>
#include <lib/zx/result.h>
#include <zircon/compiler.h>

#include <cstring>
#include <vector>

#include <gtest/gtest.h>
#include <usb-inspect/usb-inspect-test-helper.h>
#include <usb/usb-request.h>

#include "src/devices/usb/lib/usb-endpoint/testing/fake-usb-endpoint-server.h"

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace usb_adb_function {

class UsbAdbTestHelper {
 public:
  static State state(const UsbAdbDevice& device) { return device.state_; }
  static void set_state(UsbAdbDevice& device, State state) { device.state_ = state; }
  static bool has_usb_function_binding(const UsbAdbDevice& device) {
    return device.usb_function_binding_.has_value();
  }
  static bool has_adb_binding(const UsbAdbDevice& device) {
    return device.adb_binding_.has_value();
  }
  static bool CancelAllCompleted(const UsbAdbDevice& device) { return device.CancelAllCompleted(); }
  static bool AllRequestsReturned(UsbAdbDevice& device) { return device.AllRequestsReturned(); }
  static bool InternalPoolsFull(UsbAdbDevice& device) {
    return device.bulk_out_ep_.RequestsFull() && device.bulk_in_ep_.RequestsFull();
  }
  static size_t BulkInEpPendingRequests(UsbAdbDevice& device) {
    return device.bulk_in_ep_.GetInFlightCount();
  }
  static size_t TxPendingReqsCount(const UsbAdbDevice& device) {
    return device.tx_pending_reqs_.size();
  }
  static size_t RxRequestsCount(const UsbAdbDevice& device) { return device.rx_requests_.size(); }
  static size_t PendingRepliesCount(const UsbAdbDevice& device) {
    return device.pending_replies_.size();
  }
  static inspect::Inspector GetInspector(UsbAdbDevice& device) {
    if (device.component_inspector_.has_value()) {
      return device.component_inspector_->inspector();
    }
    return {};
  }

  static usb_inspect::ThroughputTracker* GetThroughputTracker(UsbAdbDevice& device) {
    if (device.throughput_tracker_.has_value()) {
      return &*device.throughput_tracker_;
    }
    return nullptr;
  }
};

static constexpr uint32_t kBulkOutEp = 1;
static constexpr uint32_t kBulkInEp = 2;

class DelayedCancelEndpoint : public fake_usb_endpoint::FakeEndpoint {
 public:
  void CancelAll(CancelAllCompleter::Sync& completer) override {
    zx_status_t reply_status = ZX_OK;
    {
      std::lock_guard<std::mutex> _(lock_);
      reply_status = cancel_status_;
      if (reply_status == ZX_OK && hold_cancel_) {
        delayed_cancels_.push_back(completer.ToAsync());
        return;
      }
    }
    // Requests are drained even when replying an error. This mirrors
    // Dwc3::EpServer::CancelAll(), which completes all active requests when the
    // controller is stopped or powered off. Replying an error without draining
    // would leave requests permanently outstanding and stall teardown.
    DoCancelAll(completer.ToAsync(), reply_status);
  }

  void ReleaseCancelAll() {
    std::vector<CancelAllCompleter::Async> completers;
    {
      std::lock_guard<std::mutex> _(lock_);
      completers = std::move(delayed_cancels_);
      delayed_cancels_.clear();
    }
    for (auto& completer : completers) {
      DoCancelAll(std::move(completer));
    }
  }

  void set_hold_cancel(bool hold) {
    std::lock_guard<std::mutex> _(lock_);
    hold_cancel_ = hold;
  }

  void set_cancel_status(zx_status_t status) {
    std::lock_guard<std::mutex> _(lock_);
    cancel_status_ = status;
  }

  void RequestCompleteAtIndex(size_t index, zx_status_t status, size_t actual) {
    std::vector<fuchsia_hardware_usb_endpoint::Completion> completions;
    std::optional<fidl::ServerBindingRef<fuchsia_hardware_usb_endpoint::Endpoint>> local_binding;
    {
      std::lock_guard<std::mutex> _(lock_);
      ASSERT_LT(index, requests_.size());
      local_binding = binding_ref_;
      auto request = std::move(requests_[index]);
      requests_.erase(requests_.begin() + static_cast<std::ptrdiff_t>(index));
      completions.emplace_back(std::move(fuchsia_hardware_usb_endpoint::Completion()
                                             .request(std::move(request))
                                             .status(status)
                                             .transfer_size(actual)));
    }
    if (local_binding.has_value()) {
      EXPECT_TRUE(fidl::SendEvent(*local_binding)->OnCompletion(std::move(completions)).is_ok());
    }
  }

 private:
  void DoCancelAll(CancelAllCompleter::Async completer, zx_status_t reply_status = ZX_OK) {
    std::vector<fuchsia_hardware_usb_endpoint::Completion> completions;
    std::optional<fidl::ServerBindingRef<fuchsia_hardware_usb_endpoint::Endpoint>> local_binding;
    {
      std::lock_guard<std::mutex> _(lock_);
      local_binding = binding_ref_;
      std::vector<fuchsia_hardware_usb_request::Request> pending_requests = std::move(requests_);
      requests_.clear();
      completions.reserve(pending_requests.size());
      for (auto& request : pending_requests) {
        completions.emplace_back(std::move(fuchsia_hardware_usb_endpoint::Completion()
                                               .request(std::move(request))
                                               .status(ZX_ERR_CANCELED)
                                               .transfer_size(0)));
      }
    }
    if (!completions.empty() && local_binding.has_value()) {
      EXPECT_TRUE(fidl::SendEvent(*local_binding)->OnCompletion(std::move(completions)).is_ok());
    }
    if (reply_status == ZX_OK) {
      completer.Reply(fit::ok());
    } else {
      completer.Reply(fit::error(reply_status));
    }
  }

  zx_status_t cancel_status_ __TA_GUARDED(lock_) = ZX_OK;
  bool hold_cancel_ __TA_GUARDED(lock_) = false;
  std::vector<CancelAllCompleter::Async> delayed_cancels_ __TA_GUARDED(lock_);
};

class AdbFakeUsb
    : public fake_usb_endpoint::FakeUsbFidlProvider<fuchsia_hardware_usb_function::UsbFunction,
                                                    DelayedCancelEndpoint> {
 public:
  using Base = fake_usb_endpoint::FakeUsbFidlProvider<fuchsia_hardware_usb_function::UsbFunction,
                                                      DelayedCancelEndpoint>;
  AdbFakeUsb(async_dispatcher_t* dispatcher) : Base(dispatcher), dispatcher_(dispatcher) {}

  void AllocResources(
      fidl::Request<fuchsia_hardware_usb_function::UsbFunction::AllocResources>& request,
      fidl::internal::NaturalCompleter<
          fuchsia_hardware_usb_function::UsbFunction::AllocResources>::Sync& completer) override {
    zx_status_t alloc_status = ZX_OK;
    bool omit_eps = false;
    {
      std::lock_guard<std::mutex> _(lock_);
      alloc_status = alloc_resources_status_;
      omit_eps = omit_endpoints_;
    }
    if (alloc_status != ZX_OK) {
      completer.Reply(fit::error(alloc_status));
      return;
    }

    fuchsia_hardware_usb_function::UsbFunctionAllocResourcesResponse response;
    EXPECT_EQ(request.endpoints().size(), 2u);
    EXPECT_EQ(request.interface_count(), 1u);
    EXPECT_EQ(request.strings().size(), 0u);
    response.interface_nums() = {0};
    if (omit_eps) {
      response.endpoint_addrs() = {kBulkOutEp};
    } else {
      response.endpoint_addrs() = {kBulkOutEp, kBulkInEp};
    }
    response.string_indices() = {};

    size_t count = std::min(request.endpoints().size(), response.endpoint_addrs().size());
    for (size_t i = 0; i < count; i++) {
      uint8_t addr = response.endpoint_addrs()[i];
      fake_endpoint(addr).Connect(dispatcher_, std::move(request.endpoints()[i].endpoint()));
    }

    completer.Reply(fit::ok(std::move(response)));
  }

  void Configure(
      fidl::Request<fuchsia_hardware_usb_function::UsbFunction::Configure>& request,
      fidl::internal::NaturalCompleter<fuchsia_hardware_usb_function::UsbFunction::Configure>::Sync&
          completer) override {
    fit::callback<void()> cb;
    {
      std::lock_guard<std::mutex> _(lock_);
      if (configure_status_ != ZX_OK) {
        completer.Reply(fit::error(configure_status_));
        return;
      }
      if (iface_client_.is_valid()) {
        completer.Reply(fit::error(ZX_ERR_ALREADY_BOUND));
        return;
      }
      iface_client_ = std::move(request.iface());
      configured_ = true;
      deconfigured_ = false;
      cb = std::move(on_configured_);
    }
    completer.Reply(fit::ok());
    if (cb) {
      cb();
    }
  }

  void Deconfigure(
      fidl::internal::NaturalCompleter<
          fuchsia_hardware_usb_function::UsbFunction::Deconfigure>::Sync& completer) override {
    fit::callback<void()> cb;
    {
      std::lock_guard<std::mutex> _(lock_);
      deconfigured_ = true;
      deconfigure_count_++;
      configured_ = false;
      if (hold_deconfigure_) {
        delayed_deconfigure_completer_ = completer.ToAsync();
        return;
      }
      cb = std::move(on_deconfigured_);
    }
    completer.Reply(fit::ok());
    if (cb) {
      cb();
    }
  }

  size_t deconfigure_count() const {
    std::lock_guard<std::mutex> _(lock_);
    return deconfigure_count_;
  }

  bool is_configured() const {
    std::lock_guard<std::mutex> _(lock_);
    return configured_;
  }
  void set_hold_deconfigure(bool hold) {
    std::lock_guard<std::mutex> _(lock_);
    hold_deconfigure_ = hold;
  }

  fidl::ClientEnd<fuchsia_hardware_usb_function::UsbFunctionInterface> TakeIfaceClient() {
    std::lock_guard<std::mutex> _(lock_);
    return std::move(iface_client_);
  }

  void set_on_configured(fit::callback<void()> on_configured) {
    bool call_now = false;
    {
      std::lock_guard<std::mutex> _(lock_);
      // Only report "already configured" when a fresh iface client is actually
      // available to hand out. Otherwise WaitConfigured() would re-register and
      // be signalled immediately on every iteration, spinning until the driver
      // finishes teardown and rebinds.
      if (configured_ && iface_client_.is_valid()) {
        call_now = true;
      } else {
        on_configured_ = std::move(on_configured);
      }
    }
    if (call_now && on_configured) {
      on_configured();
    }
  }

  void set_on_deconfigured(fit::callback<void()> on_deconfigured) {
    bool call_now = false;
    {
      std::lock_guard<std::mutex> _(lock_);
      if (deconfigured_) {
        call_now = true;
      } else {
        on_deconfigured_ = std::move(on_deconfigured);
      }
    }
    if (call_now && on_deconfigured) {
      on_deconfigured();
    }
  }

  void DisableEndpoint(
      fidl::Request<fuchsia_hardware_usb_function::UsbFunction::DisableEndpoint>& request,
      fidl::internal::NaturalCompleter<
          fuchsia_hardware_usb_function::UsbFunction::DisableEndpoint>::Sync& completer) override {
    {
      std::lock_guard<std::mutex> _(lock_);
      disabled_endpoints_.push_back(request.endpoint_address());
    }
    Base::DisableEndpoint(request, completer);
  }

  std::vector<uint8_t> disabled_endpoints() const {
    std::lock_guard<std::mutex> _(lock_);
    return disabled_endpoints_;
  }
  void clear_disabled_endpoints() {
    std::lock_guard<std::mutex> _(lock_);
    disabled_endpoints_.clear();
  }

  void ConfigureEndpoint(
      fidl::Request<fuchsia_hardware_usb_function::UsbFunction::ConfigureEndpoint>& request,
      fidl::internal::NaturalCompleter<
          fuchsia_hardware_usb_function::UsbFunction::ConfigureEndpoint>::Sync& completer)
      override {
    bool fail = false;
    {
      std::lock_guard<std::mutex> _(lock_);
      fail = fail_configure_endpoint_ ||
             (fail_configure_endpoint_addr_.has_value() &&
              *fail_configure_endpoint_addr_ == request.endpoint_address());
    }
    if (fail) {
      completer.Reply(fit::error(ZX_ERR_NO_RESOURCES));
      return;
    }
    Base::ConfigureEndpoint(request, completer);
  }

  void set_fail_configure_endpoint(bool fail) {
    std::lock_guard<std::mutex> _(lock_);
    fail_configure_endpoint_ = fail;
  }

  void set_fail_configure_endpoint_addr(std::optional<uint8_t> ep_addr) {
    std::lock_guard<std::mutex> _(lock_);
    fail_configure_endpoint_addr_ = ep_addr;
  }

  void set_alloc_resources_status(zx_status_t status) {
    std::lock_guard<std::mutex> _(lock_);
    alloc_resources_status_ = status;
  }
  void set_configure_status(zx_status_t status) {
    std::lock_guard<std::mutex> _(lock_);
    configure_status_ = status;
  }
  void set_omit_endpoints(bool omit) {
    std::lock_guard<std::mutex> _(lock_);
    omit_endpoints_ = omit;
  }

 private:
  async_dispatcher_t* dispatcher_;
  fidl::ClientEnd<fuchsia_hardware_usb_function::UsbFunctionInterface> iface_client_
      __TA_GUARDED(lock_);
  fit::callback<void()> on_configured_ __TA_GUARDED(lock_);
  fit::callback<void()> on_deconfigured_ __TA_GUARDED(lock_);
  zx_status_t alloc_resources_status_ __TA_GUARDED(lock_) = ZX_OK;
  zx_status_t configure_status_ __TA_GUARDED(lock_) = ZX_OK;
  bool omit_endpoints_ __TA_GUARDED(lock_) = false;
  bool fail_configure_endpoint_ __TA_GUARDED(lock_) = false;
  std::optional<uint8_t> fail_configure_endpoint_addr_ __TA_GUARDED(lock_);
  bool hold_deconfigure_ __TA_GUARDED(lock_) = false;
  std::optional<fidl::internal::NaturalCompleter<
      fuchsia_hardware_usb_function::UsbFunction::Deconfigure>::Async>
      delayed_deconfigure_completer_ __TA_GUARDED(lock_);
  bool deconfigured_ __TA_GUARDED(lock_) = false;
  size_t deconfigure_count_ __TA_GUARDED(lock_) = 0;
  bool configured_ __TA_GUARDED(lock_) = false;
  std::vector<uint8_t> disabled_endpoints_ __TA_GUARDED(lock_);
  mutable std::mutex lock_;
};

class UsbAdbEnvironment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();
    fake_dev_.emplace(dispatcher);

    fake_dev_->set_alloc_resources_status(alloc_resources_status_);
    fake_dev_->set_configure_status(configure_status_);
    fake_dev_->set_omit_endpoints(omit_endpoints_);

    fuchsia_hardware_usb_function::UsbFunctionService::InstanceHandler handler({
        .device = usb_function_bindings_.CreateHandler(&*fake_dev_, dispatcher,
                                                       fidl::kIgnoreBindingClosure),
    });
    EXPECT_TRUE(
        to_driver_vfs
            .AddService<fuchsia_hardware_usb_function::UsbFunctionService>(std::move(handler))
            .is_ok());

    return zx::ok();
  }

  void set_alloc_resources_status(zx_status_t status) { alloc_resources_status_ = status; }
  void set_configure_status(zx_status_t status) { configure_status_ = status; }
  void set_omit_endpoints(bool omit) { omit_endpoints_ = omit; }

  void CancelAllUsbRequests() {
    if (!fake_dev_.has_value()) {
      return;
    }
    while (fake_dev_->fake_endpoint(kBulkOutEp).pending_request_count() > 0) {
      fake_dev_->fake_endpoint(kBulkOutEp).RequestComplete(ZX_ERR_CANCELED, 0);
    }
    while (fake_dev_->fake_endpoint(kBulkInEp).pending_request_count() > 0) {
      fake_dev_->fake_endpoint(kBulkInEp).RequestComplete(ZX_ERR_CANCELED, 0);
    }
  }

  std::optional<AdbFakeUsb> fake_dev_;
  fidl::ServerBindingGroup<fuchsia_hardware_usb_function::UsbFunction> usb_function_bindings_;
  zx_status_t alloc_resources_status_ = ZX_OK;
  zx_status_t configure_status_ = ZX_OK;
  bool omit_endpoints_ = false;
};

class UsbAdbTestConfig final {
 public:
  using DriverType = UsbAdbDevice;
  using EnvironmentType = UsbAdbEnvironment;
};

class EventHandler : public fidl::WireSyncEventHandler<fadb::UsbAdbImpl> {
 public:
  ~EventHandler() { EXPECT_TRUE(expected_statuses_.empty()); }

  void OnStatusChanged(fidl::WireEvent<fadb::UsbAdbImpl::OnStatusChanged>* event) override {
    ASSERT_FALSE(expected_statuses_.empty());
    EXPECT_EQ(event->status, expected_statuses_.front());
    expected_statuses_.pop();
  }

  std::queue<fadb::StatusFlags> expected_statuses_;
};

class UsbAdbTest : public testing::Test {
 public:
  void ReadInspect(const inspect::Inspector& inspector) {
    hierarchy_ = usb_inspect::ReadHierarchyFromInspector(inspector);
  }
  const inspect::Hierarchy& hierarchy() const { return *hierarchy_; }

  std::optional<inspect::Hierarchy> hierarchy_;

  fidl::WireSyncClient<fadb::UsbAdbImpl> NormalStartAdb() {
    auto [client_end, server_end] = fidl::Endpoints<fadb::UsbAdbImpl>::Create();
    auto start_res = client_->StartAdb(std::move(server_end));
    EXPECT_TRUE(start_res.ok());
    EXPECT_TRUE(start_res->is_ok());
    WaitConfigured();
    EnableUsb();

    return fidl::WireSyncClient<fadb::UsbAdbImpl>(std::move(client_end));
  }

  void SetUp() override {
    ASSERT_EQ(driver_test_.StartDriver().status_value(), ZX_OK);

    driver_test_.runtime().RunUntil([&]() {
      bool ready = false;
      driver_test_.RunInEnvironmentTypeContext(
          [&](UsbAdbEnvironment& env) { ready = env.fake_dev_->is_configured(); });
      return ready;
    });

    auto device = driver_test_.Connect<fadb::Service::Adb>();
    EXPECT_EQ(device.status_value(), ZX_OK);
    client_.Bind(std::move(device.value()));

    driver_test_.RunInDriverContext([&](UsbAdbDevice& dev) {
      EXPECT_EQ(dev.bulk_out_addr(), kBulkOutEp);
      EXPECT_EQ(dev.bulk_in_addr(), kBulkInEp);
    });
  }

  void TearDown() override {
    client_ = {};
    iface_client_ = {};

    SafeStopDriver();

    driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
      if (env.fake_dev_.has_value()) {
        env.fake_dev_->set_on_configured(nullptr);
        env.fake_dev_->set_on_deconfigured(nullptr);
        env.fake_dev_.reset();
      }
      env.usb_function_bindings_.RemoveAll();
    });

    driver_test_.runtime().RunUntilIdle();
  }

  void EnsureIfaceBound() { WaitConfigured(); }

  void EnableUsb() {
    EnsureIfaceBound();
    ASSERT_TRUE(iface_client_.is_valid());
    fidl::Result result = iface_client_->SetConfigured({{
        .configured = true,
        .speed = fdescriptor::UsbSpeed::kFull,
    }});
    EXPECT_TRUE(result.is_ok()) << result.error_value().FormatDescription();

    driver_test_.runtime().RunUntil([&]() {
      size_t count = 0;
      driver_test_.RunInEnvironmentTypeContext([&count](UsbAdbEnvironment& env) {
        count = env.fake_dev_->fake_endpoint(kBulkOutEp).pending_request_count();
      });
      bool is_online = false;
      driver_test_.RunInDriverContext([&is_online](UsbAdbDevice& dev) {
        is_online = (UsbAdbTestHelper::state(dev) == State::kOnline);
      });
      return (count == kBulkRxCount) && is_online;
    });
  }

  void WaitConfigured() {
    if (iface_client_.is_valid()) {
      return;
    }
    fidl::ClientEnd<fuchsia_hardware_usb_function::UsbFunctionInterface> iface_client;
    for (;;) {
      libsync::Completion configured;
      driver_test_.RunInEnvironmentTypeContext(
          [&iface_client, &configured](UsbAdbEnvironment& env) {
            if (!env.fake_dev_.has_value()) {
              return;
            }
            iface_client = env.fake_dev_->TakeIfaceClient();
            if (iface_client.is_valid()) {
              env.fake_dev_->set_on_configured(nullptr);
              return;
            }
            env.fake_dev_->set_on_configured([&configured]() { configured.Signal(); });
          });
      if (iface_client.is_valid()) {
        iface_client_.Bind(std::move(iface_client));
        return;
      }
      configured.Wait();
      driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
        if (env.fake_dev_.has_value()) {
          env.fake_dev_->set_on_configured(nullptr);
        }
      });
    }
  }

  void SafeStopDriver() {
    if (driver_stopped_) {
      return;
    }
    driver_test_.runtime().RunUntilIdle();
    CancelAllUsbRequestsOnDeconfigure();
    EXPECT_TRUE(driver_test_.StopDriver().is_ok());
    driver_stopped_ = true;
  }

  void WaitForState(State target_state) {
    driver_test_.runtime().RunUntil([&]() {
      bool match = false;
      driver_test_.RunInDriverContext(
          [&](UsbAdbDevice& dev) { match = (UsbAdbTestHelper::state(dev) == target_state); });
      return match;
    });
  }

  void ExpectHandleOneEventSafe(fidl::WireSyncClient<fadb::UsbAdbImpl>& client,
                                EventHandler& handler) {
    while (true) {
      auto result = client.HandleOneEvent(handler);
      if (result.status() == ZX_ERR_PEER_CLOSED) {
        break;
      }
      ASSERT_EQ(result.status(), ZX_OK);
      if (handler.expected_statuses_.empty()) {
        break;
      }
    }
  }

  void SendTestData(fidl::WireSyncClient<fadb::UsbAdbImpl>& usb_impl, size_t size) {
    std::vector<uint8_t> test_data(size);

    driver_test_.RunInEnvironmentTypeContext([&test_data](UsbAdbEnvironment& env) {
      for (uint32_t i = 0; i < test_data.size() / kVmoDataSize; i++) {
        env.fake_dev_->fake_endpoint(kBulkInEp).RequestComplete(ZX_OK, kVmoDataSize);
      }
      if (test_data.size() % kVmoDataSize) {
        env.fake_dev_->fake_endpoint(kBulkInEp).RequestComplete(ZX_OK,
                                                                test_data.size() % kVmoDataSize);
      }
    });

    auto result = usb_impl->QueueTx(
        fidl::VectorView<uint8_t>::FromExternal(test_data.data(), test_data.size()));
    ASSERT_TRUE(result.ok());
    ASSERT_TRUE(result->is_ok());

    driver_test_.runtime().RunUntil([&] {
      bool empty = false;
      driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
        empty = (env.fake_dev_->fake_endpoint(kBulkInEp).pending_request_count() == 0u);
      });
      if (!empty) {
        return false;
      }
      driver_test_.RunInDriverContext([&](UsbAdbDevice& dev) {
        empty = (UsbAdbTestHelper::BulkInEpPendingRequests(dev) == 0u);
      });
      return empty;
    });
  }

  void ExpectReceiveData(size_t size) {
    driver_test_.RunInEnvironmentTypeContext([size](UsbAdbEnvironment& env) {
      env.fake_dev_->fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, size);
    });
  }

  // Simulates the host starting an adb session: the host's CNXN is passed to the client, and the
  // device replies. Defined below, after the adb header helpers.
  void StartHostSession(fidl::WireSyncClient<fadb::UsbAdbImpl>& usb_impl);

  // Safety net that drains any fake endpoint requests still outstanding at
  // deconfigure time. The driver now issues its own CancelAll() at the start of
  // teardown, which is what actually drains requests, and Deconfigure() only runs
  // once all requests have already been returned. This therefore matters only via
  // set_on_deconfigured()'s immediate-callback path, when a previous teardown has
  // already set deconfigured_.
  void CancelAllUsbRequestsOnDeconfigure() {
    driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
      if (env.fake_dev_.has_value()) {
        env.fake_dev_->set_on_deconfigured([&]() { env.CancelAllUsbRequests(); });
      }
    });
  }

  fdf_testing::BackgroundDriverTest<UsbAdbTestConfig> driver_test_;
  fidl::WireSyncClient<fadb::Device> client_;
  fidl::SyncClient<fuchsia_hardware_usb_function::UsbFunctionInterface> iface_client_;
  bool driver_stopped_ = false;
};

TEST_F(UsbAdbTest, StopBeforeUsbStartsUp) {
  driver_test_.runtime().RunUntil([&]() {
    bool awaiting = false;
    driver_test_.RunInDriverContext([&](UsbAdbDevice& dev) {
      awaiting = (UsbAdbTestHelper::state(dev) == State::kAwaitingUsbConnection);
    });
    return awaiting;
  });
  driver_test_.RunInDriverContext([&](UsbAdbDevice& dev) {
    EXPECT_EQ(UsbAdbTestHelper::state(dev), State::kAwaitingUsbConnection);
  });
  SafeStopDriver();
}

TEST_F(UsbAdbTest, StartStop) {
  auto [client_end, server_end] = fidl::Endpoints<fadb::UsbAdbImpl>::Create();
  EXPECT_TRUE(client_->StartAdb(std::move(server_end)).ok());
  auto usb_impl = fidl::WireSyncClient<fadb::UsbAdbImpl>(std::move(client_end));

  EventHandler handler;

  EnableUsb();

  handler.expected_statuses_.push(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl, handler);

  libsync::Completion stop_requested;
  driver_test_.RunInEnvironmentTypeContext([&stop_requested](UsbAdbEnvironment& env) {
    env.fake_dev_->set_on_deconfigured([&stop_requested]() { stop_requested.Signal(); });
  });

  std::atomic<bool> stop_finished = false;
  auto dev_client_end = client_.TakeClientEnd();
  fidl::WireClient async_client(std::move(dev_client_end),
                                fdf::Dispatcher::GetCurrent()->async_dispatcher());

  async_client->StopAdb().ThenExactlyOnce([&stop_finished](auto& result) {
    EXPECT_TRUE(result.ok() || result.status() == ZX_ERR_PEER_CLOSED);
    stop_finished = true;
  });

  driver_test_.RunInEnvironmentTypeContext(
      [&](UsbAdbEnvironment& env) { env.CancelAllUsbRequests(); });
  driver_test_.runtime().RunUntil([&]() { return stop_finished.load(); });

  driver_test_.RunInEnvironmentTypeContext(
      [&](UsbAdbEnvironment& env) { env.fake_dev_->set_on_deconfigured(nullptr); });

  usb_impl = {};
  async_client = {};

  SafeStopDriver();
}

TEST_F(UsbAdbTest, StopDriverWhileConnected) {
  auto usb_impl = NormalStartAdb();

  EventHandler handler;
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);

  ExpectHandleOneEventSafe(usb_impl, handler);

  CancelAllUsbRequestsOnDeconfigure();
  usb_impl = {};

  SafeStopDriver();
}

TEST_F(UsbAdbTest, UsbStackRequestsStop) {
  auto usb_impl = NormalStartAdb();

  EventHandler handler;
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);

  ExpectHandleOneEventSafe(usb_impl, handler);

  auto iface_client_end = iface_client_.TakeClientEnd();
  fidl::WireClient async_iface(std::move(iface_client_end),
                               fdf::Dispatcher::GetCurrent()->async_dispatcher());

  std::atomic<bool> set_configured_finished = false;
  async_iface->SetConfigured(false, fuchsia_hardware_usb_descriptor::wire::UsbSpeed::kFull)
      .ThenExactlyOnce([&](auto& result) {
        EXPECT_TRUE(result.ok() || result.status() == ZX_ERR_PEER_CLOSED);
        set_configured_finished = true;
      });

  driver_test_.RunInEnvironmentTypeContext(
      [](UsbAdbEnvironment& env) { env.CancelAllUsbRequests(); });

  handler.expected_statuses_.emplace(0);

  ExpectHandleOneEventSafe(usb_impl, handler);

  usb_impl = {};

  driver_test_.runtime().RunUntil([&] { return set_configured_finished.load(); });

  SafeStopDriver();
}

TEST_F(UsbAdbTest, SendAdbMessage) {
  auto usb_impl = NormalStartAdb();

  ASSERT_NO_FATAL_FAILURE(SendTestData(usb_impl, kVmoDataSize - 2));
  ASSERT_NO_FATAL_FAILURE(SendTestData(usb_impl, kVmoDataSize));
  ASSERT_NO_FATAL_FAILURE(SendTestData(usb_impl, kVmoDataSize + 2));
  ASSERT_NO_FATAL_FAILURE(SendTestData(usb_impl, kVmoDataSize * (kBulkTxCount - 2) + 2));
  ASSERT_NO_FATAL_FAILURE(SendTestData(usb_impl, kVmoDataSize * (kBulkTxCount - 1) + 2));

  SafeStopDriver();
}

TEST_F(UsbAdbTest, RecvAdbMessage) {
  constexpr uint32_t kReceiveSize = kVmoDataSize - 2;
  auto usb_impl = NormalStartAdb();

  auto dev_client_end = usb_impl.TakeClientEnd();
  fidl::WireClient async_usb(std::move(dev_client_end),
                             fdf::Dispatcher::GetCurrent()->async_dispatcher());

  std::atomic<bool> completed = false;
  size_t actual_size = 0;
  zx_status_t response_status = ZX_ERR_INTERNAL;
  async_usb->Receive().ThenExactlyOnce([&](auto& response) {
    completed = true;
    response_status = response.status();
    if (response.ok()) {
      actual_size = response.value().value()->data.size();
    }
  });

  driver_test_.runtime().RunUntilIdle();

  EXPECT_NO_FATAL_FAILURE(ExpectReceiveData(kReceiveSize));
  driver_test_.runtime().RunUntil([&] { return completed.load(); });
  EXPECT_EQ(response_status, ZX_OK);
  EXPECT_EQ(actual_size, kReceiveSize);

  SafeStopDriver();
}

// Returns an adb packet header (struct amessage) for |command| with a valid magic.
std::vector<uint8_t> AdbHeader(uint32_t command) {
  const uint32_t fields[] = {command, 0x01000001, 256 * 1024, 0, 0, command ^ 0xffffffff};
  std::vector<uint8_t> header(sizeof(fields));
  memcpy(header.data(), fields, sizeof(fields));
  return header;
}

constexpr uint32_t kAdbCnxn = 0x4e584e43;
constexpr uint32_t kAdbWrte = 0x45545257;

void UsbAdbTest::StartHostSession(fidl::WireSyncClient<fadb::UsbAdbImpl>& usb_impl) {
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, AdbHeader(kAdbCnxn));
  });
  auto result = usb_impl->Receive();
  ASSERT_TRUE(result.ok());
  ASSERT_TRUE(result->is_ok());
  ASSERT_EQ(result->value()->data.size(), AdbHeader(kAdbCnxn).size());

  ASSERT_NO_FATAL_FAILURE(SendTestData(usb_impl, 100));
}

TEST_F(UsbAdbTest, CnxnDuringHostSessionRestartsUsb) {
  auto usb_impl = NormalStartAdb();
  EventHandler handler;
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl, handler);

  // The host starts a session. Its CNXN is passed to the client as usual.
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, AdbHeader(kAdbCnxn));
  });
  auto first = usb_impl->Receive();
  ASSERT_TRUE(first.ok());
  ASSERT_TRUE(first->is_ok());
  EXPECT_EQ(first->value()->data.size(), AdbHeader(kAdbCnxn).size());

  // The device replies, which establishes the session.
  ASSERT_NO_FATAL_FAILURE(SendTestData(usb_impl, 100));

  // The host adb server restarts and sends a new CNXN without a USB disconnect. The driver restarts
  // USB: the client is told the device is offline and its connection is closed.
  handler.expected_statuses_.emplace(0);
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, AdbHeader(kAdbCnxn));
  });
  ExpectHandleOneEventSafe(usb_impl, handler);
  EXPECT_EQ(usb_impl.HandleOneEvent(handler).status(), ZX_ERR_PEER_CLOSED);

  WaitForState(State::kAwaitingUsbConnection);
  driver_test_.RunInEnvironmentTypeContext(
      [](UsbAdbEnvironment& env) { EXPECT_EQ(env.fake_dev_->deconfigure_count(), 1u); });

  usb_impl = {};
  SafeStopDriver();
}

// Returns |stale_bytes| of filler followed by |tail|, as the device receives it when the host's
// previous transfer was interrupted partway through a request.
std::vector<uint8_t> AfterStaleData(size_t stale_bytes, const std::vector<uint8_t>& tail) {
  std::vector<uint8_t> data(stale_bytes, 0xab);
  data.insert(data.end(), tail.begin(), tail.end());
  return data;
}

TEST_F(UsbAdbTest, CnxnAfterInterruptedTransferRestartsUsb) {
  auto usb_impl = NormalStartAdb();
  EventHandler handler;
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl, handler);

  // The host starts a session and the device replies.
  ASSERT_NO_FATAL_FAILURE(StartHostSession(usb_impl));

  // The host adb server is killed after two packets of a transfer, and the restarted server's CNXN
  // completes the same request. The driver still recognizes the CNXN and restarts USB.
  handler.expected_statuses_.emplace(0);
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp)
        .RequestComplete(ZX_OK, AfterStaleData(2 * kBulkMaxPacket, AdbHeader(kAdbCnxn)));
  });
  ExpectHandleOneEventSafe(usb_impl, handler);
  EXPECT_EQ(usb_impl.HandleOneEvent(handler).status(), ZX_ERR_PEER_CLOSED);

  WaitForState(State::kAwaitingUsbConnection);
  driver_test_.RunInEnvironmentTypeContext(
      [](UsbAdbEnvironment& env) { EXPECT_EQ(env.fake_dev_->deconfigure_count(), 1u); });

  usb_impl = {};
  SafeStopDriver();
}

TEST_F(UsbAdbTest, CnxnNotInItsOwnPacketIsDelivered) {
  auto usb_impl = NormalStartAdb();
  ASSERT_NO_FATAL_FAILURE(StartHostSession(usb_impl));

  // A CNXN header that does not start on a packet boundary, or that is followed by more data, was
  // not sent as its own packet, so it is payload and is passed to the client.
  std::vector<uint8_t> unaligned = AfterStaleData(100, AdbHeader(kAdbCnxn));
  std::vector<uint8_t> leading = AdbHeader(kAdbCnxn);
  leading.resize(kBulkMaxPacket + 100, 0xab);
  for (const auto& data : {unaligned, leading}) {
    driver_test_.RunInEnvironmentTypeContext([&data](UsbAdbEnvironment& env) {
      env.fake_dev_->fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, data);
    });
    auto result = usb_impl->Receive();
    ASSERT_TRUE(result.ok());
    ASSERT_TRUE(result->is_ok());
    EXPECT_EQ(result->value()->data.size(), data.size());
  }

  driver_test_.RunInDriverContext(
      [](UsbAdbDevice& dev) { EXPECT_EQ(UsbAdbTestHelper::state(dev), State::kOnline); });
  driver_test_.RunInEnvironmentTypeContext(
      [](UsbAdbEnvironment& env) { EXPECT_EQ(env.fake_dev_->deconfigure_count(), 0u); });

  SafeStopDriver();
}

TEST_F(UsbAdbTest, CnxnBeforeDeviceRepliesIsDelivered) {
  auto usb_impl = NormalStartAdb();

  // Until the device has sent anything, a repeated CNXN does not indicate a stale session and is
  // passed to the client.
  for (int i = 0; i < 2; i++) {
    driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
      env.fake_dev_->fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, AdbHeader(kAdbCnxn));
    });
    auto result = usb_impl->Receive();
    ASSERT_TRUE(result.ok());
    ASSERT_TRUE(result->is_ok());
    EXPECT_EQ(result->value()->data.size(), AdbHeader(kAdbCnxn).size());
  }

  driver_test_.RunInDriverContext(
      [](UsbAdbDevice& dev) { EXPECT_EQ(UsbAdbTestHelper::state(dev), State::kOnline); });
  driver_test_.RunInEnvironmentTypeContext(
      [](UsbAdbEnvironment& env) { EXPECT_EQ(env.fake_dev_->deconfigure_count(), 0u); });

  SafeStopDriver();
}

TEST_F(UsbAdbTest, OtherHeadersDuringHostSessionAreDelivered) {
  auto usb_impl = NormalStartAdb();
  ASSERT_NO_FATAL_FAILURE(StartHostSession(usb_impl));

  // Only CNXN restarts USB; other packet headers are passed to the client.
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, AdbHeader(kAdbWrte));
  });
  auto result = usb_impl->Receive();
  ASSERT_TRUE(result.ok());
  ASSERT_TRUE(result->is_ok());
  EXPECT_EQ(result->value()->data.size(), AdbHeader(kAdbWrte).size());

  driver_test_.RunInDriverContext(
      [](UsbAdbDevice& dev) { EXPECT_EQ(UsbAdbTestHelper::state(dev), State::kOnline); });
  driver_test_.RunInEnvironmentTypeContext(
      [](UsbAdbEnvironment& env) { EXPECT_EQ(env.fake_dev_->deconfigure_count(), 0u); });

  SafeStopDriver();
}

TEST_F(UsbAdbTest, OfflineTxDoesNotStartHostSession) {
  auto [client_end, server_end] = fidl::Endpoints<fadb::UsbAdbImpl>::Create();
  ASSERT_TRUE(client_->StartAdb(std::move(server_end)).ok());
  auto usb_impl = fidl::WireClient<fadb::UsbAdbImpl>(
      std::move(client_end), fdf::Dispatcher::GetCurrent()->async_dispatcher());

  // While offline, the client queues more data than fits in the bulk IN request pool, so some of
  // it is still being sent after the endpoints are enabled, from TxComplete(). A FIDL message is
  // limited to 64 KiB, so the data is queued as two writes that together need more requests than
  // the pool has.
  constexpr size_t kChunksPerWrite = kBulkTxCount / 2 + 1;
  constexpr size_t kOfflineWrites = 2;
  static_assert(kChunksPerWrite * kOfflineWrites > kBulkTxCount);
  std::vector<uint8_t> offline_data(kVmoDataSize * kChunksPerWrite, 0xaa);
  std::atomic<size_t> offline_tx_done = 0;
  std::atomic<size_t> offline_tx_ok = 0;
  for (size_t i = 0; i < kOfflineWrites; i++) {
    usb_impl
        ->QueueTx(fidl::VectorView<uint8_t>::FromExternal(offline_data.data(), offline_data.size()))
        .ThenExactlyOnce([&](auto& result) {
          if (result.ok() && result->is_ok()) {
            offline_tx_ok++;
          }
          offline_tx_done++;
        });
  }
  driver_test_.runtime().RunUntil([&]() {
    size_t count = 0;
    driver_test_.RunInDriverContext(
        [&](UsbAdbDevice& dev) { count = UsbAdbTestHelper::TxPendingReqsCount(dev); });
    return count == kOfflineWrites;
  });

  // Bring USB online, and let the host read everything queued while offline.
  WaitConfigured();
  EnableUsb();
  driver_test_.runtime().RunUntil([&]() {
    driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
      auto& bulk_in = env.fake_dev_->fake_endpoint(kBulkInEp);
      while (bulk_in.pending_request_count() > 0) {
        bulk_in.RequestComplete(ZX_OK, kVmoDataSize);
      }
    });
    return offline_tx_done.load() == kOfflineWrites;
  });
  EXPECT_EQ(offline_tx_ok.load(), kOfflineWrites);

  // The host has not sent anything yet, so the data sent so far does not mean that the device has
  // answered a host session. The host's first CNXN is passed to the client rather than treated as a
  // restarted host.
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, AdbHeader(kAdbCnxn));
  });
  std::atomic<bool> received = false;
  size_t received_size = 0;
  usb_impl->Receive().ThenExactlyOnce([&](auto& result) {
    if (result.ok() && result->is_ok()) {
      received_size = result->value()->data.size();
    }
    received = true;
  });
  driver_test_.runtime().RunUntil([&]() { return received.load(); });
  EXPECT_EQ(received_size, AdbHeader(kAdbCnxn).size());

  driver_test_.RunInDriverContext(
      [](UsbAdbDevice& dev) { EXPECT_EQ(UsbAdbTestHelper::state(dev), State::kOnline); });
  driver_test_.RunInEnvironmentTypeContext(
      [](UsbAdbEnvironment& env) { EXPECT_EQ(env.fake_dev_->deconfigure_count(), 0u); });

  SafeStopDriver();
}

TEST_F(UsbAdbTest, VerifyShutdownCompletesWhenHardwareUnresponsive) {
  auto usb_impl = NormalStartAdb();
  EventHandler handler;
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl, handler);

  auto iface_client_end = iface_client_.TakeClientEnd();
  ASSERT_TRUE(iface_client_end.is_valid());
  fidl::WireClient async_iface(std::move(iface_client_end),
                               fdf::Dispatcher::GetCurrent()->async_dispatcher());

  CancelAllUsbRequestsOnDeconfigure();

  // Simulate hardware power-off or inactive controller endpoints where CancelAll
  // returns ZX_ERR_IO_NOT_PRESENT. This is armed *before* teardown is triggered so the
  // driver's CancelAll() deterministically observes the failure. Arming it afterwards
  // would race the background dispatcher, which usually processes SetConfigured(false)
  // first and therefore exercises the ZX_OK path instead.
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).set_cancel_status(ZX_ERR_IO_NOT_PRESENT);
    env.fake_dev_->fake_endpoint(kBulkInEp).set_cancel_status(ZX_ERR_IO_NOT_PRESENT);
  });

  // The driver should reply ZX_OK to held SetConfigured(false) completers before Deconfigure()
  // and complete shutdown cleanly.
  std::atomic<bool> set_configured_replied = false;
  async_iface->SetConfigured(false, fuchsia_hardware_usb_descriptor::wire::UsbSpeed::kHigh)
      .ThenExactlyOnce([&](auto& result) {
        EXPECT_TRUE(result.ok());
        EXPECT_TRUE(result->is_ok());
        set_configured_replied = true;
      });

  driver_test_.runtime().RunUntilIdle();

  EXPECT_TRUE(driver_test_.StopDriver().is_ok());
  driver_test_.runtime().RunUntil([&]() { return set_configured_replied.load(); });
  driver_stopped_ = true;
}

TEST_F(UsbAdbTest, VerifyInspect) {
  auto usb_impl = NormalStartAdb();

  // Queue some tx packets
  ASSERT_NO_FATAL_FAILURE(SendTestData(usb_impl, 100));

  auto dev_client_end = usb_impl.TakeClientEnd();
  fidl::WireClient async_usb(std::move(dev_client_end),
                             fdf::Dispatcher::GetCurrent()->async_dispatcher());

  std::atomic<bool> completed = false;
  size_t actual_size = 0;
  zx_status_t response_status = ZX_ERR_INTERNAL;
  async_usb->Receive().ThenExactlyOnce([&](auto& response) {
    completed = true;
    response_status = response.status();
    if (response.ok()) {
      actual_size = response.value().value()->data.size();
    }
  });

  driver_test_.runtime().RunUntilIdle();

  EXPECT_NO_FATAL_FAILURE(ExpectReceiveData(200));
  driver_test_.runtime().RunUntil([&] { return completed.load(); });
  EXPECT_EQ(response_status, ZX_OK);
  EXPECT_EQ(actual_size, 200u);

  // Fetch inspector from driver
  inspect::Inspector inspector;
  driver_test_.RunInDriverContext([&](UsbAdbDevice& dev) {
    if (auto* tracker = UsbAdbTestHelper::GetThroughputTracker(dev)) {
      tracker->MeasureForTesting(zx::sec(1));
    }
    inspector = UsbAdbTestHelper::GetInspector(dev);
  });

  ASSERT_NO_FATAL_FAILURE(ReadInspect(inspector));

  // Verify adb function inspect node exists
  auto* root_node = this->hierarchy().GetByPath({"usb-adb-function"});
  ASSERT_NE(nullptr, root_node);

  // Verify state string
  const auto* state_prop = root_node->node().get_property<inspect::StringPropertyValue>("state");
  ASSERT_NE(nullptr, state_prop);
  EXPECT_EQ("kOnline", state_prop->value());

  // Verify online boolean property
  const auto* online_prop = root_node->node().get_property<inspect::BoolPropertyValue>("online");
  ASSERT_NE(nullptr, online_prop);
  EXPECT_TRUE(online_prop->value());

  // Verify bulk_in (TX) stats using the shared helper
  auto* bulk_in = this->hierarchy().GetByPath({"usb-adb-function", "bulk_in"});
  ASSERT_NE(nullptr, bulk_in);
  auto err_in =
      usb_inspect::VerifyEndpointInspect(bulk_in, 100, std::nullopt, 0, std::nullopt, 100, 0);
  EXPECT_TRUE(err_in.is_ok()) << err_in.error_value();

  // Verify bulk_out (RX) stats using the shared helper
  auto* bulk_out = this->hierarchy().GetByPath({"usb-adb-function", "bulk_out"});
  ASSERT_NE(nullptr, bulk_out);
  auto err_out =
      usb_inspect::VerifyEndpointInspect(bulk_out, std::nullopt, 200, std::nullopt, 0, 200, 0);
  EXPECT_TRUE(err_out.is_ok()) << err_out.error_value();

  // Verify function-level lifecycle events are logged under usb-adb-function/event_history
  auto* event_history = this->hierarchy().GetByPath({"usb-adb-function", "event_history"});
  ASSERT_NE(nullptr, event_history);
  EXPECT_GT(event_history->children().size(), 0u);

  async_usb = {};
}

TEST_F(UsbAdbTest, QueueTxZeroLengthRejected) {
  // TODO(b/395982869): Move this test to a separate error-expecting test file/suite after all
  // this is done so we don't have to degrade the max_severity checks for other tests.
  auto usb_impl_sync = NormalStartAdb();
  auto dev_client_end = usb_impl_sync.TakeClientEnd();
  fidl::WireClient async_usb(std::move(dev_client_end),
                             fdf::Dispatcher::GetCurrent()->async_dispatcher());

  // Queue a zero-length vector, which should be rejected.
  std::atomic<bool> completed = false;
  async_usb->QueueTx(fidl::VectorView<uint8_t>()).ThenExactlyOnce([&](auto& result) {
    ASSERT_TRUE(result.ok());
    ASSERT_TRUE(result->is_error());
    EXPECT_EQ(result->error_value(), ZX_ERR_INVALID_ARGS);
    completed = true;
  });

  driver_test_.runtime().RunUntil([&]() { return completed.load(); });

  async_usb = {};
}

TEST_F(UsbAdbTest, OfflineRxQueuedAndServicedOnline) {
  auto [client_end, server_end] = fidl::Endpoints<fadb::UsbAdbImpl>::Create();
  EXPECT_TRUE(client_->StartAdb(std::move(server_end)).ok());
  auto usb_impl =
      fidl::WireClient(std::move(client_end), fdf::Dispatcher::GetCurrent()->async_dispatcher());

  constexpr uint32_t kReceiveSize = 128;
  std::atomic<bool> rx_completed = false;
  usb_impl->Receive().ThenExactlyOnce([&](auto& response) {
    ASSERT_TRUE(response.ok());
    ASSERT_TRUE(response.value().is_ok());
    EXPECT_EQ(response.value().value()->data.size(), kReceiveSize);
    rx_completed = true;
  });

  // Wait for the driver to queue the Receive request in its software queue.
  driver_test_.runtime().RunUntil([&]() {
    size_t count = 0;
    driver_test_.RunInDriverContext(
        [&](UsbAdbDevice& dev) { count = UsbAdbTestHelper::RxRequestsCount(dev); });
    return count == 1;
  });

  // Bring USB online.
  WaitConfigured();
  EnableUsb();

  // Trigger RX complete at the hardware layer.
  driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).RequestComplete(ZX_OK, kReceiveSize);
  });

  // Wait for the background thread to finish.
  driver_test_.runtime().RunUntil([&]() { return rx_completed.load(); });

  usb_impl = {};
}

TEST_F(UsbAdbTest, StartAdbTwiceFails) {
  auto usb_impl = NormalStartAdb();

  // Try to start ADB again with a new channel. It should fail with ZX_ERR_ALREADY_BOUND.
  auto [client_end, server_end] = fidl::Endpoints<fadb::UsbAdbImpl>::Create();
  auto result = client_->StartAdb(std::move(server_end));
  ASSERT_TRUE(result.ok());
  ASSERT_TRUE(result->is_error());
  EXPECT_EQ(result->error_value(), ZX_ERR_ALREADY_BOUND);
}

TEST_F(UsbAdbTest, IsConfiguredReflectsDeconfigureState) {
  auto usb_impl = NormalStartAdb();
  driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
    EXPECT_TRUE(env.fake_dev_->is_configured());
    EXPECT_EQ(env.fake_dev_->deconfigure_count(), 0u);
    EXPECT_TRUE(env.fake_dev_->disabled_endpoints().empty());
  });

  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).set_hold_cancel(true);
    env.fake_dev_->fake_endpoint(kBulkInEp).set_hold_cancel(true);
  });

  // Trigger ADB-initiated USB reset via StopAdb().
  auto dev_client_end = client_.TakeClientEnd();
  fidl::WireClient async_client(std::move(dev_client_end),
                                fdf::Dispatcher::GetCurrent()->async_dispatcher());
  std::atomic<bool> stop_done = false;
  async_client->StopAdb().ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.ok());
    stop_done = true;
  });
  WaitForState(State::kStoppingForReconnect);

  // 1. DisableEndpoint should have been called immediately upon stop request:
  driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
    auto eps = env.fake_dev_->disabled_endpoints();
    EXPECT_EQ(eps.size(), 2u);
  });

  // 2. Deconfigure() should NOT have been called yet while CancelAll is pending:
  driver_test_.RunInEnvironmentTypeContext(
      [&](UsbAdbEnvironment& env) { EXPECT_EQ(env.fake_dev_->deconfigure_count(), 0u); });
  EXPECT_FALSE(stop_done.load());

  // 3. Release CancelAll:
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).ReleaseCancelAll();
    env.fake_dev_->fake_endpoint(kBulkInEp).ReleaseCancelAll();
    env.fake_dev_->fake_endpoint(kBulkOutEp).set_hold_cancel(false);
    env.fake_dev_->fake_endpoint(kBulkInEp).set_hold_cancel(false);
    env.CancelAllUsbRequests();
  });

  // 4. Now Deconfigure() should have been called and StopAdb() completed:
  driver_test_.runtime().RunUntil([&]() {
    size_t count = 0;
    driver_test_.RunInEnvironmentTypeContext(
        [&](UsbAdbEnvironment& env) { count = env.fake_dev_->deconfigure_count(); });
    return count >= 1 && stop_done.load();
  });
  async_client = {};
}

TEST_F(UsbAdbTest, OfflineTxQueuedAndFlushedOnline) {
  auto [client_end, server_end] = fidl::Endpoints<fadb::UsbAdbImpl>::Create();
  ASSERT_TRUE(client_->StartAdb(std::move(server_end)).ok());
  auto usb_impl = fidl::WireClient<fadb::UsbAdbImpl>(
      std::move(client_end), fdf::Dispatcher::GetCurrent()->async_dispatcher());

  // Queue a TX request while offline.
  std::vector<uint8_t> test_data(100, 0xAA);
  std::atomic<bool> tx_completed = false;
  usb_impl->QueueTx(fidl::VectorView<uint8_t>::FromExternal(test_data.data(), test_data.size()))
      .ThenExactlyOnce([&](fidl::WireUnownedResult<fadb::UsbAdbImpl::QueueTx>& result) {
        if (result.ok() && result->is_ok()) {
          tx_completed = true;
        }
      });

  // Wait for the driver to receive and queue the request in its software queue.
  driver_test_.runtime().RunUntil([&]() {
    size_t count = 0;
    driver_test_.RunInDriverContext(
        [&](UsbAdbDevice& dev) { count = UsbAdbTestHelper::TxPendingReqsCount(dev); });
    return count == 1;
  });

  // Verify that nothing has been queued to the hardware yet.
  driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
    EXPECT_EQ(env.fake_dev_->fake_endpoint(kBulkInEp).pending_request_count(), 0u);
  });

  // Now bring USB online.
  WaitConfigured();
  EnableUsb();

  // The driver should now automatically dequeue the request and queue it to bulk IN endpoint.
  driver_test_.runtime().RunUntil([&]() {
    size_t pending_endpoints = 0;
    driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
      pending_endpoints = env.fake_dev_->fake_endpoint(kBulkInEp).pending_request_count();
    });
    return pending_endpoints == 1;
  });

  // Complete the request at the hardware layer.
  driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkInEp).RequestComplete(ZX_OK, test_data.size());
  });

  // Wait for the FIDL callback to finish.
  driver_test_.runtime().RunUntil([&]() { return tx_completed.load(); });
}

TEST_F(UsbAdbTest, UsbFunctionBindingReconfigure) {
  // Establish an initial ADB connection
  auto [client_end1, server_end1] = fidl::Endpoints<fadb::UsbAdbImpl>::Create();
  auto result1 = client_->StartAdb(std::move(server_end1));
  ASSERT_TRUE(result1.ok());

  // Wait for iface_client_ to become valid
  ASSERT_NO_FATAL_FAILURE(EnsureIfaceBound());

  auto config_result_first = iface_client_->SetConfigured({{
      .configured = true,
      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kHigh,
  }});
  ASSERT_TRUE(config_result_first.is_ok());

  auto deconfig_result = iface_client_->SetConfigured({{
      .configured = false,
      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kHigh,
  }});
  ASSERT_TRUE(deconfig_result.is_ok());

  // SetConfigured(false) must transition cleanly to kAwaitingUsbConnection without tearing down
  // usb_function_binding_ or calling Deconfigure().
  WaitForState(State::kAwaitingUsbConnection);
  driver_test_.RunInEnvironmentTypeContext(
      [](UsbAdbEnvironment& env) { EXPECT_EQ(env.fake_dev_->deconfigure_count(), 0u); });

  // Re-configuring on the same iface_client_ must succeed.
  auto config_result = iface_client_->SetConfigured({{
      .configured = true,
      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kHigh,
  }});
  ASSERT_TRUE(config_result.is_ok());

  driver_test_.runtime().RunUntilIdle();

  // Verify that driver is online AND usb_function_binding_ remains bound.
  driver_test_.RunInDriverContext([&](UsbAdbDevice& dev) {
    EXPECT_EQ(UsbAdbTestHelper::state(dev), State::kOnline);
    EXPECT_TRUE(UsbAdbTestHelper::has_usb_function_binding(dev));
  });

  // Verify the channel to usb_function_binding_ is still live by making a synchronous call.
  auto ping_result = iface_client_->SetConfigured({{
      .configured = true,
      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kHigh,
  }});
  EXPECT_TRUE(ping_result.is_ok());
  ASSERT_NO_FATAL_FAILURE(SafeStopDriver());
}

TEST_F(UsbAdbTest, DisconnectTransitionsToStoppingState) {
  auto usb_impl = NormalStartAdb();
  EventHandler handler;
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl, handler);

  // Disable cancellation so the fake endpoints hold onto the requests during stopping.
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).set_hold_cancel(true);
    env.fake_dev_->fake_endpoint(kBulkInEp).set_hold_cancel(true);
  });

  // Deconfiguring USB transitions driver state to kStoppingForReconnect and holds the
  // SetConfigured(false) completer until CancelAll finishes.
  auto iface_client_end = iface_client_.TakeClientEnd();
  fidl::WireClient async_iface(std::move(iface_client_end),
                               fdf::Dispatcher::GetCurrent()->async_dispatcher());
  std::atomic<bool> unconfigured_done = false;
  async_iface->SetConfigured(false, fuchsia_hardware_usb_descriptor::wire::UsbSpeed::kHigh)
      .ThenExactlyOnce([&](auto& result) {
        EXPECT_TRUE(result.ok());
        EXPECT_TRUE(result->is_ok());
        unconfigured_done = true;
      });

  WaitForState(State::kStoppingForReconnect);
  EXPECT_FALSE(unconfigured_done.load());

  // Release cancellation so the driver can finish stopping cleanly.
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).ReleaseCancelAll();
    env.fake_dev_->fake_endpoint(kBulkInEp).ReleaseCancelAll();
    env.fake_dev_->fake_endpoint(kBulkOutEp).set_hold_cancel(false);
    env.fake_dev_->fake_endpoint(kBulkInEp).set_hold_cancel(false);
    env.CancelAllUsbRequests();
  });

  driver_test_.runtime().RunUntil([&]() { return unconfigured_done.load(); });
  async_iface = {};

  ASSERT_NO_FATAL_FAILURE(SafeStopDriver());
}

TEST_F(UsbAdbTest, TeardownWhileRequestsPending) {
  auto usb_impl = NormalStartAdb();
  EventHandler handler;
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl, handler);

  // Disable cancellation so the fake endpoints hold onto the requests.
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).set_hold_cancel(true);
    env.fake_dev_->fake_endpoint(kBulkInEp).set_hold_cancel(true);
  });

  // Initiate disconnect/reset while hold is active to set up pending CancelAll requests.
  usb_impl = {};
  iface_client_ = {};

  WaitForState(State::kStoppingForReconnect);

  // Verify the driver is waiting for CancelAll and in stopping state:
  driver_test_.RunInDriverContext([&](UsbAdbDevice& dev) {
    EXPECT_FALSE(UsbAdbTestHelper::CancelAllCompleted(dev));
    EXPECT_EQ(UsbAdbTestHelper::state(dev), State::kStoppingForReconnect);
  });

  // Release cancellation so the driver receives the CancelAll callbacks during unbind.
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).ReleaseCancelAll();
    env.fake_dev_->fake_endpoint(kBulkInEp).ReleaseCancelAll();
    env.fake_dev_->fake_endpoint(kBulkOutEp).set_hold_cancel(false);
    env.fake_dev_->fake_endpoint(kBulkInEp).set_hold_cancel(false);
    env.CancelAllUsbRequests();
  });

  ASSERT_NO_FATAL_FAILURE(SafeStopDriver());
}

TEST_F(UsbAdbTest, InspectStateTransitions) {
  inspect::Inspector inspector;
  driver_test_.RunInDriverContext(
      [&](UsbAdbDevice& dev) { inspector = UsbAdbTestHelper::GetInspector(dev); });
  ASSERT_NO_FATAL_FAILURE(ReadInspect(inspector));
  auto* root_node = this->hierarchy().GetByPath({"usb-adb-function"});
  ASSERT_NE(nullptr, root_node);
  const auto* state_prop = root_node->node().get_property<inspect::StringPropertyValue>("state");
  ASSERT_NE(nullptr, state_prop);
  EXPECT_EQ("kAwaitingUsbConnection", state_prop->value());
  const auto* online_prop = root_node->node().get_property<inspect::BoolPropertyValue>("online");
  ASSERT_NE(nullptr, online_prop);
  EXPECT_FALSE(online_prop->value());

  auto usb_impl = NormalStartAdb();
  driver_test_.RunInDriverContext(
      [&](UsbAdbDevice& dev) { inspector = UsbAdbTestHelper::GetInspector(dev); });
  ASSERT_NO_FATAL_FAILURE(ReadInspect(inspector));
  root_node = this->hierarchy().GetByPath({"usb-adb-function"});
  ASSERT_NE(nullptr, root_node);
  state_prop = root_node->node().get_property<inspect::StringPropertyValue>("state");
  ASSERT_NE(nullptr, state_prop);
  EXPECT_EQ("kOnline", state_prop->value());
  online_prop = root_node->node().get_property<inspect::BoolPropertyValue>("online");
  ASSERT_NE(nullptr, online_prop);
  EXPECT_TRUE(online_prop->value());

  ASSERT_NO_FATAL_FAILURE(SafeStopDriver());
}

TEST_F(UsbAdbTest, InspectLifecycleEvents) {
  auto has_function_event = [&](const std::string& expected_event) {
    const auto* event_history = this->hierarchy().GetByPath({"usb-adb-function", "event_history"});
    if (event_history == nullptr) {
      return false;
    }
    for (const auto& child : event_history->children()) {
      const auto* time_prop = child.node().get_property<inspect::UintPropertyValue>("@time");
      const auto* event_prop = child.node().get_property<inspect::StringPropertyValue>("event");
      if (time_prop != nullptr && event_prop != nullptr && event_prop->value() == expected_event) {
        return true;
      }
    }
    return false;
  };

  auto has_endpoint_event = [&](const char* endpoint_name, const std::string& expected_event) {
    const auto* event_history =
        this->hierarchy().GetByPath({"usb-adb-function", endpoint_name, "event_history"});
    if (event_history == nullptr) {
      return false;
    }
    for (const auto& child : event_history->children()) {
      const auto* time_prop = child.node().get_property<inspect::UintPropertyValue>("@time");
      const auto* event_prop = child.node().get_property<inspect::StringPropertyValue>("event");
      if (time_prop != nullptr && event_prop != nullptr && event_prop->value() == expected_event) {
        return true;
      }
    }
    return false;
  };

  auto expect_lifecycle_event = [&](const std::string& expected_event) {
    inspect::Inspector inspector;
    driver_test_.RunInDriverContext(
        [&](UsbAdbDevice& dev) { inspector = UsbAdbTestHelper::GetInspector(dev); });
    ASSERT_NO_FATAL_FAILURE(ReadInspect(inspector));
    EXPECT_TRUE(has_function_event(expected_event))
        << "Missing event '" << expected_event << "' in usb-adb-function/event_history";
    EXPECT_FALSE(has_endpoint_event("bulk_in", expected_event))
        << "Function event '" << expected_event << "' unexpectedly duplicated in bulk_in";
    EXPECT_FALSE(has_endpoint_event("bulk_out", expected_event))
        << "Function event '" << expected_event << "' unexpectedly duplicated in bulk_out";
  };

  // 1. StartAdb logs "adb_client_connected" and configuring USB logs "SetConfigured(true)".
  auto usb_impl = NormalStartAdb();
  EventHandler handler;
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl, handler);
  ASSERT_NO_FATAL_FAILURE(expect_lifecycle_event("adb_client_connected"));
  ASSERT_NO_FATAL_FAILURE(expect_lifecycle_event("SetConfigured(true)"));

  // 2. SetConfigured(false) while kOnline logs unconfigure milestones on the function node
  // and per-endpoint "CancelAll complete" on bulk_in and bulk_out.
  auto deconfig_result = iface_client_->SetConfigured({{
      .configured = false,
      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kHigh,
  }});
  ASSERT_TRUE(deconfig_result.is_ok());
  WaitForState(State::kAwaitingUsbConnection);
  ASSERT_NO_FATAL_FAILURE(expect_lifecycle_event("usb_bus_disconnected: SetConfigured(false)"));
  ASSERT_NO_FATAL_FAILURE(expect_lifecycle_event("SetConfigured(false) start"));
  ASSERT_NO_FATAL_FAILURE(expect_lifecycle_event("DisableEndpoints complete"));
  ASSERT_NO_FATAL_FAILURE(expect_lifecycle_event("SetConfigured(false) reply"));
  EXPECT_TRUE(has_endpoint_event("bulk_in", "CancelAll complete"));
  EXPECT_TRUE(has_endpoint_event("bulk_out", "CancelAll complete"));

  {
    inspect::Inspector inspector;
    driver_test_.RunInDriverContext(
        [&](UsbAdbDevice& dev) { inspector = UsbAdbTestHelper::GetInspector(dev); });
    ASSERT_NO_FATAL_FAILURE(ReadInspect(inspector));
    const auto* adb_node = hierarchy_->GetByPath({"usb-adb-function"});
    ASSERT_NE(adb_node, nullptr);
    const auto* cfg_prop = adb_node->node().get_property<inspect::BoolPropertyValue>("configured");
    ASSERT_NE(cfg_prop, nullptr);
    EXPECT_FALSE(cfg_prop->value());
    const auto* uncfg_prop =
        adb_node->node().get_property<inspect::BoolPropertyValue>("unconfiguring");
    ASSERT_NE(uncfg_prop, nullptr);
    EXPECT_FALSE(uncfg_prop->value());
    const auto* true_cnt =
        adb_node->node().get_property<inspect::UintPropertyValue>("set_configured_true_count");
    ASSERT_NE(true_cnt, nullptr);
    EXPECT_EQ(true_cnt->value(), 1u);
    const auto* false_cnt =
        adb_node->node().get_property<inspect::UintPropertyValue>("set_configured_false_count");
    ASSERT_NE(false_cnt, nullptr);
    EXPECT_EQ(false_cnt->value(), 1u);

    for (const char* ep_name : {"bulk_in", "bulk_out"}) {
      const auto* ep_node = hierarchy_->GetByPath({"usb-adb-function", ep_name});
      ASSERT_NE(ep_node, nullptr) << ep_name;
      const auto* canc = ep_node->node().get_property<inspect::BoolPropertyValue>("cancelled");
      ASSERT_NE(canc, nullptr) << ep_name;
      EXPECT_TRUE(canc->value());
      const auto* inflight =
          ep_node->node().get_property<inspect::UintPropertyValue>("in_flight_requests");
      ASSERT_NE(inflight, nullptr) << ep_name;
      EXPECT_EQ(inflight->value(), 0u);
      const auto* total =
          ep_node->node().get_property<inspect::UintPropertyValue>("total_requests");
      ASSERT_NE(total, nullptr) << ep_name;
      EXPECT_GT(total->value(), 0u);
    }
  }

  // 3. StopAdb while kOnline logs both "adb_client_disconnected: StopAdb" and "reset_usb_on_stop".
  // SetConfigured(false) in step 2 keeps both usb_function_binding_ and adb_binding_ bound,
  // so drain the offline status event and re-enable USB on the existing bindings.
  handler.expected_statuses_.emplace(fadb::StatusFlags(0));
  ExpectHandleOneEventSafe(usb_impl, handler);
  EnableUsb();
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl, handler);

  // Hold CancelAll briefly so we also exercise connecting a new ADB client while stopping for
  // reconnect ("adb_client_connected_during_stop").
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).set_hold_cancel(true);
    env.fake_dev_->fake_endpoint(kBulkInEp).set_hold_cancel(true);
  });

  std::atomic<bool> stop_done = false;
  auto dev_client_end = client_.TakeClientEnd();
  fidl::WireClient async_client(std::move(dev_client_end),
                                fdf::Dispatcher::GetCurrent()->async_dispatcher());
  async_client->StopAdb().ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.ok());
    stop_done = true;
  });
  WaitForState(State::kStoppingForReconnect);
  ASSERT_NO_FATAL_FAILURE(expect_lifecycle_event("adb_client_disconnected: StopAdb"));
  ASSERT_NO_FATAL_FAILURE(expect_lifecycle_event("reset_usb_on_stop"));

  auto [client_end_during_stop, server_end_during_stop] =
      fidl::Endpoints<fadb::UsbAdbImpl>::Create();
  std::atomic<bool> start_during_stop_done = false;
  async_client->StartAdb(std::move(server_end_during_stop)).ThenExactlyOnce([&](auto& result) {
    EXPECT_TRUE(result.ok());
    start_during_stop_done = true;
  });
  driver_test_.runtime().RunUntil([&]() { return start_during_stop_done.load(); });
  ASSERT_NO_FATAL_FAILURE(expect_lifecycle_event("adb_client_connected_during_stop"));

  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).ReleaseCancelAll();
    env.fake_dev_->fake_endpoint(kBulkInEp).ReleaseCancelAll();
    env.fake_dev_->fake_endpoint(kBulkOutEp).set_hold_cancel(false);
    env.fake_dev_->fake_endpoint(kBulkInEp).set_hold_cancel(false);
    env.CancelAllUsbRequests();
  });
  driver_test_.runtime().RunUntil([&]() { return stop_done.load(); });
  WaitForState(State::kAwaitingUsbConnection);
  async_client = {};
  client_.Bind(*driver_test_.Connect<fadb::Service::Adb>());
  client_end_during_stop.reset();
  driver_test_.runtime().RunUntil([&]() {
    bool bound = true;
    driver_test_.RunInDriverContext(
        [&](UsbAdbDevice& dev) { bound = UsbAdbTestHelper::has_adb_binding(dev); });
    return !bound;
  });

  // 4. RxComplete / TxComplete observing ZX_ERR_IO_NOT_PRESENT while kOnline logs
  // "usb_disconnect_observed: RxComplete" / "usb_disconnect_observed: TxComplete", and
  // closing UsbAdbImpl when configure subsequently fails logs "restarting_usb_connection_failed".
  iface_client_ = {};
  usb_impl = NormalStartAdb();
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl, handler);

  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).RequestComplete(ZX_ERR_IO_NOT_PRESENT, 0);
  });
  driver_test_.runtime().RunUntil([&]() {
    inspect::Inspector inspector;
    driver_test_.RunInDriverContext(
        [&](UsbAdbDevice& dev) { inspector = UsbAdbTestHelper::GetInspector(dev); });
    ReadInspect(inspector);
    return has_function_event("usb_disconnect_observed: RxComplete");
  });

  // Reset usb_disconnect_observed_ via SetConfigured(false) -> SetConfigured(true), then queue a TX
  // packet and fail it with ZX_ERR_IO_NOT_PRESENT to verify "usb_disconnect_observed: TxComplete".
  ASSERT_TRUE(iface_client_
                  ->SetConfigured({{
                      .configured = false,
                      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kHigh,
                  }})
                  .is_ok());
  WaitForState(State::kAwaitingUsbConnection);
  handler.expected_statuses_.emplace(fadb::StatusFlags(0));
  ExpectHandleOneEventSafe(usb_impl, handler);

  EnableUsb();
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl, handler);

  uint8_t tx_byte = 0x55;
  auto async_usb = fidl::WireClient<fadb::UsbAdbImpl>(
      usb_impl.TakeClientEnd(), fdf::Dispatcher::GetCurrent()->async_dispatcher());
  async_usb->QueueTx(fidl::VectorView<uint8_t>::FromExternal(&tx_byte, 1))
      .ThenExactlyOnce([](auto&) {});
  driver_test_.runtime().RunUntil([&]() {
    size_t pending = 0;
    driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
      pending = env.fake_dev_->fake_endpoint(kBulkInEp).pending_request_count();
    });
    return pending == 1u;
  });
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkInEp).RequestComplete(ZX_ERR_IO_NOT_PRESENT, 0);
  });
  driver_test_.runtime().RunUntil([&]() {
    inspect::Inspector inspector;
    driver_test_.RunInDriverContext(
        [&](UsbAdbDevice& dev) { inspector = UsbAdbTestHelper::GetInspector(dev); });
    ReadInspect(inspector);
    return has_function_event("usb_disconnect_observed: TxComplete");
  });

  // Re-cycle SetConfigured(false)/(true) so usb_disconnect_observed_ is false, close
  // iface_client_ while bound to verify "usb_function_disconnected: unbind", inject Configure
  // failure, and close async_usb to verify "adb_client_disconnected: unbind" +
  // "restarting_usb_connection_failed".
  ASSERT_TRUE(iface_client_
                  ->SetConfigured({{
                      .configured = false,
                      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kHigh,
                  }})
                  .is_ok());
  WaitForState(State::kAwaitingUsbConnection);
  EnableUsb();
  iface_client_ = {};
  driver_test_.runtime().RunUntil([&]() {
    inspect::Inspector inspector;
    driver_test_.RunInDriverContext(
        [&](UsbAdbDevice& dev) { inspector = UsbAdbTestHelper::GetInspector(dev); });
    ReadInspect(inspector);
    return has_function_event("usb_function_disconnected: unbind");
  });

  driver_test_.RunInEnvironmentTypeContext(
      [](UsbAdbEnvironment& env) { env.fake_dev_->set_configure_status(ZX_ERR_BAD_STATE); });

  async_usb = {};
  WaitForState(State::kAwaitingUsbConnection);
  ASSERT_NO_FATAL_FAILURE(expect_lifecycle_event("adb_client_disconnected: unbind"));
  ASSERT_NO_FATAL_FAILURE(expect_lifecycle_event("restarting_usb_connection_failed"));

  // 5. Stopping the driver logs "driver_stopped".
  inspect::Inspector inspector;
  driver_test_.RunInDriverContext(
      [&](UsbAdbDevice& dev) { inspector = UsbAdbTestHelper::GetInspector(dev); });
  ASSERT_NO_FATAL_FAILURE(SafeStopDriver());
  ASSERT_NO_FATAL_FAILURE(ReadInspect(inspector));
  EXPECT_TRUE(has_function_event("driver_stopped"));
}

TEST_F(UsbAdbTest, StartAdbFailsWhileStopping) {
  driver_test_.RunInDriverContext(
      [](UsbAdbDevice& dev) { UsbAdbTestHelper::set_state(dev, State::kStoppingForUnbind); });

  auto [client_end, server_end] = fidl::Endpoints<fadb::UsbAdbImpl>::Create();
  auto result = client_->StartAdb(std::move(server_end));
  EXPECT_TRUE(result.ok());
  EXPECT_TRUE(result->is_error());
  EXPECT_EQ(result->error_value(), ZX_ERR_BAD_STATE);

  driver_test_.RunInDriverContext(
      [](UsbAdbDevice& dev) { UsbAdbTestHelper::set_state(dev, State::kAwaitingUsbConnection); });
  ASSERT_NO_FATAL_FAILURE(SafeStopDriver());
}

TEST_F(UsbAdbTest, OperationsFailWhileStopping) {
  auto [client_end, server_end] = fidl::Endpoints<fadb::UsbAdbImpl>::Create();
  auto result = client_->StartAdb(std::move(server_end));
  EXPECT_TRUE(result.ok());

  fidl::WireSyncClient<fadb::UsbAdbImpl> adb_client{std::move(client_end)};

  driver_test_.RunInDriverContext(
      [](UsbAdbDevice& dev) { UsbAdbTestHelper::set_state(dev, State::kStoppingForUnbind); });

  std::vector<uint8_t> data(1);
  auto tx_result =
      adb_client->QueueTx(fidl::VectorView<uint8_t>::FromExternal(data.data(), data.size()));
  EXPECT_TRUE(tx_result.ok());
  EXPECT_TRUE(tx_result->is_error());
  EXPECT_EQ(tx_result->error_value(), ZX_ERR_BAD_STATE);

  auto rx_result = adb_client->Receive();
  EXPECT_TRUE(rx_result.ok());
  EXPECT_TRUE(rx_result->is_error());
  EXPECT_EQ(rx_result->error_value(), ZX_ERR_BAD_STATE);

  driver_test_.RunInDriverContext(
      [](UsbAdbDevice& dev) { UsbAdbTestHelper::set_state(dev, State::kAwaitingUsbConnection); });
  ASSERT_NO_FATAL_FAILURE(SafeStopDriver());
}

TEST_F(UsbAdbTest, EarlyConnectRace) {
  auto [client_end, server_end] = fidl::Endpoints<fadb::UsbAdbImpl>::Create();
  EXPECT_TRUE(client_->StartAdb(std::move(server_end)).ok());

  uint8_t test_data[] = {0x01, 0x02, 0x03};

  fidl::WireClient async_usb(std::move(client_end),
                             fdf::Dispatcher::GetCurrent()->async_dispatcher());

  bool completed = false;
  async_usb->QueueTx(fidl::VectorView<uint8_t>::FromExternal(test_data, sizeof(test_data)))
      .ThenExactlyOnce([&](auto& response) { completed = true; });

  driver_test_.runtime().RunUntil([&]() {
    size_t count = 0;
    driver_test_.RunInDriverContext(
        [&](UsbAdbDevice& dev) { count = UsbAdbTestHelper::TxPendingReqsCount(dev); });
    return count == 1;
  });

  SafeStopDriver();
}

TEST(UsbAdbStartTest, StartFailsGracefullyWhenConfigureFails) {
  fdf_testing::BackgroundDriverTest<UsbAdbTestConfig> driver_test;
  driver_test.RunInEnvironmentTypeContext(
      [](UsbAdbEnvironment& env) { env.set_configure_status(ZX_ERR_BAD_STATE); });

  // StartDriver must fail with ZX_ERR_BAD_STATE rather than crashing/panicking.
  zx::result<> start_result = driver_test.StartDriver();
  EXPECT_TRUE(start_result.is_error());
  EXPECT_EQ(start_result.status_value(), ZX_ERR_BAD_STATE);
  driver_test.runtime().RunUntilIdle();
}

TEST_F(UsbAdbTest, ReconnectHandlesConfigureFailureGracefully) {
  auto usb_impl = NormalStartAdb();
  EventHandler handler;
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl, handler);
  // Set the fake device to reject subsequent Configure calls (simulating peripheral
  // stopping/tearing down).
  driver_test_.RunInEnvironmentTypeContext(
      [](UsbAdbEnvironment& env) { env.fake_dev_->set_configure_status(ZX_ERR_BAD_STATE); });

  // Trigger an ADB-initiated USB reset by closing the UsbAdbImpl channel while online.
  usb_impl = {};
  WaitForState(State::kAwaitingUsbConnection);

  // The driver should have attempted to restart USB, encountered ZX_ERR_BAD_STATE,
  // transitioned cleanly to kAwaitingUsbConnection, and logged a warning without panicking.
  // Verify that subsequent StopAdb() completes cleanly (returning zx::ok()) rather than hanging.
  auto stop_res = client_->StopAdb();
  ASSERT_TRUE(stop_res.ok());
  EXPECT_TRUE(stop_res->is_ok());

  ASSERT_NO_FATAL_FAILURE(SafeStopDriver());
}

// Verifies that when endpoint configuration fails during SetConfigured(true) (e.g. Bulk OUT
// fails due to resource exhaustion in the controller), EnableEndpoints() propagates the error
// cleanly via the FIDL completer instead of calling ZX_PANIC. Verifies that the driver remains
// in State::kAwaitingUsbConnection and can successfully recover when endpoint configuration
// succeeds on a subsequent retry.
TEST_F(UsbAdbTest, ConfigureEndpointFailureDoesNotPanic) {
  // Connect ADB client while USB is not yet configured.
  auto [client_end, server_end] = fidl::Endpoints<fadb::UsbAdbImpl>::Create();
  ASSERT_TRUE(client_->StartAdb(std::move(server_end)).ok());

  WaitConfigured();
  ASSERT_TRUE(iface_client_.is_valid());

  // Set the fake device to fail endpoint configuration.
  driver_test_.RunInEnvironmentTypeContext(
      [](UsbAdbEnvironment& env) { env.fake_dev_->set_fail_configure_endpoint(true); });

  // SetConfigured(true) should fail gracefully with ZX_ERR_NO_RESOURCES rather than panicking.
  auto result = iface_client_->SetConfigured({{
      .configured = true,
      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kFull,
  }});
  EXPECT_TRUE(result.is_error());
  EXPECT_EQ(result.error_value().domain_error(), ZX_ERR_NO_RESOURCES);

  // The driver should remain in kAwaitingUsbConnection rather than transitioning to kOnline.
  driver_test_.RunInDriverContext([&](UsbAdbDevice& dev) {
    EXPECT_EQ(UsbAdbTestHelper::state(dev), State::kAwaitingUsbConnection);
  });

  // Clear the endpoint configuration failure on the fake device and retry.
  driver_test_.RunInEnvironmentTypeContext(
      [](UsbAdbEnvironment& env) { env.fake_dev_->set_fail_configure_endpoint(false); });

  auto retry_result = iface_client_->SetConfigured({{
      .configured = true,
      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kFull,
  }});
  EXPECT_TRUE(retry_result.is_ok());
  WaitForState(State::kOnline);

  ASSERT_NO_FATAL_FAILURE(SafeStopDriver());
}

// Verifies that when Bulk IN endpoint configuration fails after Bulk OUT has already been
// successfully configured, EnableEndpoints() rolls back the partial configuration by calling
// DisableEndpoint() on the Bulk OUT endpoint. Verifies that the driver does not leave orphan
// active endpoints in the controller, remains in State::kAwaitingUsbConnection, and can
// recover to State::kOnline when the failure condition clears.
TEST_F(UsbAdbTest, ConfigureEndpointFailureBulkInRollsBackBulkOut) {
  // Connect ADB client while USB is not yet configured.
  auto [client_end, server_end] = fidl::Endpoints<fadb::UsbAdbImpl>::Create();
  ASSERT_TRUE(client_->StartAdb(std::move(server_end)).ok());

  WaitConfigured();
  ASSERT_TRUE(iface_client_.is_valid());

  // Fail specifically on Bulk IN endpoint configuration (after Bulk OUT has succeeded).
  driver_test_.RunInEnvironmentTypeContext(
      [](UsbAdbEnvironment& env) { env.fake_dev_->set_fail_configure_endpoint_addr(kBulkInEp); });

  // SetConfigured(true) should fail with ZX_ERR_NO_RESOURCES.
  auto result = iface_client_->SetConfigured({{
      .configured = true,
      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kFull,
  }});
  EXPECT_TRUE(result.is_error());
  EXPECT_EQ(result.error_value().domain_error(), ZX_ERR_NO_RESOURCES);

  // Bulk OUT must have been rolled back via DisableEndpoint.
  driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
    auto disabled = env.fake_dev_->disabled_endpoints();
    ASSERT_EQ(disabled.size(), 1u);
    EXPECT_EQ(disabled[0], kBulkOutEp);
    env.fake_dev_->clear_disabled_endpoints();
  });

  // The driver should remain in kAwaitingUsbConnection rather than transitioning to kOnline.
  driver_test_.RunInDriverContext([&](UsbAdbDevice& dev) {
    EXPECT_EQ(UsbAdbTestHelper::state(dev), State::kAwaitingUsbConnection);
  });

  // Clear the endpoint configuration failure on the fake device and retry.
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->set_fail_configure_endpoint_addr(std::nullopt);
  });

  auto retry_result = iface_client_->SetConfigured({{
      .configured = true,
      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kFull,
  }});
  EXPECT_TRUE(retry_result.is_ok());
  WaitForState(State::kOnline);

  ASSERT_NO_FATAL_FAILURE(SafeStopDriver());
}

// Verifies that during a USB disconnect/reconnect cycle, if endpoint configuration fails when the
// host reconfigures USB, the driver handles the error gracefully without panicking. Verifies
// that SetConfigured(true) returns an error, the driver maintains State::kAwaitingUsbConnection,
// and full connectivity to State::kOnline is restored once endpoint configuration succeeds.
TEST_F(UsbAdbTest, ReconnectHandlesConfigureEndpointFailureGracefully) {
  auto usb_impl = NormalStartAdb();
  EventHandler handler;
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl, handler);

  // Trigger disconnect by deconfiguring USB.
  auto deconfig_result = iface_client_->SetConfigured({{
      .configured = false,
      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kHigh,
  }});
  ASSERT_TRUE(deconfig_result.is_ok());

  // Wait for requests to drain and driver to reach kAwaitingUsbConnection.
  WaitForState(State::kAwaitingUsbConnection);
  ASSERT_TRUE(iface_client_.is_valid());

  // Set the fake USB device to fail endpoint configuration on reconnect.
  driver_test_.RunInEnvironmentTypeContext(
      [](UsbAdbEnvironment& env) { env.fake_dev_->set_fail_configure_endpoint(true); });

  // Host attempts to re-enable USB. Configuration should fail gracefully.
  auto reconfig_result = iface_client_->SetConfigured({{
      .configured = true,
      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kHigh,
  }});
  EXPECT_TRUE(reconfig_result.is_error());
  EXPECT_EQ(reconfig_result.error_value().domain_error(), ZX_ERR_NO_RESOURCES);

  // Driver must remain in kAwaitingUsbConnection.
  driver_test_.RunInDriverContext([&](UsbAdbDevice& dev) {
    EXPECT_EQ(UsbAdbTestHelper::state(dev), State::kAwaitingUsbConnection);
  });

  // Clear the endpoint configuration failure and re-attempt connection.
  driver_test_.RunInEnvironmentTypeContext(
      [](UsbAdbEnvironment& env) { env.fake_dev_->set_fail_configure_endpoint(false); });

  auto retry_result = iface_client_->SetConfigured({{
      .configured = true,
      .speed = fuchsia_hardware_usb_descriptor::UsbSpeed::kHigh,
  }});
  EXPECT_TRUE(retry_result.is_ok());
  WaitForState(State::kOnline);

  ASSERT_NO_FATAL_FAILURE(SafeStopDriver());
}

// Verifies that if SetConfigured(true) arrives while the driver is stopping for reconnect
// (e.g. rapid host reconnect while CancelAll is still draining), the driver records
// pending_reconfigure_ and immediately re-enables endpoints and transitions to State::kOnline
// once the stopping cycle completes.
TEST_F(UsbAdbTest, SetConfiguredTrueDuringStoppingReconfiguresCleanly) {
  auto usb_impl = NormalStartAdb();
  EventHandler handler;
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl, handler);

  // 1. Hold CancelAll so that the driver remains in State::kStoppingForReconnect.
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).set_hold_cancel(true);
    env.fake_dev_->fake_endpoint(kBulkInEp).set_hold_cancel(true);
  });

  EnsureIfaceBound();
  ASSERT_TRUE(iface_client_.is_valid());
  auto iface_client_end = iface_client_.TakeClientEnd();
  fidl::WireClient async_iface(std::move(iface_client_end),
                               fdf::Dispatcher::GetCurrent()->async_dispatcher());

  // 2. Disconnect USB: SetConfigured(false) asynchronously while CancelAll is held.
  std::atomic<bool> unconfigured_done = false;
  async_iface->SetConfigured(false, fuchsia_hardware_usb_descriptor::wire::UsbSpeed::kFull)
      .ThenExactlyOnce([&](auto& result) {
        EXPECT_TRUE(result.ok());
        EXPECT_TRUE(result->is_ok());
        unconfigured_done = true;
      });
  WaitForState(State::kStoppingForReconnect);
  EXPECT_FALSE(unconfigured_done.load());

  // 3. While still in kStoppingForReconnect, host reconnects: SetConfigured(true).
  std::atomic<bool> reconfigured_done = false;
  async_iface->SetConfigured(true, fuchsia_hardware_usb_descriptor::wire::UsbSpeed::kFull)
      .ThenExactlyOnce([&](auto& result) {
        EXPECT_TRUE(result.ok());
        EXPECT_TRUE(result->is_ok());
        reconfigured_done = true;
      });
  driver_test_.runtime().RunUntil([&]() { return reconfigured_done.load(); });

  // 4. Release CancelAll so the stopping process completes.
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).ReleaseCancelAll();
    env.fake_dev_->fake_endpoint(kBulkInEp).ReleaseCancelAll();
    env.fake_dev_->fake_endpoint(kBulkOutEp).set_hold_cancel(false);
    env.fake_dev_->fake_endpoint(kBulkInEp).set_hold_cancel(false);
    env.CancelAllUsbRequests();
  });

  // 5. The driver should complete its stop and immediately transition to State::kOnline.
  WaitForState(State::kOnline);
  driver_test_.runtime().RunUntil([&]() { return unconfigured_done.load(); });
  async_iface = {};

  ASSERT_NO_FATAL_FAILURE(SafeStopDriver());
}

// Verifies that SetConfigured(false) from usb-peripheral (e.g. on physical unplug or host reset)
// holds its FIDL completer until CancelAll() finishes, does NOT call function_->Deconfigure()
// or StartUsb(), and keeps both usb_function_binding_ and adb_binding_ alive across reconnect.
TEST_F(UsbAdbTest, SetConfiguredFalseDoesNotDeconfigureFunctionAndKeepsAdbBound) {
  auto usb_impl = NormalStartAdb();
  EventHandler handler;
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl, handler);

  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).set_hold_cancel(true);
    env.fake_dev_->fake_endpoint(kBulkInEp).set_hold_cancel(true);
  });

  auto iface_client_end = iface_client_.TakeClientEnd();
  fidl::WireClient async_iface(std::move(iface_client_end),
                               fdf::Dispatcher::GetCurrent()->async_dispatcher());

  std::atomic<bool> set_configured_done = false;
  async_iface->SetConfigured(false, fuchsia_hardware_usb_descriptor::wire::UsbSpeed::kHigh)
      .ThenExactlyOnce([&](auto& result) {
        ASSERT_TRUE(result.ok());
        EXPECT_TRUE(result->is_ok());
        set_configured_done = true;
      });

  WaitForState(State::kStoppingForReconnect);
  EXPECT_FALSE(set_configured_done.load());

  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).ReleaseCancelAll();
    env.fake_dev_->fake_endpoint(kBulkInEp).ReleaseCancelAll();
    env.fake_dev_->fake_endpoint(kBulkOutEp).set_hold_cancel(false);
    env.fake_dev_->fake_endpoint(kBulkInEp).set_hold_cancel(false);
    env.CancelAllUsbRequests();
  });

  driver_test_.runtime().RunUntil([&]() { return set_configured_done.load(); });
  WaitForState(State::kAwaitingUsbConnection);

  // Verify Deconfigure() was NEVER called and both bindings remain alive.
  driver_test_.RunInEnvironmentTypeContext(
      [](UsbAdbEnvironment& env) { EXPECT_EQ(env.fake_dev_->deconfigure_count(), 0u); });
  driver_test_.RunInDriverContext([](UsbAdbDevice& dev) {
    EXPECT_TRUE(UsbAdbTestHelper::has_usb_function_binding(dev));
    EXPECT_TRUE(UsbAdbTestHelper::has_adb_binding(dev));
  });

  // The existing usb_impl client must receive OnStatusChanged(0) without its channel closing.
  handler.expected_statuses_.emplace(fadb::StatusFlags(0));
  ExpectHandleOneEventSafe(usb_impl, handler);

  // Re-configuring on the same async_iface brings the same usb_impl client back online.
  std::atomic<bool> reconfigured_done = false;
  async_iface->SetConfigured(true, fuchsia_hardware_usb_descriptor::wire::UsbSpeed::kHigh)
      .ThenExactlyOnce([&](auto& result) {
        ASSERT_TRUE(result.ok());
        EXPECT_TRUE(result->is_ok());
        reconfigured_done = true;
      });
  driver_test_.runtime().RunUntil([&]() { return reconfigured_done.load(); });
  WaitForState(State::kOnline);

  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl, handler);

  async_iface = {};
  ASSERT_NO_FATAL_FAILURE(SafeStopDriver());
}

// Verifies the physical unplug race: when DWC3 cancels an active RX request with
// ZX_ERR_IO_NOT_PRESENT slightly before usb-peripheral delivers SetConfigured(false),
// ReceiveQueued() does not re-queue the failed request onto the disconnecting endpoint,
// and if adbd calls StopAdb() before SetConfigured(false) arrives, SetConfigured(false)
// coalesces cleanly without logging a warning or calling function_->Deconfigure().
TEST_F(UsbAdbTest, SetConfiguredFalseDuringStopAdbCoalescesWithoutDeconfigure) {
  auto usb_impl_sync = NormalStartAdb();
  EventHandler handler;
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl_sync, handler);

  auto usb_impl = fidl::WireClient<fadb::UsbAdbImpl>(
      usb_impl_sync.TakeClientEnd(), fdf::Dispatcher::GetCurrent()->async_dispatcher());

  // 1. Queue a Receive() call and complete one bulk OUT request with ZX_ERR_IO_NOT_PRESENT.
  std::atomic<bool> rx_failed = false;
  usb_impl->Receive().ThenExactlyOnce([&](auto& response) {
    ASSERT_TRUE(response.ok());
    ASSERT_TRUE(response->is_error());
    EXPECT_EQ(response->error_value(), ZX_ERR_CANCELED);
    rx_failed = true;
  });

  driver_test_.runtime().RunUntil([&]() {
    size_t count = 0;
    driver_test_.RunInDriverContext(
        [&](UsbAdbDevice& dev) { count = UsbAdbTestHelper::RxRequestsCount(dev); });
    return count == 1;
  });

  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).RequestComplete(ZX_ERR_IO_NOT_PRESENT, 0);
  });

  driver_test_.runtime().RunUntil([&]() { return rx_failed.load(); });

  // ReceiveQueued() must have returned the request to the pool rather than re-queuing it onto the
  // disconnecting endpoint.
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    EXPECT_EQ(env.fake_dev_->fake_endpoint(kBulkOutEp).pending_request_count(), kBulkRxCount - 1);
  });

  // 2. Hold CancelAll and simulate adbd calling StopAdb() 5ms before SetConfigured(false) arrives.
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).set_hold_cancel(true);
    env.fake_dev_->fake_endpoint(kBulkInEp).set_hold_cancel(true);
  });

  auto dev_client_end = client_.TakeClientEnd();
  fidl::WireClient async_client(std::move(dev_client_end),
                                fdf::Dispatcher::GetCurrent()->async_dispatcher());
  std::atomic<bool> stop_adb_done = false;
  async_client->StopAdb().ThenExactlyOnce([&](auto& result) {
    ASSERT_TRUE(result.ok());
    EXPECT_TRUE(result->is_ok());
    stop_adb_done = true;
  });

  WaitForState(State::kStoppingForReconnect);
  EXPECT_FALSE(stop_adb_done.load());

  // 3. Now SetConfigured(false) arrives from usb-peripheral while kStoppingForReconnect is active.
  auto iface_client_end = iface_client_.TakeClientEnd();
  fidl::WireClient async_iface(std::move(iface_client_end),
                               fdf::Dispatcher::GetCurrent()->async_dispatcher());
  std::atomic<bool> set_configured_done = false;
  async_iface->SetConfigured(false, fuchsia_hardware_usb_descriptor::wire::UsbSpeed::kHigh)
      .ThenExactlyOnce([&](auto& result) {
        ASSERT_TRUE(result.ok());
        EXPECT_TRUE(result->is_ok());
        set_configured_done = true;
      });

  driver_test_.runtime().RunUntilIdle();
  EXPECT_FALSE(stop_adb_done.load());
  EXPECT_FALSE(set_configured_done.load());

  // 4. Release CancelAll and verify both completers finish without calling Deconfigure().
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).ReleaseCancelAll();
    env.fake_dev_->fake_endpoint(kBulkInEp).ReleaseCancelAll();
    env.fake_dev_->fake_endpoint(kBulkOutEp).set_hold_cancel(false);
    env.fake_dev_->fake_endpoint(kBulkInEp).set_hold_cancel(false);
    env.CancelAllUsbRequests();
  });

  driver_test_.runtime().RunUntil(
      [&]() { return stop_adb_done.load() && set_configured_done.load(); });
  WaitForState(State::kAwaitingUsbConnection);

  driver_test_.RunInEnvironmentTypeContext(
      [](UsbAdbEnvironment& env) { EXPECT_EQ(env.fake_dev_->deconfigure_count(), 0u); });

  usb_impl = {};
  async_client = {};
  async_iface = {};
  ASSERT_NO_FATAL_FAILURE(SafeStopDriver());
}

// Verifies that StartAdb() can be called after USB is already online (State::kOnline) AND
// while a previous session's stop/reconnect is still draining (State::kStoppingForReconnect).
TEST_F(UsbAdbTest, StartAdbAfterUsbAlreadyOnlineAndDuringStop) {
  // Part A: Bring USB online BEFORE calling StartAdb(), and verify that a Bulk OUT packet
  // arriving before StartAdb() is buffered in pending_replies_ and delivered once StartAdb()
  // and Receive() are called.
  WaitConfigured();
  EnableUsb();
  WaitForState(State::kOnline);
  ExpectReceiveData(32);

  auto [client_end1, server_end1] = fidl::Endpoints<fadb::UsbAdbImpl>::Create();
  auto start_res1 = client_->StartAdb(std::move(server_end1));
  ASSERT_TRUE(start_res1.ok());
  ASSERT_TRUE(start_res1->is_ok());

  fidl::WireSyncClient<fadb::UsbAdbImpl> usb_impl1(std::move(client_end1));
  EventHandler handler1;
  handler1.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl1, handler1);

  auto rx1_res = usb_impl1->Receive();
  ASSERT_TRUE(rx1_res.ok());
  ASSERT_TRUE(rx1_res->is_ok());
  EXPECT_EQ(rx1_res->value()->data.size(), 32u);

  // Part B: Hold CancelAll, call StopAdb(), and immediately call StartAdb() with a new channel
  // while the driver is still in State::kStoppingForReconnect.
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).set_hold_cancel(true);
    env.fake_dev_->fake_endpoint(kBulkInEp).set_hold_cancel(true);
  });

  auto dev_client_end = client_.TakeClientEnd();
  fidl::WireClient async_client(std::move(dev_client_end),
                                fdf::Dispatcher::GetCurrent()->async_dispatcher());
  std::atomic<bool> stop_done = false;
  async_client->StopAdb().ThenExactlyOnce([&](auto& result) {
    ASSERT_TRUE(result.ok());
    EXPECT_TRUE(result->is_ok());
    stop_done = true;
  });

  WaitForState(State::kStoppingForReconnect);
  handler1.expected_statuses_.emplace(fadb::StatusFlags(0));
  ExpectHandleOneEventSafe(usb_impl1, handler1);
  usb_impl1 = {};

  // Connect new ADB session while State::kStoppingForReconnect is still draining!
  auto [client_end2, server_end2] = fidl::Endpoints<fadb::UsbAdbImpl>::Create();
  std::atomic<bool> start2_done = false;
  async_client->StartAdb(std::move(server_end2)).ThenExactlyOnce([&](auto& result) {
    ASSERT_TRUE(result.ok());
    EXPECT_TRUE(result->is_ok());
    start2_done = true;
  });
  driver_test_.runtime().RunUntil([&]() { return start2_done.load(); });

  // Queue an offline Receive() on the new session while still stopping.
  fidl::WireClient<fadb::UsbAdbImpl> usb_impl2(std::move(client_end2),
                                               fdf::Dispatcher::GetCurrent()->async_dispatcher());
  std::atomic<bool> rx2_done = false;
  usb_impl2->Receive().ThenExactlyOnce([&](auto& response) {
    ASSERT_TRUE(response.ok());
    ASSERT_TRUE(response->is_ok());
    EXPECT_EQ(response->value()->data.size(), 64u);
    rx2_done = true;
  });
  driver_test_.runtime().RunUntilIdle();

  // Release CancelAll so the stop completes and restarts USB (Deconfigure -> Configure).
  iface_client_ = {};
  driver_test_.RunInEnvironmentTypeContext([](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkOutEp).ReleaseCancelAll();
    env.fake_dev_->fake_endpoint(kBulkInEp).ReleaseCancelAll();
    env.fake_dev_->fake_endpoint(kBulkOutEp).set_hold_cancel(false);
    env.fake_dev_->fake_endpoint(kBulkInEp).set_hold_cancel(false);
    env.CancelAllUsbRequests();
  });

  driver_test_.runtime().RunUntil([&]() { return stop_done.load(); });
  WaitForState(State::kAwaitingUsbConnection);

  // Re-enable USB and verify the new ADB session comes online and services its queued Receive().
  WaitConfigured();
  EnableUsb();
  ExpectReceiveData(64);
  driver_test_.runtime().RunUntil([&]() { return rx2_done.load(); });

  usb_impl2 = {};
  async_client = {};
  ASSERT_NO_FATAL_FAILURE(SafeStopDriver());
}

TEST_F(UsbAdbTest, QueueTxCompletesOnHardwareTxComplete) {
  auto usb_impl = NormalStartAdb();
  EventHandler handler;
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl, handler);

  auto async_usb_impl = fidl::WireClient<fadb::UsbAdbImpl>(
      usb_impl.TakeClientEnd(), fdf::Dispatcher::GetCurrent()->async_dispatcher());

  // Queue a payload without pre-queuing a completion on the fake Bulk IN endpoint.
  // SendQueued() submits the request to kBulkInEp, while the QueueTx FIDL completer remains
  // pending until TxComplete() receives the hardware completion.
  std::vector<uint8_t> payload = {'r', 'e', 's', 't', 'a', 'r', 't', 'i', 'n', 'g'};
  std::atomic<bool> tx_completed = false;
  async_usb_impl->QueueTx(fidl::VectorView<uint8_t>::FromExternal(payload.data(), payload.size()))
      .ThenExactlyOnce([&tx_completed](auto& result) {
        ASSERT_TRUE(result.ok());
        EXPECT_TRUE(result->is_ok());
        tx_completed = true;
      });

  driver_test_.runtime().RunUntil([&] {
    size_t pending = 0;
    driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
      pending = env.fake_dev_->fake_endpoint(kBulkInEp).pending_request_count();
    });
    return pending == 1u;
  });

  driver_test_.runtime().RunUntilIdle();
  EXPECT_FALSE(tx_completed.load());

  // Complete the in-flight Bulk IN request on the endpoint and verify QueueTx completes.
  driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkInEp).RequestComplete(ZX_OK, payload.size());
  });

  driver_test_.runtime().RunUntil([&] { return tx_completed.load(); });

  async_usb_impl = {};
  SafeStopDriver();
}

TEST_F(UsbAdbTest, QueueTxPipelinedFifoAndErrorCompletion) {
  auto usb_impl = NormalStartAdb();
  EventHandler handler;
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl, handler);

  auto async_usb_impl = fidl::WireClient<fadb::UsbAdbImpl>(
      usb_impl.TakeClientEnd(), fdf::Dispatcher::GetCurrent()->async_dispatcher());

  std::vector<uint8_t> payload1 = {1, 2, 3, 4};
  std::vector<uint8_t> payload2 = {5, 6, 7, 8};
  std::atomic<bool> tx1_ok = false;
  std::atomic<bool> tx2_failed = false;

  async_usb_impl->QueueTx(fidl::VectorView<uint8_t>::FromExternal(payload1.data(), payload1.size()))
      .ThenExactlyOnce([&tx1_ok](auto& result) {
        ASSERT_TRUE(result.ok());
        EXPECT_TRUE(result->is_ok());
        tx1_ok = true;
      });
  async_usb_impl->QueueTx(fidl::VectorView<uint8_t>::FromExternal(payload2.data(), payload2.size()))
      .ThenExactlyOnce([&tx2_failed](auto& result) {
        ASSERT_TRUE(result.ok());
        EXPECT_TRUE(result->is_error());
        tx2_failed = true;
      });

  driver_test_.runtime().RunUntil([&] {
    size_t pending = 0;
    driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
      pending = env.fake_dev_->fake_endpoint(kBulkInEp).pending_request_count();
    });
    return pending == 2u;
  });

  driver_test_.runtime().RunUntilIdle();
  EXPECT_FALSE(tx1_ok.load());
  EXPECT_FALSE(tx2_failed.load());

  // Complete first Bulk IN request -> only tx1 resolves; tx2 remains pending.
  driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkInEp).RequestComplete(ZX_OK, payload1.size());
  });
  driver_test_.runtime().RunUntil([&] { return tx1_ok.load(); });
  driver_test_.runtime().RunUntilIdle();
  EXPECT_FALSE(tx2_failed.load());

  // Complete second Bulk IN request with hardware error -> tx2 resolves with error.
  driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkInEp).RequestComplete(ZX_ERR_IO_NOT_PRESENT, 0);
  });
  driver_test_.runtime().RunUntil([&] { return tx2_failed.load(); });

  async_usb_impl = {};
  SafeStopDriver();
}

TEST_F(UsbAdbTest, QueueTxOutOfOrderCompletionMatchesByVmoId) {
  auto usb_impl = NormalStartAdb();
  EventHandler handler;
  handler.expected_statuses_.emplace(fadb::StatusFlags::kOnline);
  ExpectHandleOneEventSafe(usb_impl, handler);

  auto async_usb_impl = fidl::WireClient<fadb::UsbAdbImpl>(
      usb_impl.TakeClientEnd(), fdf::Dispatcher::GetCurrent()->async_dispatcher());

  std::vector<uint8_t> payload1 = {1, 2, 3, 4};
  std::vector<uint8_t> payload2 = {5, 6, 7, 8};
  std::atomic<bool> tx1_ok = false;
  std::atomic<bool> tx2_ok = false;

  async_usb_impl->QueueTx(fidl::VectorView<uint8_t>::FromExternal(payload1.data(), payload1.size()))
      .ThenExactlyOnce([&tx1_ok](auto& result) {
        ASSERT_TRUE(result.ok());
        EXPECT_TRUE(result->is_ok());
        tx1_ok = true;
      });
  async_usb_impl->QueueTx(fidl::VectorView<uint8_t>::FromExternal(payload2.data(), payload2.size()))
      .ThenExactlyOnce([&tx2_ok](auto& result) {
        ASSERT_TRUE(result.ok());
        EXPECT_TRUE(result->is_ok());
        tx2_ok = true;
      });

  driver_test_.runtime().RunUntil([&] {
    size_t pending = 0;
    driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
      pending = env.fake_dev_->fake_endpoint(kBulkInEp).pending_request_count();
    });
    return pending == 2u;
  });

  // Complete the second request (index 1) out of order before the first request (index 0).
  // Matching by VMO ID must resolve tx2 while keeping tx1 pending until its own request completes.
  driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkInEp).RequestCompleteAtIndex(1, ZX_OK, payload2.size());
  });
  driver_test_.runtime().RunUntil([&] { return tx2_ok.load(); });
  driver_test_.runtime().RunUntilIdle();
  EXPECT_FALSE(tx1_ok.load());

  // Complete the remaining first request -> tx1 resolves.
  driver_test_.RunInEnvironmentTypeContext([&](UsbAdbEnvironment& env) {
    env.fake_dev_->fake_endpoint(kBulkInEp).RequestCompleteAtIndex(0, ZX_OK, payload1.size());
  });
  driver_test_.runtime().RunUntil([&] { return tx1_ok.load(); });

  async_usb_impl = {};
  SafeStopDriver();
}

}  // namespace usb_adb_function
