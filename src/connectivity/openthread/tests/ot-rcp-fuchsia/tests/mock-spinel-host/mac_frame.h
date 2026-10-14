// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_TESTS_MOCK_SPINEL_HOST_MAC_FRAME_H_
#define SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_TESTS_MOCK_SPINEL_HOST_MAC_FRAME_H_

#include <cstdint>
#include <vector>

// Builders for IEEE 802.15.4 MAC frames (PSDUs), e.g. the frame carried in a Spinel STREAM_RAW
// value.
namespace ot_rcp::testing {

// Frame Control Field bits (IEEE 802.15.4-2015 §7.2.1).
constexpr uint8_t kFcfTypeData = 0x1;
constexpr uint8_t kFcfAckRequest = 1 << 5;
constexpr uint8_t kFcfPanIdCompression = 1 << 6;
constexpr uint8_t kFcfDstAddrShort = 0b10 << 2;
constexpr uint8_t kFcfVersion2003 = 0b00 << 4;
constexpr uint8_t kFcfSrcAddrShort = 0b10 << 6;
constexpr uint16_t kBroadcastShortAddress = 0xffff;

// A minimal IEEE 802.15.4-2003 data frame (`Mac::Frame::kTypeData`): short addresses, PAN ID,
// 6-byte payload.
std::vector<uint8_t> DataFrame(uint16_t pan_id, uint16_t dst_short, uint16_t src_short, uint8_t seq,
                               bool ack_request);

// Wrappers around `DataFrame` that exercise different code paths in ot-rcp-fuchsia: broadcast
// without ACK request vs. unicast with ACK request.
std::vector<uint8_t> BroadcastDataFrame(uint16_t pan_id, uint16_t src_short, uint8_t seq);

std::vector<uint8_t> UnicastAckRequestedDataFrame(uint16_t pan_id, uint16_t dst_short,
                                                  uint16_t src_short, uint8_t seq);

}  // namespace ot_rcp::testing

#endif  // SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_TESTS_MOCK_SPINEL_HOST_MAC_FRAME_H_
