// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Basic round-trip tests for the Spinel codec in spinel_frame.h. Everything else in
// mock-spinel-host is exercised against a real RCP by the integration test.

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/tests/mock-spinel-host/spinel_frame.h"

#include <cstdint>
#include <optional>
#include <vector>

#include <gtest/gtest.h>

namespace ot_rcp::testing {
namespace {

TEST(SpinelFrameTest, CommandRoundTrips) {
  const std::vector<uint8_t> value = PackSpinelValue(SPINEL_DATATYPE_UINT16_S, 0x1234);
  const std::vector<uint8_t> raw =
      BuildSpinelFrame(/*tid=*/3, SPINEL_CMD_PROP_VALUE_SET, SPINEL_PROP_MAC_15_4_PANID, value);

  EXPECT_EQ(raw[0], SPINEL_HEADER_FLAG | SPINEL_HEADER_IID_0 | 3);
  std::optional<SpinelFrame> frame = SpinelFrame::Parse(raw);
  ASSERT_TRUE(frame.has_value());
  EXPECT_EQ(frame->tid(), 3u);
  EXPECT_EQ(frame->command(), SPINEL_CMD_PROP_VALUE_SET);
  EXPECT_EQ(frame->prop(), SPINEL_PROP_MAC_15_4_PANID);
  EXPECT_EQ(frame->payload(), value);
}

TEST(SpinelFrameTest, LastStatusAndResetNotification) {
  auto last_status = [](uint8_t tid, unsigned int status) {
    return SpinelFrame::Parse(
        BuildSpinelFrame(tid, SPINEL_CMD_PROP_VALUE_IS, SPINEL_PROP_LAST_STATUS,
                         PackSpinelValue(SPINEL_DATATYPE_UINT_PACKED_S, status)));
  };

  std::optional<SpinelFrame> ok = last_status(/*tid=*/4, SPINEL_STATUS_OK);
  ASSERT_TRUE(ok.has_value());
  EXPECT_EQ(ok->LastStatus(), std::optional<unsigned int>(SPINEL_STATUS_OK));
  EXPECT_FALSE(ok->IsResetNotification());

  // A reset notification is an unsolicited (TID 0) LAST_STATUS(RESET_*).
  EXPECT_TRUE(last_status(0, SPINEL_STATUS_RESET_POWER_ON)->IsResetNotification());
  EXPECT_FALSE(last_status(1, SPINEL_STATUS_RESET_POWER_ON)->IsResetNotification());
}

TEST(SpinelFrameTest, MalformedInputIsRejected) {
  EXPECT_FALSE(SpinelFrame::Parse({}).has_value());
  // PROP_VALUE_GET with a truncated (continuation bit set) property id.
  EXPECT_FALSE(
      SpinelFrame::Parse({SPINEL_HEADER_FLAG, SPINEL_CMD_PROP_VALUE_GET, 0x80}).has_value());
}

}  // namespace
}  // namespace ot_rcp::testing
