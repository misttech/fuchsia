// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_BLUETOOTH_HCI_VENDOR_BROADCOM_PACKETS_H_
#define SRC_CONNECTIVITY_BLUETOOTH_HCI_VENDOR_BROADCOM_PACKETS_H_

#include <endian.h>
#include <stdint.h>
#include <zircon/compiler.h>

#include <cstddef>
#include <limits>

namespace bt_hci_broadcom {

// Official HCI definitions:

struct HciCommandHeader {
  uint16_t opcode;
  uint8_t parameter_total_size;
} __PACKED;

struct HciEventHeader {
  uint8_t event_code;
  uint8_t parameter_total_size;
} __PACKED;

constexpr size_t kMacAddrLen = 6;

// Vendor HCI definitions:

constexpr uint16_t kStartFirmwareDownloadCmdOpCode = 0xfc2e;

struct BcmSetBdaddrCmd {
  HciCommandHeader header;
  uint8_t bdaddr[kMacAddrLen];
} __PACKED;
constexpr uint16_t kBcmSetBdaddrCmdOpCode = 0xfc01;

struct BcmSetAclPriorityCmd {
  HciCommandHeader header;
  uint16_t connection_handle;
  uint8_t priority;
  uint8_t direction;
} __PACKED;

constexpr uint16_t kBcmSetAclPriorityCmdOpCode = ((0x3F << 10) | 0x11A);
constexpr uint8_t kBcmAclPriorityNormal = 0x00;
constexpr uint8_t kBcmAclPriorityHigh = 0x01;
constexpr uint8_t kBcmAclDirectionSource = 0x00;
constexpr uint8_t kBcmAclDirectionSink = 0x01;
constexpr size_t kBcmSetAclPriorityCmdSize = sizeof(BcmSetAclPriorityCmd);

constexpr size_t kMaxHciCommandSize =
    sizeof(HciCommandHeader) +
    std::numeric_limits<decltype(HciCommandHeader::parameter_total_size)>::max();

// Max size of an event frame.
constexpr size_t kChanReadBufLen =
    sizeof(HciEventHeader) +
    std::numeric_limits<decltype(HciEventHeader::parameter_total_size)>::max();

// vendor command to begin firmware download
const HciCommandHeader kStartFirmwareDownloadCmd = {
    .opcode = htole16(kStartFirmwareDownloadCmdOpCode),
    .parameter_total_size = 0,
};

// Set Max TX Power

}  // namespace bt_hci_broadcom

#endif  // SRC_CONNECTIVITY_BLUETOOTH_HCI_VENDOR_BROADCOM_PACKETS_H_
