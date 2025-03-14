// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_MAC_TEST_UTIL_BANJO_H_
#define SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_MAC_TEST_UTIL_BANJO_H_

#include <lib/zx/event.h>

#include <vector>

#include <gtest/gtest.h>

#include "mac_interface.h"

namespace network::testing::banjo {

class FakeMacDeviceImpl : public ddk::MacAddrProtocol<FakeMacDeviceImpl> {
 public:
  FakeMacDeviceImpl();

  zx::result<std::unique_ptr<MacAddrDeviceInterface>> CreateChild();

  void MacAddrGetAddress(mac_address_t* out_mac);
  void MacAddrGetFeatures(features_t* out_features);

  void MacAddrSetMode(mode_t mode, const mac_address_t* multicast_macs_list,
                      size_t multicast_macs_count);

  zx_status_t WaitConfigurationChanged();

  const fuchsia_net::wire::MacAddress& mac() { return mac_; }

  features_t& features() { return features_; }

  mode_t mode() {
    EXPECT_TRUE(mode_.has_value());
    return mode_.value();
  }

  std::vector<MacAddress>& addresses() { return addresses_; }

  mac_addr_protocol_t proto() { return {.ops = &mac_addr_protocol_ops_, .ctx = this}; }

 private:
  fuchsia_net::wire::MacAddress mac_ = {0x00, 0x02, 0x03, 0x04, 0x05, 0x06};
  features_t features_{};
  std::optional<mode_t> mode_ = std::nullopt;

  std::vector<MacAddress> addresses_;
  zx::event event_;
};

}  // namespace network::testing::banjo

#endif  // SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_MAC_TEST_UTIL_BANJO_H_
