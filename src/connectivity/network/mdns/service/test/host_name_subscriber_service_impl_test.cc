// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/network/mdns/service/services/host_name_subscriber_service_impl.h"

#include <fuchsia/net/mdns/cpp/fidl.h>

#include "src/lib/testing/loop_fixture/real_loop_fixture.h"

namespace mdns {
namespace test {

class HostNameSubscriberServiceImplTests : public gtest::RealLoopFixture,
                                           public fuchsia::net::mdns::HostNameSubscriptionListener {
 public:
  void OnAddressesChanged(std::vector<fuchsia::net::mdns::HostAddress> addresses,
                          OnAddressesChangedCallback callback) override {
    on_addresses_changed_count_++;
    if (on_addresses_changed_handler_) {
      on_addresses_changed_handler_();
    }
    callback();
  }

  size_t on_addresses_changed_count() {
    auto result = on_addresses_changed_count_;
    on_addresses_changed_count_ = 0;
    return result;
  }

  void set_on_addresses_changed_handler(fit::closure handler) {
    on_addresses_changed_handler_ = std::move(handler);
  }

 private:
  size_t on_addresses_changed_count_ = 0;
  fit::closure on_addresses_changed_handler_;
};

class TestTransceiver : public Mdns::Transceiver {
 public:
  // Mdns::Transceiver implementation.
  void SetVerbose(bool verbose) override {}

  void Start(fuchsia::net::interfaces::WatcherPtr watcher, fit::closure link_change_callback,
             InboundMessageCallback inbound_message_callback,
             InterfaceTransceiverCreateFunction transceiver_factory) override {
    link_change_callback();
  }

  void Stop() override {}

  bool HasInterfaces() override { return true; }

  void SendMessages(
      std::unordered_map<ReplyAddress, Mdns::DnsMessageBuilder, Mdns::ReplyAddressHash> messages)
      override {}

  void LogTraffic() override {}

  std::vector<HostAddress> LocalHostAddresses() override { return local_host_addresses_; }

  void SetLocalHostAddresses(std::vector<HostAddress> addresses) {
    local_host_addresses_ = std::move(addresses);
  }

