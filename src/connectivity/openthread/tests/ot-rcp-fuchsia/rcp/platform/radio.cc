// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/platform/radio.h"

#include <endian.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/status.h>

#include <cstring>

#include <openthread/platform/radio.h>

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/platform/platform.h"

namespace ot_rcp {
namespace {

// IEEE 802.15.4-2015 §7.2.1.7: Ack Request bit, bit 5 of the first frame control octet.
constexpr uint8_t kFcfAckRequest = 1 << 5;

}  // namespace

Radio::Radio(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher) {
  tx_frame_.mPsdu = tx_psdu_;
  tx_task_.set_handler([this] { CompleteTransmit(); });
}

void Radio::GetIeeeEui64(uint8_t* eui64) const {
  // The EUI-64 is laid out most significant byte first.
  const uint64_t eui64_be = htobe64(Platform::Get().eui64());
  static_assert(sizeof(eui64_be) == OT_EXT_ADDRESS_SIZE);
  memcpy(eui64, &eui64_be, sizeof(eui64_be));
}

otRadioCaps Radio::GetCaps() const { return OT_RADIO_CAPS_NONE; }

otError Radio::Enable() {
  if (config_.state == OT_RADIO_STATE_DISABLED) {
    config_.state = OT_RADIO_STATE_SLEEP;
  }
  return OT_ERROR_NONE;
}

otError Radio::Disable() {
  AbortTransmit();
  config_.state = OT_RADIO_STATE_DISABLED;
  return OT_ERROR_NONE;
}

otError Radio::Sleep() {
  if (config_.state == OT_RADIO_STATE_DISABLED) {
    return OT_ERROR_INVALID_STATE;
  }
  if (config_.state == OT_RADIO_STATE_TRANSMIT) {
    return OT_ERROR_BUSY;
  }
  config_.state = OT_RADIO_STATE_SLEEP;
  return OT_ERROR_NONE;
}

otError Radio::Receive(uint8_t channel) {
  OT_UNUSED_VARIABLE(channel);
  if (config_.state == OT_RADIO_STATE_DISABLED || config_.state == OT_RADIO_STATE_TRANSMIT) {
    return OT_ERROR_INVALID_STATE;
  }
  config_.state = OT_RADIO_STATE_RECEIVE;
  return OT_ERROR_NONE;
}

otError Radio::Transmit(otInstance* instance, otRadioFrame* frame) {
  if (config_.state != OT_RADIO_STATE_RECEIVE || frame != &tx_frame_) {
    return OT_ERROR_INVALID_STATE;
  }
  zx_status_t status = tx_task_.Post(dispatcher_);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to post radio transmit task: " << zx_status_get_string(status);
    return OT_ERROR_FAILED;
  }
  config_.state = OT_RADIO_STATE_TRANSMIT;
  tx_instance_ = instance;
  return OT_ERROR_NONE;
}

otError Radio::GetCoexMetrics(otRadioCoexMetrics* metrics) const {
  if (metrics == nullptr) {
    return OT_ERROR_INVALID_ARGS;
  }
  memset(metrics, 0, sizeof(*metrics));
  return OT_ERROR_NONE;
}

otError Radio::SetCoexEnabled(bool enabled) {
  config_.coex_enabled = enabled;
  return OT_ERROR_NONE;
}

otError Radio::GetTransmitPower(int8_t* power) const {
  if (power == nullptr) {
    return OT_ERROR_INVALID_ARGS;
  }
  *power = config_.tx_power;
  return OT_ERROR_NONE;
}

otError Radio::SetTransmitPower(int8_t power) {
  config_.tx_power = power;
  return OT_ERROR_NONE;
}

otError Radio::GetCcaEnergyDetectThreshold(int8_t* threshold) const {
  if (threshold == nullptr) {
    return OT_ERROR_INVALID_ARGS;
  }
  *threshold = config_.cca_threshold;
  return OT_ERROR_NONE;
}

otError Radio::SetCcaEnergyDetectThreshold(int8_t threshold) {
  config_.cca_threshold = threshold;
  return OT_ERROR_NONE;
}

void Radio::AbortTransmit() {
  tx_task_.Cancel();
  tx_instance_ = nullptr;
}

void Radio::CompleteTransmit() {
  otInstance* instance = tx_instance_;
  otRadioFrame* frame = &tx_frame_;
  if (instance == nullptr || config_.state != OT_RADIO_STATE_TRANSMIT) {
    return;
  }

  otPlatRadioTxStarted(instance, frame);

  FX_LOGS(DEBUG) << "null radio: dropping " << frame->mLength << " byte frame on channel "
                 << static_cast<int>(frame->mChannel) << " for now.";
  const bool ack_requested = frame->mLength > 0 && (frame->mPsdu[0] & kFcfAckRequest) != 0;
  // Move to Receive before reporting completion: OpenThread may start another transmission from
  // within `otPlatRadioTxDone()`.
  config_.state = OT_RADIO_STATE_RECEIVE;
  tx_instance_ = nullptr;
  otPlatRadioTxDone(instance, frame, nullptr, ack_requested ? OT_ERROR_NO_ACK : OT_ERROR_NONE);
}

}  // namespace ot_rcp
