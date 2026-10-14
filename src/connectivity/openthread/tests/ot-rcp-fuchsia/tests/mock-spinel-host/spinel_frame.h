// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_TESTS_MOCK_SPINEL_HOST_SPINEL_FRAME_H_
#define SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_TESTS_MOCK_SPINEL_HOST_SPINEL_FRAME_H_

#include <cstdint>
#include <optional>
#include <string>
#include <vector>

// Spinel definitions from OpenThread.
#include <spinel/spinel.h>

namespace ot_rcp::testing {

// Decoded Spinel frame: `header | command (packed uint) | [prop (packed uint)] | payload`.
// Immutable once parsed.
class SpinelFrame {
 public:
  // Parses a raw frame received via `OnReceiveFrame`. Returns nullopt on malformed input.
  static std::optional<SpinelFrame> Parse(const std::vector<uint8_t>& raw);

  uint8_t header() const { return header_; }
  uint8_t tid() const { return SPINEL_HEADER_GET_TID(header_); }
  unsigned int command() const { return command_; }
  // Only meaningful for PROP_VALUE_* commands.
  unsigned int prop() const { return prop_; }
  // Bytes after the property key (or after the command, for commands without one).
  const std::vector<uint8_t>& payload() const { return payload_; }

  bool IsPropValueIs(unsigned int key) const {
    return command_ == SPINEL_CMD_PROP_VALUE_IS && prop_ == key;
  }
  bool IsLastStatus() const { return IsPropValueIs(SPINEL_PROP_LAST_STATUS); }

  // Returns the LAST_STATUS status code, if this frame is a LAST_STATUS frame.
  std::optional<unsigned int> LastStatus() const;

  // True if this is an unsolicited (TID 0) LAST_STATUS carrying a RESET_* status.
  bool IsResetNotification() const;

  // Human-readable summary for test failure messages, e.g.
  // "SpinelFrame{header=0x81 tid=1 cmd=6 prop=0 payload_len=1 last_status=0}".
  std::string Describe() const;

 private:
  SpinelFrame() = default;

  uint8_t header_ = 0;
  unsigned int command_ = 0;
  unsigned int prop_ = 0;
  std::vector<uint8_t> payload_;
};

// Packs a Spinel value using OT's own encoder. `format` is a SPINEL_DATATYPE_* string.
template <typename... Args>
std::vector<uint8_t> PackSpinelValue(const char* format, Args... args) {
  spinel_ssize_t needed = spinel_datatype_pack(nullptr, 0, format, args...);
  if (needed <= 0) {
    return {};
  }
  std::vector<uint8_t> out(static_cast<size_t>(needed));
  spinel_ssize_t written =
      spinel_datatype_pack(out.data(), static_cast<spinel_size_t>(out.size()), format, args...);
  if (written != needed) {
    return {};
  }
  return out;
}

// Builds `header(TID, IID 0) | command | [prop] | value`.
std::vector<uint8_t> BuildSpinelFrame(uint8_t tid, unsigned int command,
                                      std::optional<unsigned int> prop,
                                      const std::vector<uint8_t>& value = {});

// Mirrors `OPENTHREAD_CONFIG_MAC_MAX_CSMA_BACKOFFS_DIRECT`.
constexpr uint8_t kMacMaxCsmaBackoffsDirect = 4;

// Builds a STREAM_RAW property value the way lowpan-ot-driver does.
std::vector<uint8_t> StreamRawValue(const std::vector<uint8_t>& psdu, uint8_t channel,
                                    uint8_t max_frame_retries = 0);

// The TX completion payload for a STREAM_RAW set.
struct StreamRawTxDone {
  unsigned int status = 0;
  bool frame_pending = false;
  bool header_updated = false;
};

// Decodes a TX completion. `nullopt` indicates failure.
std::optional<StreamRawTxDone> ParseStreamRawTxDone(const SpinelFrame& frame);

// Decodes a single packed uint property value. `nullopt` indicates failure.
std::optional<unsigned int> UnpackUintPacked(const std::vector<uint8_t>& payload);

// Decodes an EUI-64 property value (sent MSB first) as a host-order integer. `nullopt`
// indicates failure.
std::optional<uint64_t> UnpackEui64(const std::vector<uint8_t>& payload);

}  // namespace ot_rcp::testing

#endif  // SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_TESTS_MOCK_SPINEL_HOST_SPINEL_FRAME_H_