 private:
  std::vector<HostAddress> local_host_addresses_;
};

// Tests that publications outlive subscribers.
TEST_F(HostNameSubscriberServiceImplTests, SubscriptionLifetime) {
  // Instantiate |Mdns| so we can register a subscriber with it.
  TestTransceiver transceiver;
  Mdns mdns(transceiver);
  bool ready_callback_called = false;
  mdns.Start(nullptr, DnsName("TestHostName"), /* perform probe */ false,
             [&ready_callback_called]() {
               // Ready callback.
               ready_callback_called = true;
             },
             {});

  // Create the subscriber bound to the |subscriber_ptr| channel.
  fuchsia::net::mdns::HostNameSubscriberPtr subscriber_ptr;
  bool delete_callback_called = false;
  auto under_test = std::make_unique<HostNameSubscriberServiceImpl>(
      mdns, subscriber_ptr.NewRequest(), [&delete_callback_called]() {
        // Delete callback.
        delete_callback_called = true;
      });

  // Expect that the |Mdns| instance is ready and the subscriber has not requested deletion.
  RunLoopUntilIdle();
  EXPECT_TRUE(ready_callback_called);
  EXPECT_FALSE(delete_callback_called);

  // Instantiate a subscriber.
  fuchsia::net::mdns::HostNameSubscriptionListenerHandle listener_handle;
  fidl::Binding<fuchsia::net::mdns::HostNameSubscriptionListener> binding(
      this, listener_handle.NewRequest());
  zx_status_t binding_status = ZX_OK;
  binding.set_error_handler([&binding_status](zx_status_t status) { binding_status = status; });

  // Register the subscriber with the |Mdns| instance.
  subscriber_ptr->SubscribeToHostName("TestHostName",
                                      fuchsia::net::mdns::HostNameSubscriptionOptions(),
                                      std::move(listener_handle));

  // Expect the listener binding is fine and the subscriber has not requested deletion.
  RunLoopUntilIdle();
  EXPECT_EQ(ZX_OK, binding_status);
  EXPECT_FALSE(delete_callback_called);

  // Close the subscriber channel. Expect that the listener binding is fine and the subscriber has
  // requested deletion.
  subscriber_ptr = nullptr;
  RunLoopUntilIdle();
  EXPECT_EQ(ZX_OK, binding_status);
  EXPECT_TRUE(delete_callback_called);

  // Actually delete the subscriber as requested by the delete callback. Expect that the binding is
  // fine.
  under_test = nullptr;
  RunLoopUntilIdle();
  EXPECT_EQ(ZX_OK, binding_status);

  binding.Close(ZX_ERR_PEER_CLOSED);
  RunLoopUntilIdle();
}

// Tests that closing the listener connection during OnAddressesChanged doesn't cause a
// use-after-free.
TEST_F(HostNameSubscriberServiceImplTests, DisconnectionDuringOnAddressesChanged) {
  TestTransceiver transceiver;
  transceiver.SetLocalHostAddresses(
      {HostAddress(inet::IpAddress(192, 168, 1, 1), 1, zx::sec(4500))});
  Mdns mdns(transceiver);
  bool ready_callback_called = false;
  mdns.Start(nullptr, DnsName("TestHostName"), /* perform probe */ false,
             [&ready_callback_called]() { ready_callback_called = true; }, {});

  fuchsia::net::mdns::HostNameSubscriberPtr subscriber_ptr;
  auto under_test =
      std::make_unique<HostNameSubscriberServiceImpl>(mdns, subscriber_ptr.NewRequest(), []() {});

  RunLoopUntilIdle();
  EXPECT_TRUE(ready_callback_called);

  fuchsia::net::mdns::HostNameSubscriptionListenerHandle listener_handle;
  fidl::Binding<fuchsia::net::mdns::HostNameSubscriptionListener> binding(
      this, listener_handle.NewRequest());

  // Close the listener binding when OnAddressesChanged is invoked.
  set_on_addresses_changed_handler([&binding]() { binding.Unbind(); });

  subscriber_ptr->SubscribeToHostName("TestHostName",
                                      fuchsia::net::mdns::HostNameSubscriptionOptions(),
                                      std::move(listener_handle));

  RunLoopUntilIdle();
  EXPECT_EQ(1u, on_addresses_changed_count());

  set_on_addresses_changed_handler(nullptr);
  subscriber_ptr = nullptr;
  RunLoopUntilIdle();
}

// Tests that synchronous invocation of the client error handler inside OnAddressesChanged defers
// deletion of Subscriber until MaybeSendAddresses completes (regression test for b/521969422).
TEST_F(HostNameSubscriberServiceImplTests, SynchronousErrorDuringOnAddressesChanged) {
  TestTransceiver transceiver;
  std::vector<HostAddress> too_many_addresses;
  too_many_addresses.reserve(fuchsia::net::mdns::MAX_ADDRESSES + 1);
  for (size_t i = 0; i <= fuchsia::net::mdns::MAX_ADDRESSES; ++i) {
    too_many_addresses.emplace_back(inet::IpAddress(192, 168, 1, static_cast<uint8_t>(i + 1)), 1,
                                    zx::sec(4500));
  }
  transceiver.SetLocalHostAddresses(std::move(too_many_addresses));

  Mdns mdns(transceiver);
  bool ready_callback_called = false;
  mdns.Start(nullptr, DnsName("TestHostName"), /* perform probe */ false,
             [&ready_callback_called]() { ready_callback_called = true; }, {});

  fuchsia::net::mdns::HostNameSubscriberPtr subscriber_ptr;
  auto under_test =
      std::make_unique<HostNameSubscriberServiceImpl>(mdns, subscriber_ptr.NewRequest(), []() {});

  RunLoopUntilIdle();
  EXPECT_TRUE(ready_callback_called);

  fuchsia::net::mdns::HostNameSubscriptionListenerHandle listener_handle;
  fidl::Binding<fuchsia::net::mdns::HostNameSubscriptionListener> binding(
      this, listener_handle.NewRequest());
  zx_status_t binding_status = ZX_OK;
  binding.set_error_handler([&binding_status](zx_status_t status) { binding_status = status; });

  subscriber_ptr->SubscribeToHostName("TestHostName",
                                      fuchsia::net::mdns::HostNameSubscriptionOptions(),
                                      std::move(listener_handle));

  RunLoopUntilIdle();
  EXPECT_EQ(0u, on_addresses_changed_count());
  EXPECT_EQ(ZX_ERR_PEER_CLOSED, binding_status);

  subscriber_ptr = nullptr;
  RunLoopUntilIdle();
}

}  // namespace test
}  // namespace mdns
