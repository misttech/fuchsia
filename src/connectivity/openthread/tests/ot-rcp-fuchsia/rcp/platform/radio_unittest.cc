// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Unit tests for the ot-rcp-fuchsia stub radio (radio.cc).
//
// The OpenThread core is not linked: the radio callbacks into the core (otPlatRadioTxStarted,
// otPlatRadioTxDone) are faked.

#include <lib/async-testing/test_loop.h>
#include <lib/fit/function.h>

#include <cstdint>
#include <cstring>

#include <gtest/gtest.h>
#include <openthread/platform/radio.h>

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/platform/platform.h"

namespace {

// Fake OpenThread implementation.

// Records the radio's calls into the (faked) OpenThread SubMac.
struct FakeSubMac {
  int tx_started_calls = 0;
  otRadioFrame* last_started_frame = nullptr;
  int tx_done_calls = 0;
  otRadioFrame* last_done_frame = nullptr;
  otRadioFrame* last_done_ack = nullptr;
  otError last_done_error = OT_ERROR_NONE;
  // Optional action run from within otPlatRadioTxDone(), to exercise re-entry into the radio the
  // way the SubMac does.
  fit::closure on_tx_done;
};

FakeSubMac& SubMac() {
  static FakeSubMac sub_mac;
  return sub_mac;
}

}  // namespace

extern "C" {

void otPlatRadioTxStarted(otInstance* aInstance, otRadioFrame* aFrame) {
  EXPECT_NE(aInstance, nullptr);
  FakeSubMac& sub_mac = SubMac();
  sub_mac.tx_started_calls++;
  sub_mac.last_started_frame = aFrame;
  // TxStarted always precedes TxDone for the same transmission.
  EXPECT_EQ(sub_mac.tx_started_calls, sub_mac.tx_done_calls + 1);
}

void otPlatRadioTxDone(otInstance* aInstance, otRadioFrame* aFrame, otRadioFrame* aAckFrame,
                       otError aError) {
  EXPECT_NE(aInstance, nullptr);
  FakeSubMac& sub_mac = SubMac();
  sub_mac.tx_done_calls++;
  sub_mac.last_done_frame = aFrame;
  sub_mac.last_done_ack = aAckFrame;
  sub_mac.last_done_error = aError;
  if (sub_mac.on_tx_done) {
    sub_mac.on_tx_done();
  }
}

}  // extern "C"

namespace ot_rcp {
namespace {

constexpr uint32_t kNodeId = 7;
constexpr uint8_t kChannel = 15;

// Referencing IEEE 802.15.4-2015 §7.2.1 Frame Control Field.
constexpr uint8_t kFcfTypeData = 0x1;
constexpr uint8_t kFcfAckRequest = 1 << 5;
constexpr uint8_t kFcfPanIdCompression = 1 << 6;
constexpr uint8_t kFcfDstAddrShort = 0b10 << 2;
constexpr uint8_t kFcfVersion2003 = 0b00 << 4;
constexpr uint8_t kFcfSrcAddrShort = 0b10 << 6;

// Length of a minimal data frame: 9-byte MHR (FCF 2, sequence 1, destination PAN ID 2,
// short destination 2, short source 2) + 5-byte payload + 2-byte FCS.
constexpr uint16_t kFrameLength = 16;

// `otInstance` is only passed by platform code.
otInstance* FakeInstance() {
  static uint64_t storage;
  return reinterpret_cast<otInstance*>(&storage);
}

class RadioTest : public ::testing::Test {
 protected:
  void SetUp() override {
    SubMac() = FakeSubMac();
    Platform::Create(loop_.dispatcher(), kNodeId);
  }

  void TearDown() override { Platform::Destroy(); }

  // Brings the radio to the Receive state
  void EnableAndReceive() {
    ASSERT_EQ(otPlatRadioEnable(FakeInstance()), OT_ERROR_NONE);
    ASSERT_EQ(otPlatRadioReceive(FakeInstance(), kChannel), OT_ERROR_NONE);
    ASSERT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_RECEIVE);
  }

  // Fills the radio's transmit buffer with a minimal data frame (short addresses, PAN ID
  // compression) and returns it.
  otRadioFrame* PrepareFrame(bool ack_request) {
    otRadioFrame* frame = otPlatRadioGetTransmitBuffer(FakeInstance());
    frame->mChannel = kChannel;
    frame->mLength = kFrameLength;
    frame->mPsdu[0] = kFcfTypeData | kFcfPanIdCompression | (ack_request ? kFcfAckRequest : 0);
    frame->mPsdu[1] = kFcfDstAddrShort | kFcfVersion2003 | kFcfSrcAddrShort;
    return frame;
  }

  async::TestLoop loop_;
};

// State machine

TEST_F(RadioTest, StartsDisabledAndEnableMovesToSleep) {
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_DISABLED);
  EXPECT_FALSE(otPlatRadioIsEnabled(FakeInstance()));

  EXPECT_EQ(otPlatRadioEnable(FakeInstance()), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_SLEEP);
  EXPECT_TRUE(otPlatRadioIsEnabled(FakeInstance()));

  // Enabling an enabled radio keeps its current state.
  EXPECT_EQ(otPlatRadioReceive(FakeInstance(), kChannel), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioEnable(FakeInstance()), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_RECEIVE);
}

