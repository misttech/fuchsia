// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_TESTS_MOCK_SPINEL_HOST_MOCK_SPINEL_HOST_FIXTURE_H_
#define SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_TESTS_MOCK_SPINEL_HOST_MOCK_SPINEL_HOST_FIXTURE_H_

#include <fidl/fuchsia.lowpan.spinel/cpp/fidl.h>
#include <lib/fidl/cpp/client.h>
#include <lib/fit/function.h>
#include <lib/zx/time.h>
#include <zircon/types.h>

#include <cstdint>
#include <optional>
#include <vector>

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/tests/mock-spinel-host/mock_spinel_host.h"
#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/tests/mock-spinel-host/spinel_frame.h"
#include "src/lib/testing/loop_fixture/real_loop_fixture.h"

namespace ot_rcp::testing {

// gtest fixture that drives a MockSpinelHost: Open and sync on the reset notification, GET/SET
// properties with TID matching, bring the raw link up. Each helper pumps the fixture's loop
// until the RCP answers.
class MockSpinelHostFixture : public gtest::RealLoopFixture {
 protected:
  // Waiting for an expected event is unbounded: a stalled RCP is caught by the test harness's
  // per-test timeout, and a short deadline here would only add flakes on slow CQ bots.
  static constexpr zx::duration kTimeout = zx::duration::infinite();
  // How long to wait when asserting that something does NOT happen. It bounds how long a negative
  // check holds the test.
  static constexpr zx::duration kQuietPeriod = zx::msec(750);

  zx_status_t SetChannel(fidl::Client<fspinel::DeviceSetup>& setup, MockSpinelHost& host);

  // Opens the device, then runs the loop until idle so that events sent before the `Open` reply
  // (e.g. the initial `OnReadyForSendFrames` grant) have been handled when this returns.
  void OpenDevice(MockSpinelHost& host);
  void CloseDevice(MockSpinelHost& host);

  std::optional<uint32_t> GetMaxFrameSize(MockSpinelHost& host);

  std::optional<SpinelFrame> WaitForFrame(MockSpinelHost& host,
                                          fit::function<bool(const SpinelFrame&)> pred,
                                          zx::duration timeout = kTimeout);

  // Waits for the frame answering transaction `tid`.
  std::optional<SpinelFrame> WaitForResponse(MockSpinelHost& host, uint8_t tid,
                                             zx::duration timeout = kTimeout);

  std::optional<SpinelFrame> WaitForResetNotification(MockSpinelHost& host);

  // Required after `OpenDevice()`: grants the inbound window, turns on auto-replenish and
  // asserts the unsolicited `LAST_STATUS(expected_reset_status)` that follows a reset.
  void GrantInboundWindowAndWaitForRcpToBeReady(MockSpinelHost& host,
                                                spinel_status_t expected_reset_status);

  // Waits for radio-bound credit, then sends `data`.
  void SendWhenReady(MockSpinelHost& host, std::vector<uint8_t> data);

  // Sends one Spinel request and waits for its response.
  std::optional<SpinelFrame> Transact(MockSpinelHost& host, unsigned int command,
                                      std::optional<unsigned int> prop,
                                      const std::vector<uint8_t>& value = {},
                                      zx::duration timeout = kTimeout);

  // GET `prop`; asserts the response is PROP_VALUE_IS(prop) and returns its payload.
  std::optional<std::vector<uint8_t>> Get(MockSpinelHost& host, unsigned int prop);

  // SET `prop`; expects `PROP_VALUE_IS(prop)` (or `LAST_STATUS(OK)`). Returns the response payload.
  std::optional<std::vector<uint8_t>> Set(MockSpinelHost& host, unsigned int prop,
                                          const std::vector<uint8_t>& value);

  // Sends `NOOP` and expects `LAST_STATUS(OK)` for the same TID: proves the RCP is responsive.
  void ExpectResponsive(MockSpinelHost& host);

  // GET `prop` which has a variable-length unsigned integer.
  std::optional<unsigned int> GetUintPacked(MockSpinelHost& host, unsigned int prop);

  // Brings the raw link up the way RadioSpinel::Enable + Receive do, so `STREAM_RAW` is accepted.
  void EnableRawLink(MockSpinelHost& host, uint16_t pan_id, uint16_t short_address,
                     uint8_t channel);

  // Decodes the TX completion of a `STREAM_RAW` set and returns its status.
  std::optional<unsigned int> StreamRawTxDoneStatus(const std::optional<SpinelFrame>& response);

  // Asserts `frame` is a `LAST_STATUS(expected_status)`.
  static void ExpectLastStatus(const std::optional<SpinelFrame>& frame,
                               spinel_status_t expected_status);

  // Asserts `reset` is the unsolicited `LAST_STATUS(RESET_*)` carrying `expected_status`.
  static void ExpectResetNotification(const std::optional<SpinelFrame>& reset,
                                      spinel_status_t expected_status);

  // Asserts the transport is in its normal state: no `OnError` events and still bound.
  static void ExpectHealthy(const MockSpinelHost& host);
};

}  // namespace ot_rcp::testing

#endif  // SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_TESTS_MOCK_SPINEL_HOST_MOCK_SPINEL_HOST_FIXTURE_H_
