// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/tests/mock-spinel-host/spinel_frame.h"

#include <endian.h>

#include <cstdio>
#include <cstring>

#include <openthread/platform/radio.h>

namespace ot_rcp::testing {

std::optional<SpinelFrame> SpinelFrame::Parse(const std::vector<uint8_t>& raw) {
  SpinelFrame frame;
  const uint8_t* data = raw.data();
  spinel_size_t len = static_cast<spinel_size_t>(raw.size());
  spinel_ssize_t used =
      spinel_datatype_unpack(data, len, SPINEL_DATATYPE_UINT8_S SPINEL_DATATYPE_UINT_PACKED_S,
                             &frame.header_, &frame.command_);
  if (used <= 0) {
    return std::nullopt;
  }
  data += used;
  len -= static_cast<spinel_size_t>(used);
  if (frame.command_ >= SPINEL_CMD_PROP_VALUE_GET &&
      frame.command_ <= SPINEL_CMD_PROP_VALUE_REMOVED) {
    used = spinel_datatype_unpack(data, len, SPINEL_DATATYPE_UINT_PACKED_S, &frame.prop_);
    if (used <= 0) {
      return std::nullopt;
    }
    data += used;
    len -= static_cast<spinel_size_t>(used);
  }
  frame.payload_.assign(data, data + len);
  return frame;
}

std::optional<unsigned int> SpinelFrame::LastStatus() const {
  if (!IsLastStatus()) {
    return std::nullopt;
  }
  unsigned int status = 0;
  spinel_ssize_t len =
      spinel_datatype_unpack(payload_.data(), static_cast<spinel_size_t>(payload_.size()),
                             SPINEL_DATATYPE_UINT_PACKED_S, &status);
  if (len <= 0) {
    return std::nullopt;
  }
  return status;
}

bool SpinelFrame::IsResetNotification() const {
  std::optional<unsigned int> status = LastStatus();
  return tid() == 0 && status.has_value() && *status >= SPINEL_STATUS_RESET__BEGIN &&
         *status < SPINEL_STATUS_RESET__END;
}

std::string SpinelFrame::Describe() const {
  char buf[96];
  snprintf(buf, sizeof(buf), "SpinelFrame{header=0x%02x tid=%u cmd=%u prop=%u payload_len=%zu",
           header_, tid(), command_, prop_, payload_.size());
  std::string out = buf;
  if (std::optional<unsigned int> status = LastStatus(); status.has_value()) {
    out += " last_status=" + std::to_string(*status);
  }
  return out + "}";
}

std::vector<uint8_t> BuildSpinelFrame(uint8_t tid, unsigned int command,
                                      std::optional<unsigned int> prop,
                                      const std::vector<uint8_t>& value) {
  uint8_t header = static_cast<uint8_t>(SPINEL_HEADER_FLAG | SPINEL_HEADER_IID_0 |
                                        (tid & SPINEL_HEADER_TID_MASK));
  std::vector<uint8_t> out;
  if (prop.has_value()) {
    out = PackSpinelValue(
        SPINEL_DATATYPE_UINT8_S SPINEL_DATATYPE_UINT_PACKED_S SPINEL_DATATYPE_UINT_PACKED_S, header,
        command, *prop);
  } else {
    out = PackSpinelValue(SPINEL_DATATYPE_UINT8_S SPINEL_DATATYPE_UINT_PACKED_S, header, command);
  }
  out.insert(out.end(), value.begin(), value.end());
  return out;
}

// Lowpan sends those fields out when stream raw value.
std::vector<uint8_t> StreamRawValue(const std::vector<uint8_t>& psdu, uint8_t channel,
                                    uint8_t max_frame_retries) {
  return PackSpinelValue(
      SPINEL_DATATYPE_DATA_WLEN_S                                          // Frame data
          SPINEL_DATATYPE_UINT8_S                                          // Channel
              SPINEL_DATATYPE_UINT8_S                                      // MaxCsmaBackoffs
                  SPINEL_DATATYPE_UINT8_S                                  // MaxFrameRetries
                      SPINEL_DATATYPE_BOOL_S                               // CsmaCaEnabled
                          SPINEL_DATATYPE_BOOL_S                           // IsHeaderUpdated
                              SPINEL_DATATYPE_BOOL_S                       // IsARetx
                                  SPINEL_DATATYPE_BOOL_S                   // IsSecurityProcessed
                                      SPINEL_DATATYPE_UINT32_S             // TxDelay
                                          SPINEL_DATATYPE_UINT32_S         // TxDelayBaseTime
                                              SPINEL_DATATYPE_UINT8_S      // RxChannelAfterTxDone
                                                  SPINEL_DATATYPE_INT8_S,  // TxPower
      psdu.data(), static_cast<uint32_t>(psdu.size()), channel, kMacMaxCsmaBackoffsDirect,
      max_frame_retries,
      /*csma_ca_enabled=*/true, /*is_header_updated=*/false,
      /*is_a_retx=*/false, /*is_security_processed=*/false,
      /*tx_delay=*/uint32_t{0}, /*tx_delay_base_time=*/uint32_t{0}, channel,
      static_cast<int>(OT_RADIO_POWER_INVALID));
}

std::optional<StreamRawTxDone> ParseStreamRawTxDone(const SpinelFrame& frame) {
  if (!frame.IsLastStatus()) {
    return std::nullopt;
  }
  StreamRawTxDone done;
  if (spinel_datatype_unpack(
          frame.payload().data(), static_cast<spinel_size_t>(frame.payload().size()),
          SPINEL_DATATYPE_UINT_PACKED_S SPINEL_DATATYPE_BOOL_S SPINEL_DATATYPE_BOOL_S, &done.status,
          &done.frame_pending, &done.header_updated) <= 0) {
    return std::nullopt;
  }
  return done;
}

std::optional<unsigned int> UnpackUintPacked(const std::vector<uint8_t>& payload) {
  unsigned int value = 0;
  if (spinel_datatype_unpack(payload.data(), static_cast<spinel_size_t>(payload.size()),
                             SPINEL_DATATYPE_UINT_PACKED_S, &value) <= 0) {
    return std::nullopt;
  }
  return value;
}

std::optional<uint64_t> UnpackEui64(const std::vector<uint8_t>& payload) {
  const spinel_eui64_t* eui64 = nullptr;
  if (spinel_datatype_unpack(payload.data(), static_cast<spinel_size_t>(payload.size()),
                             SPINEL_DATATYPE_EUI64_S, &eui64) <= 0 ||
      eui64 == nullptr) {
    return std::nullopt;
  }
  uint64_t value = 0;
  static_assert(sizeof(value) == sizeof(eui64->bytes));
  memcpy(&value, eui64->bytes, sizeof(value));
  return be64toh(value);
}

}  // namespace ot_rcp::testing