TEST_F(RadioTest, ReceiveAndSleepRequireEnabledRadio) {
  EXPECT_EQ(otPlatRadioReceive(FakeInstance(), kChannel), OT_ERROR_INVALID_STATE);
  EXPECT_EQ(otPlatRadioSleep(FakeInstance()), OT_ERROR_INVALID_STATE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_DISABLED);
}

TEST_F(RadioTest, SleepAndReceiveAlternate) {
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  EXPECT_EQ(otPlatRadioSleep(FakeInstance()), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_SLEEP);
  EXPECT_EQ(otPlatRadioReceive(FakeInstance(), kChannel), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_RECEIVE);
}

// Disable from Transmit is covered by DisableDuringTransmitDropsCompletion.
TEST_F(RadioTest, DisableReturnsToDisabledFromSleepAndReceive) {
  ASSERT_EQ(otPlatRadioEnable(FakeInstance()), OT_ERROR_NONE);
  ASSERT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_SLEEP);
  EXPECT_EQ(otPlatRadioDisable(FakeInstance()), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_DISABLED);
  EXPECT_FALSE(otPlatRadioIsEnabled(FakeInstance()));

  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  EXPECT_EQ(otPlatRadioDisable(FakeInstance()), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_DISABLED);
  EXPECT_FALSE(otPlatRadioIsEnabled(FakeInstance()));
}

// Transmit

TEST_F(RadioTest, TransmitRequiresReceiveState) {
  otRadioFrame* frame = PrepareFrame(/*ack_request=*/false);

  EXPECT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_INVALID_STATE);  // Disabled.
  ASSERT_EQ(otPlatRadioEnable(FakeInstance()), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_INVALID_STATE);  // Sleep.

  loop_.RunUntilIdle();
  EXPECT_EQ(SubMac().tx_started_calls, 0);
  EXPECT_EQ(SubMac().tx_done_calls, 0);
}

// OpenThread always transmits the frame obtained from otPlatRadioGetTransmitBuffer().
TEST_F(RadioTest, TransmitRejectsForeignFrame) {
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  uint8_t psdu[OT_RADIO_FRAME_MAX_SIZE] = {};
  otRadioFrame foreign = {};
  foreign.mPsdu = psdu;
  foreign.mLength = kFrameLength;
  foreign.mChannel = kChannel;

  EXPECT_EQ(otPlatRadioTransmit(FakeInstance(), &foreign), OT_ERROR_INVALID_STATE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_RECEIVE);
}

TEST_F(RadioTest, TransmitOneFrame) {
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  otRadioFrame* frame = PrepareFrame(/*ack_request=*/false);

  ASSERT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_TRANSMIT);
  EXPECT_EQ(SubMac().tx_started_calls, 0);
  EXPECT_EQ(SubMac().tx_done_calls, 0);

  loop_.RunUntilIdle();
  EXPECT_EQ(SubMac().tx_started_calls, 1);
  EXPECT_EQ(SubMac().last_started_frame, frame);
  EXPECT_EQ(SubMac().tx_done_calls, 1);
  EXPECT_EQ(SubMac().last_done_frame, frame);
  EXPECT_EQ(SubMac().last_done_ack, nullptr);
  EXPECT_EQ(SubMac().last_done_error, OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_RECEIVE);
}

TEST_F(RadioTest, TransmitWhileTransmittingIsRejected) {
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  otRadioFrame* frame = PrepareFrame(/*ack_request=*/false);

  ASSERT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_NONE);
  EXPECT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_INVALID_STATE);

  loop_.RunUntilIdle();
  EXPECT_EQ(SubMac().tx_started_calls, 1);
  EXPECT_EQ(SubMac().tx_done_calls, 1);
}

TEST_F(RadioTest, DisableDuringTransmitDropsCompletion) {
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  otRadioFrame* frame = PrepareFrame(/*ack_request=*/false);
  ASSERT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_NONE);

  EXPECT_EQ(otPlatRadioDisable(FakeInstance()), OT_ERROR_NONE);
  loop_.RunUntilIdle();
  EXPECT_EQ(SubMac().tx_started_calls, 0);
  EXPECT_EQ(SubMac().tx_done_calls, 0);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_DISABLED);

  // The radio is usable again afterwards.
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  ASSERT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_NONE);
  loop_.RunUntilIdle();
  EXPECT_EQ(SubMac().tx_done_calls, 1);
}

TEST_F(RadioTest, TransmitFromWithinTxDoneIsAccepted) {
  ASSERT_NO_FATAL_FAILURE(EnableAndReceive());
  otRadioFrame* frame = PrepareFrame(/*ack_request=*/true);
  SubMac().on_tx_done = [frame] {
    if (SubMac().tx_done_calls == 1) {
      EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_RECEIVE);
      EXPECT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_NONE);
    }
  };

  ASSERT_EQ(otPlatRadioTransmit(FakeInstance(), frame), OT_ERROR_NONE);
  loop_.RunUntilIdle();
  EXPECT_EQ(SubMac().tx_started_calls, 2);
  EXPECT_EQ(SubMac().tx_done_calls, 2);
  EXPECT_EQ(otPlatRadioGetState(FakeInstance()), OT_RADIO_STATE_RECEIVE);
}

}  // namespace
}  // namespace ot_rcp
