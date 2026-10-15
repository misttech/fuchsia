// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_manager/shutdown/shutdown_manager.h"

#include <fidl/fuchsia.io/cpp/wire.h>
#include <fidl/fuchsia.kernel/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/task.h>
#include <lib/async_patterns/testing/cpp/dispatcher_bound.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/fake-resource/cpp/fake-resource.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/namespace.h>
#include <lib/sync/cpp/completion.h>
#include <lib/zx/event.h>
#include <zircon/syscalls/system.h>

#include <memory>

#include <gtest/gtest.h>

#include "src/devices/bin/driver_manager/shutdown/node_remover.h"

namespace driver_manager::test {

namespace {

class MockNodeRemover final : public driver_manager::NodeRemover {
 public:
  MockNodeRemover() = default;
  ~MockNodeRemover() = default;

  void set_received_shutdown_all_request_completion(
      std::shared_ptr<libsync::Completion> completion) {
    received_shutdown_all_request_ = std::move(completion);
  }

  void RunShutdownAllCallback() {
    if (shutdown_all_callback_) {
      shutdown_all_callback_();
    }
  }

  void RunTimeoutCallback() {
    if (timeout_callback_) {
      timeout_callback_();
    }
  }

  // `driver_manager::NodeRemover` implementation.
  void ShutdownAllDrivers(fit::callback<void()> callback) override {
    shutdown_all_callback_ = std::move(callback);
    if (received_shutdown_all_request_) {
      received_shutdown_all_request_->Signal();
    }
  }

  void ShutdownPkgDrivers(fit::callback<void()> callback) override {
    shutdown_pkg_callback_ = std::move(callback);
  }

  void SetOnRemovalTimeoutCallback(fit::callback<void()> callback) override {
    timeout_callback_ = std::move(callback);
  }

  void LeaseAllDriversForShutdown(fit::callback<void()> callback) override { callback(); }

 private:
  fit::callback<void()> shutdown_all_callback_;
  fit::callback<void()> shutdown_pkg_callback_;
  fit::callback<void()> timeout_callback_;

  std::shared_ptr<libsync::Completion> received_shutdown_all_request_;
};

class TestShutdownManager : public driver_manager::ShutdownManager {
 public:
  TestShutdownManager(std::unique_ptr<MockNodeRemover> mock_remover, async_dispatcher_t* dispatcher,
                      fuchsia_system_state::SystemPowerState state)
      : driver_manager::ShutdownManager(mock_remover.get(), dispatcher),
        node_remover_(std::move(mock_remover)),
        state_(state),
        received_shutdown_all_request_(std::make_shared<libsync::Completion>()),
        system_powerctl_called_(std::make_shared<libsync::Completion>()) {
    node_remover_->set_received_shutdown_all_request_completion(received_shutdown_all_request_);
  }

  // Trigger the event that the node remover has removed all nodes.
  void TriggerAllNodesRemoved() { node_remover_->RunShutdownAllCallback(); }

  // Triggers the event that the node remover has timed out waiting for nodes to be removed.
  void TriggerNodeRemovalTimedOut() { node_remover_->RunTimeoutCallback(); }

  std::shared_ptr<libsync::Completion> received_shutdown_all_request() {
    return received_shutdown_all_request_;
  }
  std::shared_ptr<libsync::Completion> system_powerctl_completion() {
    return system_powerctl_called_;
  }

  bool system_powerctl_called() const { return system_powerctl_called_->signaled(); }
  std::optional<uint32_t> system_powerctl_cmd() const { return system_powerctl_cmd_; }

 private:
  fuchsia_system_state::SystemPowerState GetSystemPowerState() override { return state_; }

  zx_status_t SystemPowerctl(uint32_t cmd) override {
    system_powerctl_cmd_ = cmd;
    system_powerctl_called_->Signal();
    return ZX_OK;
  }

  void Exit(int status) override {
    // Do nothing.
  }
  // Node remover used by the shutdown manager.
  std::unique_ptr<MockNodeRemover> node_remover_;

