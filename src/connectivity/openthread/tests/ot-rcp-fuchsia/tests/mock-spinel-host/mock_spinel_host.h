// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_TESTS_MOCK_SPINEL_HOST_MOCK_SPINEL_HOST_H_
#define SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_TESTS_MOCK_SPINEL_HOST_MOCK_SPINEL_HOST_H_

#include <fidl/fuchsia.lowpan.spinel/cpp/fidl.h>
#include <lib/async/dispatcher.h>
#include <lib/fidl/cpp/client.h>
#include <lib/fit/function.h>
#include <lib/zx/result.h>

#include <cstdint>
#include <deque>
#include <optional>
#include <utility>
#include <vector>

#include <openthread/platform/radio.h>

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/tests/mock-spinel-host/spinel_frame.h"

// The client side of `fuchsia.lowpan.spinel.Device`, for tests that drive an RCP component the
// way the real host does.
namespace ot_rcp::testing {

namespace fspinel = fuchsia_lowpan_spinel;

// Inbound window the real host grants after Open (same as lowpan's `INBOUND_FRAME_WINDOW_SIZE`).
constexpr uint32_t kHostInboundWindow = 8;

// Radio capabilities the host requires of the RCP (mirrors the `Radio::kRequiredRadioCaps` in
// the platform radio.h).
constexpr uint32_t kHostRequiredRadioCaps =
    OT_RADIO_CAPS_TRANSMIT_SEC | OT_RADIO_CAPS_TRANSMIT_TIMING | OT_RADIO_CAPS_ACK_TIMEOUT |
    OT_RADIO_CAPS_TRANSMIT_RETRIES | OT_RADIO_CAPS_CSMA_BACKOFF;

class MockSpinelHost : public fidl::AsyncEventHandler<fspinel::Device> {
 public:
  struct ErrorEvent {
    fspinel::Error error;
    bool did_close;
  };

  void Bind(fidl::ClientEnd<fspinel::Device> client_end, async_dispatcher_t* dispatcher);

  fidl::Client<fspinel::Device>& client() { return client_; }

  void OnReadyForSendFrames(fidl::Event<fspinel::Device::OnReadyForSendFrames>& event) override;
  void OnReceiveFrame(fidl::Event<fspinel::Device::OnReceiveFrame>& event) override;
  void OnError(fidl::Event<fspinel::Device::OnError>& event) override;
  void on_fidl_error(fidl::UnbindInfo info) override;

  // Sends `data` via `SendFrame` and consumes one radio-bound credit on success.
  [[nodiscard]] zx::result<> SendRaw(std::vector<uint8_t> data);

  // Grants the RCP `n` more inbound frames via `ReadyToReceiveFrames`.
  [[nodiscard]] zx::result<> ReadyToReceive(uint32_t n);

  uint8_t NextTid();

  // Removes and returns the first received frame matching `pred`.
  std::optional<SpinelFrame> TakeFrame(const fit::function<bool(const SpinelFrame&)>& pred);

  void ResetSendWindow() { send_credit_ = 0; }
  void ClearGrants() { grants_.clear(); }
  void ClearReceived() {
    rx_.clear();
    received_count_ = 0;
  }
  void ClearErrors() { errors_.clear(); }

  void set_auto_replenish(bool enabled) { auto_replenish_ = enabled; }
  int64_t send_credit() const { return send_credit_; }
  const std::vector<uint32_t>& grants() const { return grants_; }
  uint64_t total_granted() const;
  size_t received_count() const { return received_count_; }
  const std::deque<std::vector<uint8_t>>& rx() const { return rx_; }
  const std::vector<ErrorEvent>& errors() const { return errors_; }
  const std::optional<fidl::UnbindInfo>& unbind_info() const { return unbind_info_; }
  bool unbound() const { return unbind_info_.has_value(); }

 private:
  fidl::Client<fspinel::Device> client_;
  std::vector<uint32_t> grants_;
  int64_t send_credit_ = 0;
  std::deque<std::vector<uint8_t>> rx_;
  size_t received_count_ = 0;
  std::vector<ErrorEvent> errors_;
  std::optional<fidl::UnbindInfo> unbind_info_;
  bool auto_replenish_ = false;
  uint8_t next_tid_ = 0;
};

std::vector<uint8_t> NoopFrame(uint8_t tid);

std::vector<uint8_t> ResetCommand(uint8_t tid, uint8_t reset_type);

}  // namespace ot_rcp::testing

#endif  // SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_TESTS_MOCK_SPINEL_HOST_MOCK_SPINEL_HOST_H_
