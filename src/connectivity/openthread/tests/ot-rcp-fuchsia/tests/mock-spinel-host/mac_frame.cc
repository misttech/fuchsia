// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/tests/mock-spinel-host/mac_frame.h"

namespace ot_rcp::testing {

std::vector<uint8_t> DataFrame(uint16_t pan_id, uint16_t dst_short, uint16_t src_short, uint8_t seq,
                               bool ack_request) {
  return {
      static_cast<uint8_t>(kFcfTypeData | kFcfPanIdCompression |
                           (ack_request ? kFcfAckRequest : 0)),
      static_cast<uint8_t>(kFcfDstAddrShort | kFcfVersion2003 | kFcfSrcAddrShort),
      seq,
      static_cast<uint8_t>(pan_id & 0xff),
      static_cast<uint8_t>(pan_id >> 8),
      static_cast<uint8_t>(dst_short & 0xff),
      static_cast<uint8_t>(dst_short >> 8),
      static_cast<uint8_t>(src_short & 0xff),
      static_cast<uint8_t>(src_short >> 8),
      'o',
      't',
      '-',
      'r',
      'c',
      'p',
      0x00,
      0x00,  // FCS placeholder
  };
}

std::vector<uint8_t> BroadcastDataFrame(uint16_t pan_id, uint16_t src_short, uint8_t seq) {
  return DataFrame(pan_id, kBroadcastShortAddress, src_short, seq, /*ack_request=*/false);
}

std::vector<uint8_t> UnicastAckRequestedDataFrame(uint16_t pan_id, uint16_t dst_short,
                                                  uint16_t src_short, uint8_t seq) {
  return DataFrame(pan_id, dst_short, src_short, seq, /*ack_request=*/true);
}

}  // namespace ot_rcp::testing