  // The system-power state.
  fuchsia_system_state::SystemPowerState state_;

  // Command argument passed into the `system_powerctl()` call made by the shutdown manager.
  std::optional<uint32_t> system_powerctl_cmd_;

  // Completion signal for when the node remover has received a shutdown-all request.
  std::shared_ptr<libsync::Completion> received_shutdown_all_request_;

  // Completion signal for when the shutdown manager has called `system_powerctl()`.
  std::shared_ptr<libsync::Completion> system_powerctl_called_;
};

class FakePowerResource : public fidl::WireServer<fuchsia_kernel::PowerResource> {
 public:
  explicit FakePowerResource(zx::resource resource) : resource_(std::move(resource)) {}
  ~FakePowerResource() override {}
  void Get(GetCompleter::Sync& completer) override {
    ZX_ASSERT(resource_.is_valid());
    zx::resource clone;
    ZX_ASSERT(resource_.duplicate(ZX_RIGHT_SAME_RIGHTS, &clone) == ZX_OK);
    completer.Reply(std::move(clone));
  }

 private:
  zx::resource resource_;
};

zx::resource CreateDummyResource() {
  zx_handle_t raw_handle;
  ZX_ASSERT(fake_resource_create(ZX_RSRC_KIND_SYSTEM, &raw_handle) == ZX_OK);
  return zx::resource(raw_handle);
}

class FakeLogFlusher : public fidl::WireServer<fuchsia_diagnostics::LogFlusher> {
 public:
  ~FakeLogFlusher() override {}
  void WaitUntilFlushed(WaitUntilFlushedCompleter::Sync& completer) override {
    flushed_ = true;
    completer.Reply();
  }
  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_diagnostics::LogFlusher> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}
  bool flushed() const { return flushed_; }

 private:
  bool flushed_ = false;
};

class SvcRedirection {
 public:
  SvcRedirection() {
    ZX_ASSERT(fdio_ns_get_installed(&ns_) == ZX_OK);

    // Save original /svc
    zx::channel client, server;
    ZX_ASSERT(zx::channel::create(0, &client, &server) == ZX_OK);
    ZX_ASSERT(fdio_open3("/svc", static_cast<uint64_t>(fuchsia_io::wire::kPermReadable),
                         server.release()) == ZX_OK);
    original_svc_ = fidl::ClientEnd<fuchsia_io::Directory>(std::move(client));
  }

  ~SvcRedirection() {
    if (ns_ && bound_) {
      fdio_ns_unbind(ns_, "/svc");
      if (original_svc_.is_valid()) {
        fdio_ns_bind(ns_, "/svc", original_svc_.TakeChannel().release());
      }
    }
  }

  void Bind(fidl::ClientEnd<fuchsia_io::Directory> client_end) {
    (void)fdio_ns_unbind(ns_, "/svc");
    zx_status_t bind_status = fdio_ns_bind(ns_, "/svc", client_end.TakeChannel().release());
    ZX_ASSERT(bind_status == ZX_OK);
    bound_ = true;
  }

 private:
  fdio_ns_t* ns_ = nullptr;
  fidl::ClientEnd<fuchsia_io::Directory> original_svc_;
  bool bound_ = false;
};

// The incoming namespace provided to the shutdown manager.
class Incoming {
 public:
  explicit Incoming(zx::resource power) : power_(std::move(power)) {}
  ~Incoming() {}

  void Serve(async_dispatcher_t* dispatcher) {
    outgoing_.emplace(dispatcher);

    zx::result<> status = outgoing_->AddUnmanagedProtocol<fuchsia_kernel::PowerResource>(
        [this, dispatcher](fidl::ServerEnd<fuchsia_kernel::PowerResource> server_end) {
          fidl::BindServer(dispatcher, std::move(server_end), &power_);
        });
    ZX_ASSERT(status.is_ok());

    status = outgoing_->AddUnmanagedProtocol<fuchsia_diagnostics::LogFlusher>(
        [this, dispatcher](fidl::ServerEnd<fuchsia_diagnostics::LogFlusher> server_end) {
          fidl::BindServer(dispatcher, std::move(server_end), &log_flusher_);
        });
    ZX_ASSERT(status.is_ok());
  }

