// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/tests/mock-spinel-host/mock_spinel_host_fixture.h"

#include <zircon/errors.h>

#include <string>
#include <utility>

#include <gtest/gtest.h>

#include "src/lib/testing/predicates/status.h"

namespace ot_rcp::testing {

zx_status_t MockSpinelHostFixture::SetChannel(fidl::Client<fspinel::DeviceSetup>& setup,
                                              MockSpinelHost& host) {
  auto [client_end, server_end] = fidl::Endpoints<fspinel::Device>::Create();
  fidl::WireResult result = setup.wire_sync()->SetChannel(std::move(server_end));
  if (!result.ok()) {
    return result.status();
  }
  if (result->is_error()) {
    return result->error_value();
  }
  host.Bind(std::move(client_end), dispatcher());
  return ZX_OK;
}

void MockSpinelHostFixture::OpenDevice(MockSpinelHost& host) {
  host.ResetSendWindow();
  fidl::WireResult result = host.client().wire_sync()->Open();
  ASSERT_TRUE(result.ok()) << "Open() transport error: " << result.FormatDescription();
  ASSERT_FALSE(result->is_error())
      << "Open() failed: " << static_cast<uint32_t>(result->error_value());
  // The sync call only consumes the reply; events queued on the channel are dispatched by the
  // async loop.
  RunLoopUntilIdle();
}

void MockSpinelHostFixture::CloseDevice(MockSpinelHost& host) {
  fidl::WireResult result = host.client().wire_sync()->Close();
  ASSERT_TRUE(result.ok()) << "Close() transport error: " << result.FormatDescription();
  ASSERT_FALSE(result->is_error())
      << "Close() failed: " << static_cast<uint32_t>(result->error_value());
}

std::optional<uint32_t> MockSpinelHostFixture::GetMaxFrameSize(MockSpinelHost& host) {
  fidl::WireResult result = host.client().wire_sync()->GetMaxFrameSize();
  if (!result.ok()) {
    return std::nullopt;
  }
  return result->size;
}

std::optional<SpinelFrame> MockSpinelHostFixture::WaitForFrame(
    MockSpinelHost& host, fit::function<bool(const SpinelFrame&)> pred, zx::duration timeout) {
  std::optional<SpinelFrame> found;
  RunLoopWithTimeoutOrUntil(
      [&] {
        found = host.TakeFrame(pred);
        return found.has_value() || host.unbound();
      },
      timeout);
  return found;
}

std::optional<SpinelFrame> MockSpinelHostFixture::WaitForResponse(MockSpinelHost& host, uint8_t tid,
                                                                  zx::duration timeout) {
  return WaitForFrame(host, [tid](const SpinelFrame& f) { return f.tid() == tid; }, timeout);
}

std::optional<SpinelFrame> MockSpinelHostFixture::WaitForResetNotification(MockSpinelHost& host) {
  return WaitForFrame(host, [](const SpinelFrame& f) { return f.IsResetNotification(); });
}

void MockSpinelHostFixture::GrantInboundWindowAndWaitForRcpToBeReady(
    MockSpinelHost& host, spinel_status_t expected_reset_status) {
  // Should increase inbound window when receiving.
  host.set_auto_replenish(true);
  ASSERT_OK(host.ReadyToReceive(kHostInboundWindow));
  ExpectResetNotification(WaitForResetNotification(host), expected_reset_status);
}

void MockSpinelHostFixture::SendWhenReady(MockSpinelHost& host, std::vector<uint8_t> data) {
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&host] { return host.send_credit() > 0 || host.unbound(); }, kTimeout))
      << "no radio-bound credit available";
  ASSERT_FALSE(host.unbound()) << "channel closed while waiting for radio-bound credit";
  ASSERT_OK(host.SendRaw(std::move(data))) << "SendFrame failed";
}

std::optional<SpinelFrame> MockSpinelHostFixture::Transact(MockSpinelHost& host,
                                                           unsigned int command,
                                                           std::optional<unsigned int> prop,
                                                           const std::vector<uint8_t>& value,
                                                           zx::duration timeout) {
  if (!RunLoopWithTimeoutOrUntil([&host] { return host.send_credit() > 0 || host.unbound(); },
                                 timeout) ||
      host.unbound()) {
    ADD_FAILURE() << "no radio-bound credit available (or channel closed)";
    return std::nullopt;
  }
  const uint8_t tid = host.NextTid();
  if (zx::result<> sent = host.SendRaw(BuildSpinelFrame(tid, command, prop, value));
      sent.is_error()) {
    ADD_FAILURE() << "SendFrame failed: " << sent.status_string();
    return std::nullopt;
  }
  std::optional<SpinelFrame> response = WaitForResponse(host, tid, timeout);
  if (!response.has_value()) {
    ADD_FAILURE() << "no response for command " << command << " prop "
                  << (prop.has_value() ? std::to_string(*prop) : "none") << " tid "
                  << static_cast<int>(tid);
  }
  return response;
}

