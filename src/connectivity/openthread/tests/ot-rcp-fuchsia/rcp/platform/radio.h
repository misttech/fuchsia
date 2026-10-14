// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_PLATFORM_RADIO_H_
#define SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_PLATFORM_RADIO_H_

#include <lib/async/cpp/task.h>
#include <lib/async/dispatcher.h>

#include <cstdint>

#include <openthread/platform/radio.h>

namespace ot_rcp {

// Stub IEEE 802.15.4 radio for ot-rcp-fuchsia.
//
// The radio keeps a Disabled/Sleep/Receive/Transmit state machine. Transmitted frames are dropped,
// nothing is ever received, and no hardware capabilities are reported.
//
// One instance is owned by the Platform (see platform.h) and is reached through
// `Platform::Get().radio()`; the otPlatRadio* entry points in ot_radio_platform.cc forward to it.
// Destroying it cancels an in-flight transmission; the power-on configuration is restored simply
// by constructing a new one.
class Radio {
 public:
  // Settings the host changes over Spinel (PROP_PHY_TX_POWER, PROP_PHY_CCA_THRESHOLD, ...), with
  // their power-on defaults. The defaults match OpenThread's simulation platform.
  struct Config {
    otRadioState state = OT_RADIO_STATE_DISABLED;
    bool promiscuous = false;
    bool coex_enabled = false;
    int8_t tx_power = 0;         // dBm
    int8_t cca_threshold = -75;  // dBm
  };

  // Simulated radio environment.
  struct Environment {
    // Reported by otPlatRadioGetRssi(): an idle channel everywhere.
    int8_t rssi = -100;  // dBm
    // Reported through SPINEL_PROP_PHY_RX_SENSITIVITY; the host uses it as the noise floor when
    // computing link margins.
    int8_t receive_sensitivity = -100;  // dBm
  };

  explicit Radio(async_dispatcher_t* dispatcher);

  Radio(const Radio&) = delete;
  Radio& operator=(const Radio&) = delete;

  // Identity and capabilities.
  void GetIeeeEui64(uint8_t* eui64) const;  // OT_EXT_ADDRESS_SIZE bytes, MSB first.
  otRadioCaps GetCaps() const;

  // State machine.
  otRadioState GetState() const { return config_.state; }
  bool IsEnabled() const { return config_.state != OT_RADIO_STATE_DISABLED; }
  otError Enable();
  otError Disable();  // Drops an in-flight transmission without reporting completion.
  otError Sleep();
  otError Receive(uint8_t channel);

  // Transmission.
  otRadioFrame* GetTransmitBuffer() { return &tx_frame_; }
  otError Transmit(otInstance* instance, otRadioFrame* frame);

  // Environment.
  int8_t GetRssi() const { return environment_.rssi; }
  int8_t GetReceiveSensitivity() const { return environment_.receive_sensitivity; }
  otError GetCoexMetrics(otRadioCoexMetrics* metrics) const;

  // Host-controlled settings.
  bool GetPromiscuous() const { return config_.promiscuous; }
  void SetPromiscuous(bool enable) { config_.promiscuous = enable; }
  bool IsCoexEnabled() const { return config_.coex_enabled; }
  otError SetCoexEnabled(bool enabled);
  otError GetTransmitPower(int8_t* power) const;
  otError SetTransmitPower(int8_t power);
  otError GetCcaEnergyDetectThreshold(int8_t* threshold) const;
  otError SetCcaEnergyDetectThreshold(int8_t threshold);

  const Config& config() const { return config_; }
  const Environment& environment() const { return environment_; }
  void set_environment(const Environment& environment) { environment_ = environment; }

 private:
  void AbortTransmit();
  void CompleteTransmit();

  async_dispatcher_t* const dispatcher_;
  Config config_;
  Environment environment_;
  otRadioFrame tx_frame_ = {};
  uint8_t tx_psdu_[OT_RADIO_FRAME_MAX_SIZE] = {};
  // Instance to report completion to; non-null only while a transmission is in flight.
  otInstance* tx_instance_ = nullptr;
  async::TaskClosure tx_task_;
};

}  // namespace ot_rcp

#endif  // SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_PLATFORM_RADIO_H_