  fidl::ClientEnd<fuchsia_io::Directory> ServeDirectory(async_dispatcher_t* dispatcher) {
    Serve(dispatcher);
    fidl::Endpoints<fuchsia_io::Directory> endpoints =
        fidl::Endpoints<fuchsia_io::Directory>::Create();
    ZX_ASSERT(outgoing_->Serve(std::move(endpoints.server)).is_ok());
    root_client_ = std::move(endpoints.client);

    fidl::Endpoints<fuchsia_io::Directory> svc_endpoints =
        fidl::Endpoints<fuchsia_io::Directory>::Create();
    zx_status_t status = fdio_open3_at(
        root_client_.channel().get(), "svc",
        uint64_t{fuchsia_io::wire::kPermReadable | fuchsia_io::wire::Flags::kProtocolDirectory},
        svc_endpoints.server.TakeChannel().release());
    ZX_ASSERT(status == ZX_OK);
    return std::move(svc_endpoints.client);
  }

  bool LogsFlushed() const { return log_flusher_.flushed(); }

 private:
  FakePowerResource power_;
  FakeLogFlusher log_flusher_;
  std::optional<component::OutgoingDirectory> outgoing_;
  fidl::ClientEnd<fuchsia_io::Directory> root_client_;
};

class ShutdownManagerTest : public ::testing::Test {
 protected:
  ShutdownManagerTest() {
    ZX_ASSERT(incoming_loop_.StartThread("incoming-thread") == ZX_OK);
    ZX_ASSERT(shutdown_manager_loop_.StartThread("shutdown-manager-thread") == ZX_OK);

    incoming_.emplace(incoming_loop_.dispatcher(), std::in_place, CreateDummyResource());
  }

  void SetUp() override {
    fidl::ClientEnd<fuchsia_io::Directory> client_end =
        incoming_->SyncCall([dispatcher = incoming_loop_.dispatcher()](Incoming* incoming) {
          return incoming->ServeDirectory(dispatcher);
        });

    svc_redirection_.Bind(std::move(client_end));
  }

  void TearDown() override {
    EXPECT_TRUE(LogsFlushed());
    shutdown_manager_.reset();
    incoming_.reset();
    shutdown_manager_loop_.Shutdown();
    incoming_loop_.Shutdown();
  }

  bool LogsFlushed() { return incoming_->SyncCall(&Incoming::LogsFlushed); }

  fidl::WireSyncClient<fuchsia_process_lifecycle::Lifecycle> StartShutdownManager(
      fuchsia_system_state::SystemPowerState state) {
    std::unique_ptr<MockNodeRemover> mock_remover = std::make_unique<MockNodeRemover>();

    shutdown_manager_.emplace(
        shutdown_manager_loop_.dispatcher(), std::in_place,
        async_patterns::internal::Smuggle(std::move(mock_remover)),
        async_patterns::internal::Smuggle(shutdown_manager_loop_.dispatcher()), state);

    fidl::Endpoints<fuchsia_process_lifecycle::Lifecycle> endpoints =
        fidl::Endpoints<fuchsia_process_lifecycle::Lifecycle>::Create();

    shutdown_manager_->SyncCall(
        [dispatcher = shutdown_manager_loop_.dispatcher(),
         server_end = std::move(endpoints.server)](TestShutdownManager* manager) mutable {
          fidl::BindServer(dispatcher, std::move(server_end), manager);
        });

    return fidl::WireSyncClient<fuchsia_process_lifecycle::Lifecycle>(std::move(endpoints.client));
  }

  template <typename AssertCallable>
  void AssertShutdownManager(AssertCallable&& assert_callable) {
    shutdown_manager_->SyncCall(std::forward<AssertCallable>(assert_callable));
  }