std::optional<std::vector<uint8_t>> MockSpinelHostFixture::Get(MockSpinelHost& host,
                                                               unsigned int prop) {
  std::optional<SpinelFrame> response = Transact(host, SPINEL_CMD_PROP_VALUE_GET, prop);
  if (!response.has_value()) {
    return std::nullopt;
  }
  if (!response->IsPropValueIs(prop)) {
    ADD_FAILURE() << "GET prop " << prop << " answered with " << response->Describe();
    return std::nullopt;
  }
  return response->payload();
}

std::optional<std::vector<uint8_t>> MockSpinelHostFixture::Set(MockSpinelHost& host,
                                                               unsigned int prop,
                                                               const std::vector<uint8_t>& value) {
  std::optional<SpinelFrame> response = Transact(host, SPINEL_CMD_PROP_VALUE_SET, prop, value);
  if (!response.has_value()) {
    return std::nullopt;
  }
  if (response->IsPropValueIs(prop)) {
    return response->payload();
  }
  if (response->LastStatus() == std::optional<unsigned int>(SPINEL_STATUS_OK)) {
    return std::vector<uint8_t>{};
  }
  ADD_FAILURE() << "SET prop " << prop << " answered with " << response->Describe();
  return std::nullopt;
}

void MockSpinelHostFixture::ExpectResponsive(MockSpinelHost& host) {
  ExpectLastStatus(Transact(host, SPINEL_CMD_NOOP, std::nullopt), SPINEL_STATUS_OK);
}

std::optional<unsigned int> MockSpinelHostFixture::GetUintPacked(MockSpinelHost& host,
                                                                 unsigned int prop) {
  std::optional<std::vector<uint8_t>> payload = Get(host, prop);
  if (!payload.has_value()) {
    return std::nullopt;
  }
  std::optional<unsigned int> value = UnpackUintPacked(*payload);
  if (!value.has_value()) {
    ADD_FAILURE() << "prop " << prop << ": payload is not a packed uint";
  }
  return value;
}

void MockSpinelHostFixture::EnableRawLink(MockSpinelHost& host, uint16_t pan_id,
                                          uint16_t short_address, uint8_t channel) {
  ASSERT_TRUE(Set(host, SPINEL_PROP_PHY_ENABLED, PackSpinelValue(SPINEL_DATATYPE_BOOL_S, true))
                  .has_value());
  ASSERT_TRUE(
      Set(host, SPINEL_PROP_MAC_15_4_PANID, PackSpinelValue(SPINEL_DATATYPE_UINT16_S, pan_id))
          .has_value());
  ASSERT_TRUE(Set(host, SPINEL_PROP_MAC_15_4_SADDR,
                  PackSpinelValue(SPINEL_DATATYPE_UINT16_S, short_address))
                  .has_value());
  ASSERT_TRUE(Set(host, SPINEL_PROP_PHY_CHAN, PackSpinelValue(SPINEL_DATATYPE_UINT8_S, channel))
                  .has_value());
  ASSERT_TRUE(
      Set(host, SPINEL_PROP_MAC_RAW_STREAM_ENABLED, PackSpinelValue(SPINEL_DATATYPE_BOOL_S, true))
          .has_value());
}

std::optional<unsigned int> MockSpinelHostFixture::StreamRawTxDoneStatus(
    const std::optional<SpinelFrame>& response) {
  if (!response.has_value()) {
    ADD_FAILURE() << "no TX completion";
    return std::nullopt;
  }
  std::optional<StreamRawTxDone> done = ParseStreamRawTxDone(*response);
  if (!done.has_value()) {
    ADD_FAILURE() << "TX completion is not a LAST_STATUS with the frame_pending/header_updated "
                     "fields; got "
                  << response->Describe();
    return std::nullopt;
  }
  EXPECT_FALSE(done->frame_pending);
  return done->status;
}

void MockSpinelHostFixture::ExpectLastStatus(const std::optional<SpinelFrame>& frame,
                                             spinel_status_t expected_status) {
  ASSERT_TRUE(frame.has_value()) << "no frame";
  EXPECT_EQ(frame->LastStatus(), std::optional<unsigned int>(expected_status)) << frame->Describe();
}

void MockSpinelHostFixture::ExpectResetNotification(const std::optional<SpinelFrame>& reset,
                                                    spinel_status_t expected_status) {
  ASSERT_TRUE(reset.has_value()) << "no LAST_STATUS(RESET_*) notification";
  EXPECT_EQ(reset->tid(), 0u) << reset->Describe();
  ExpectLastStatus(reset, expected_status);
}

void MockSpinelHostFixture::ExpectHealthy(const MockSpinelHost& host) {
  EXPECT_TRUE(host.errors().empty()) << host.errors().size() << " OnError event(s)";
  EXPECT_FALSE(host.unbound()) << host.unbind_info()->FormatDescription();
}

}  // namespace ot_rcp::testing
