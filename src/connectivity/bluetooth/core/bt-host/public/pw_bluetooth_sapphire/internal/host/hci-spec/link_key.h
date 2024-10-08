// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_BLUETOOTH_CORE_BT_HOST_PUBLIC_PW_BLUETOOTH_SAPPHIRE_INTERNAL_HOST_HCI_SPEC_LINK_KEY_H_
#define SRC_CONNECTIVITY_BLUETOOTH_CORE_BT_HOST_PUBLIC_PW_BLUETOOTH_SAPPHIRE_INTERNAL_HOST_HCI_SPEC_LINK_KEY_H_

#include <cstdint>

#include "src/connectivity/bluetooth/core/bt-host/public/pw_bluetooth_sapphire/internal/host/common/uint128.h"

#include <pw_bluetooth/hci_commands.emb.h>

namespace bt::hci_spec {

// Represents a key used to encrypt a link.
class LinkKey final {
 public:
  LinkKey() : rand_(0), ediv_(0) { value_.fill(0); }
  LinkKey(const UInt128& value, uint64_t rand, uint16_t ediv)
      : value_(value), rand_(rand), ediv_(ediv) {}

  // 128-bit BR/EDR link key, LE Long Term Key, or LE Short Term key.
  const UInt128& value() const { return value_; }

  // Encrypted diversifier and random values used to identify the LTK. These
  // values are set to 0 for the LE Legacy STK, LE Secure Connections LTK, and
  // BR/EDR Link Key.
  uint64_t rand() const { return rand_; }
  uint16_t ediv() const { return ediv_; }

  bool operator!=(const LinkKey& other) const { return !(*this == other); }
  bool operator==(const LinkKey& other) const {
    return value() == other.value() && rand() == other.rand() &&
           ediv() == other.ediv();
  }

  auto view() { return pw::bluetooth::emboss::MakeLinkKeyView(&value_); }

 private:
  UInt128 value_;
  uint64_t rand_;
  uint16_t ediv_;
};

}  // namespace bt::hci_spec

#endif  // SRC_CONNECTIVITY_BLUETOOTH_CORE_BT_HOST_PUBLIC_PW_BLUETOOTH_SAPPHIRE_INTERNAL_HOST_HCI_SPEC_LINK_KEY_H_
