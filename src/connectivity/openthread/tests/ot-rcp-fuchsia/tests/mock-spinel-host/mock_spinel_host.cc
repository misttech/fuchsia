// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/tests/mock-spinel-host/mock_spinel_host.h"

#include <utility>

namespace ot_rcp::testing {

void MockSpinelHost::Bind(fidl::ClientEnd<fspinel::Device> client_end,
                          async_dispatcher_t* dispatcher) {
  client_.Bind(std::move(client_end), dispatcher, this);
}

void MockSpinelHost::OnReadyForSendFrames(
    fidl::Event<fspinel::Device::OnReadyForSendFrames>& event) {
  grants_.push_back(event.number_of_frames());
  send_credit_ += event.number_of_frames();
}

void MockSpinelHost::OnReceiveFrame(fidl::Event<fspinel::Device::OnReceiveFrame>& event) {
  ++received_count_;
  rx_.push_back(event.data());
  if (auto_replenish_) {
    // Grant one more inbound frame per frame received. A failure here means the channel is
    // closing, which is reported through `on_fidl_error`.
    (void)ReadyToReceive(1);
  }
}

void MockSpinelHost::OnError(fidl::Event<fspinel::Device::OnError>& event) {
  errors_.push_back({event.error(), event.did_close()});
}

void MockSpinelHost::on_fidl_error(fidl::UnbindInfo info) { unbind_info_ = info; }

zx::result<> MockSpinelHost::SendRaw(std::vector<uint8_t> data) {
  fit::result<fidl::OneWayError> result =
      client_->SendFrame(fspinel::DeviceSendFrameRequest{{.data = std::move(data)}});
  if (result.is_error()) {
    return zx::error(result.error_value().status());
  }
  --send_credit_;
  return zx::ok();
}

zx::result<> MockSpinelHost::ReadyToReceive(uint32_t n) {
  fit::result<fidl::OneWayError> result = client_->ReadyToReceiveFrames(
      fspinel::DeviceReadyToReceiveFramesRequest{{.number_of_frames = n}});
  if (result.is_error()) {
    return zx::error(result.error_value().status());
  }
  return zx::ok();
}

uint8_t MockSpinelHost::NextTid() {
  next_tid_ = SPINEL_GET_NEXT_TID(next_tid_);
  return next_tid_;
}

std::optional<SpinelFrame> MockSpinelHost::TakeFrame(
    const fit::function<bool(const SpinelFrame&)>& pred) {
  for (auto it = rx_.begin(); it != rx_.end(); ++it) {
    std::optional<SpinelFrame> frame = SpinelFrame::Parse(*it);
    if (frame.has_value() && pred(*frame)) {
      rx_.erase(it);
      return frame;
    }
  }
  return std::nullopt;
}

uint64_t MockSpinelHost::total_granted() const {
  uint64_t total = 0;
  for (uint32_t g : grants_) {
    total += g;
  }
  return total;
}

std::vector<uint8_t> NoopFrame(uint8_t tid) {
  return BuildSpinelFrame(tid, SPINEL_CMD_NOOP, std::nullopt);
}

std::vector<uint8_t> ResetCommand(uint8_t tid, uint8_t reset_type) {
  return BuildSpinelFrame(tid, SPINEL_CMD_RESET, std::nullopt, {reset_type});
}

}  // namespace ot_rcp::testing