  // Trigger the event that the node remover has removed all nodes.
  void TriggerAllNodesRemoved() {
    shutdown_manager_->SyncCall(&TestShutdownManager::TriggerAllNodesRemoved);
  }

  // Triggers the event that the node remover has timed out waiting for nodes to be removed.
  void TriggerNodeRemovalTimedOut() {
    shutdown_manager_->SyncCall(&TestShutdownManager::TriggerNodeRemovalTimedOut);
  }

  // Blocks until the node remover has received a request to shutdown all nodes.
  void WaitForShutdownAllRequest() {
    std::shared_ptr<libsync::Completion> completion = shutdown_manager_->SyncCall(
        [](TestShutdownManager* m) { return m->received_shutdown_all_request(); });
    completion->Wait();
  }

  // Blocks until the shutdown manager calls `system_powerctl()`.
  void WaitForSystemPowerctl() {
    std::shared_ptr<libsync::Completion> completion = shutdown_manager_->SyncCall(
        [](TestShutdownManager* m) { return m->system_powerctl_completion(); });
    completion->Wait();
  }

  async_dispatcher_t* shutdown_manager_dispatcher() { return shutdown_manager_loop_.dispatcher(); }

 private:
  async::Loop incoming_loop_{&kAsyncLoopConfigNoAttachToCurrentThread};
  std::optional<async_patterns::TestDispatcherBound<Incoming>> incoming_;

  async::Loop shutdown_manager_loop_{&kAsyncLoopConfigNoAttachToCurrentThread};
  std::optional<async_patterns::TestDispatcherBound<TestShutdownManager>> shutdown_manager_;

  SvcRedirection svc_redirection_;
};

// Verifies that if the node remover times out then the system still reboots.
TEST_F(ShutdownManagerTest, RebootOnTimeout) {
  fidl::WireSyncClient<fuchsia_process_lifecycle::Lifecycle> client =
      StartShutdownManager(fuchsia_system_state::SystemPowerState::kReboot);

  // Tell the shutdown manager to stop and shutdown all nodes.
  fidl::OneWayStatus stop_result = client->Stop();
  ASSERT_TRUE(stop_result.ok()) << stop_result.FormatDescription();

  // Wait for the node remover to receive the request to shutdown all nodes.
  WaitForShutdownAllRequest();

  // Pretend that the node remover timed out trying to remove all the nodes.
  TriggerNodeRemovalTimedOut();

  // Node remover timed out and so `system_powerctl()` should still be called.
  WaitForSystemPowerctl();
  AssertShutdownManager([](TestShutdownManager* manager) {
    EXPECT_TRUE(manager->system_powerctl_called());
    EXPECT_EQ(ZX_SYSTEM_POWERCTL_REBOOT, manager->system_powerctl_cmd());
  });
}

// Verifies the shutdown manager correctly removes all nodes when stopping and reboots the system
// when in the reboot power-system state.
TEST_F(ShutdownManagerTest, Reboot) {
  fidl::WireSyncClient<fuchsia_process_lifecycle::Lifecycle> client =
      StartShutdownManager(fuchsia_system_state::SystemPowerState::kReboot);

  // Tell the shutdown manager to stop and shutdown all nodes.
  fidl::OneWayStatus stop_result = client->Stop();
  ASSERT_TRUE(stop_result.ok()) << stop_result.FormatDescription();

  // Wait for the node remover to receive the request to shutdown all nodes.
  WaitForShutdownAllRequest();

  // Complete the node-removal request.
  TriggerAllNodesRemoved();

  // The shutdown manager should call `system_powerctl(ZX_SYSTEM_POWERCTL_REBOOT)`.
  WaitForSystemPowerctl();
  AssertShutdownManager([](TestShutdownManager* manager) {
    EXPECT_TRUE(manager->system_powerctl_called());
    EXPECT_EQ(ZX_SYSTEM_POWERCTL_REBOOT, manager->system_powerctl_cmd());
  });
}

}  // namespace

}  // namespace driver_manager::test
