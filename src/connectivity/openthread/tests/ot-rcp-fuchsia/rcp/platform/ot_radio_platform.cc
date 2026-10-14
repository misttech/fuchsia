// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// OpenThread radio platform entry points (otPlatRadio*) for ot-rcp-fuchsia. These are C functions
// without a context pointer, so each one is a one-line call into the Radio owned by the single
// ot_rcp::Platform (see radio.h for the behaviour).

#include <openthread/platform/radio.h>

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/platform/platform.h"

using ot_rcp::Platform;

extern "C" {

void otPlatRadioGetIeeeEui64(otInstance* aInstance, uint8_t* aIeeeEui64) {
  OT_UNUSED_VARIABLE(aInstance);
  Platform::Get().radio().GetIeeeEui64(aIeeeEui64);
}

otRadioCaps otPlatRadioGetCaps(otInstance* aInstance) {
  OT_UNUSED_VARIABLE(aInstance);
  return Platform::Get().radio().GetCaps();
}

int8_t otPlatRadioGetReceiveSensitivity(otInstance* aInstance) {
  OT_UNUSED_VARIABLE(aInstance);
  return Platform::Get().radio().GetReceiveSensitivity();
}

otRadioState otPlatRadioGetState(otInstance* aInstance) {
  OT_UNUSED_VARIABLE(aInstance);
  return Platform::Get().radio().GetState();
}

otError otPlatRadioEnable(otInstance* aInstance) {
  OT_UNUSED_VARIABLE(aInstance);
  return Platform::Get().radio().Enable();
}

otError otPlatRadioDisable(otInstance* aInstance) {
  OT_UNUSED_VARIABLE(aInstance);
  return Platform::Get().radio().Disable();
}

bool otPlatRadioIsEnabled(otInstance* aInstance) {
  OT_UNUSED_VARIABLE(aInstance);
  return Platform::Get().radio().IsEnabled();
}

otError otPlatRadioSleep(otInstance* aInstance) {
  OT_UNUSED_VARIABLE(aInstance);
  return Platform::Get().radio().Sleep();
}

otError otPlatRadioReceive(otInstance* aInstance, uint8_t aChannel) {
  OT_UNUSED_VARIABLE(aInstance);
  return Platform::Get().radio().Receive(aChannel);
}

otRadioFrame* otPlatRadioGetTransmitBuffer(otInstance* aInstance) {
  OT_UNUSED_VARIABLE(aInstance);
  return Platform::Get().radio().GetTransmitBuffer();
}

otError otPlatRadioTransmit(otInstance* aInstance, otRadioFrame* aFrame) {
  return Platform::Get().radio().Transmit(aInstance, aFrame);
}

int8_t otPlatRadioGetRssi(otInstance* aInstance) {
  OT_UNUSED_VARIABLE(aInstance);
  return Platform::Get().radio().GetRssi();
}

otError otPlatRadioEnergyScan(otInstance* aInstance, uint8_t aScanChannel, uint16_t aScanDuration) {
  OT_UNUSED_VARIABLE(aInstance);
  OT_UNUSED_VARIABLE(aScanChannel);
  OT_UNUSED_VARIABLE(aScanDuration);
  return OT_ERROR_NOT_IMPLEMENTED;
}

void otPlatRadioSetPanId(otInstance* aInstance, otPanId aPanId) {
  OT_UNUSED_VARIABLE(aInstance);
  OT_UNUSED_VARIABLE(aPanId);
}

void otPlatRadioSetExtendedAddress(otInstance* aInstance, const otExtAddress* aExtAddress) {
  OT_UNUSED_VARIABLE(aInstance);
  OT_UNUSED_VARIABLE(aExtAddress);
}

void otPlatRadioSetShortAddress(otInstance* aInstance, otShortAddress aShortAddress) {
  OT_UNUSED_VARIABLE(aInstance);
  OT_UNUSED_VARIABLE(aShortAddress);
}

bool otPlatRadioGetPromiscuous(otInstance* aInstance) {
  OT_UNUSED_VARIABLE(aInstance);
  return Platform::Get().radio().GetPromiscuous();
}

void otPlatRadioSetPromiscuous(otInstance* aInstance, bool aEnable) {
  OT_UNUSED_VARIABLE(aInstance);
  Platform::Get().radio().SetPromiscuous(aEnable);
}

otError otPlatRadioGetTransmitPower(otInstance* aInstance, int8_t* aPower) {
  OT_UNUSED_VARIABLE(aInstance);
  return Platform::Get().radio().GetTransmitPower(aPower);
}

otError otPlatRadioSetTransmitPower(otInstance* aInstance, int8_t aPower) {
  OT_UNUSED_VARIABLE(aInstance);
  return Platform::Get().radio().SetTransmitPower(aPower);
}

otError otPlatRadioGetCcaEnergyDetectThreshold(otInstance* aInstance, int8_t* aThreshold) {
  OT_UNUSED_VARIABLE(aInstance);
  return Platform::Get().radio().GetCcaEnergyDetectThreshold(aThreshold);
}

otError otPlatRadioSetCcaEnergyDetectThreshold(otInstance* aInstance, int8_t aThreshold) {
  OT_UNUSED_VARIABLE(aInstance);
  return Platform::Get().radio().SetCcaEnergyDetectThreshold(aThreshold);
}

void otPlatRadioEnableSrcMatch(otInstance* aInstance, bool aEnable) {
  OT_UNUSED_VARIABLE(aInstance);
  OT_UNUSED_VARIABLE(aEnable);
}

otError otPlatRadioAddSrcMatchShortEntry(otInstance* aInstance, otShortAddress aShortAddress) {
  OT_UNUSED_VARIABLE(aInstance);
  OT_UNUSED_VARIABLE(aShortAddress);
  return OT_ERROR_NONE;
}

otError otPlatRadioAddSrcMatchExtEntry(otInstance* aInstance, const otExtAddress* aExtAddress) {
  OT_UNUSED_VARIABLE(aInstance);
  OT_UNUSED_VARIABLE(aExtAddress);
  return OT_ERROR_NONE;
}

otError otPlatRadioClearSrcMatchShortEntry(otInstance* aInstance, otShortAddress aShortAddress) {
  OT_UNUSED_VARIABLE(aInstance);
  OT_UNUSED_VARIABLE(aShortAddress);
  return OT_ERROR_NONE;
}

otError otPlatRadioClearSrcMatchExtEntry(otInstance* aInstance, const otExtAddress* aExtAddress) {
  OT_UNUSED_VARIABLE(aInstance);
  OT_UNUSED_VARIABLE(aExtAddress);
  return OT_ERROR_NONE;
}

void otPlatRadioClearSrcMatchShortEntries(otInstance* aInstance) { OT_UNUSED_VARIABLE(aInstance); }

void otPlatRadioClearSrcMatchExtEntries(otInstance* aInstance) { OT_UNUSED_VARIABLE(aInstance); }

otError otPlatRadioSetCoexEnabled(otInstance* aInstance, bool aEnabled) {
  OT_UNUSED_VARIABLE(aInstance);
  return Platform::Get().radio().SetCoexEnabled(aEnabled);
}

bool otPlatRadioIsCoexEnabled(otInstance* aInstance) {
  OT_UNUSED_VARIABLE(aInstance);
  return Platform::Get().radio().IsCoexEnabled();
}

otError otPlatRadioGetCoexMetrics(otInstance* aInstance, otRadioCoexMetrics* aCoexMetrics) {
  OT_UNUSED_VARIABLE(aInstance);
  return Platform::Get().radio().GetCoexMetrics(aCoexMetrics);
}

otError otPlatRadioConfigureEnhAckProbing(otInstance* aInstance, otLinkMetrics aLinkMetrics,
                                          otShortAddress aShortAddress,
                                          const otExtAddress* aExtAddress) {
  OT_UNUSED_VARIABLE(aInstance);
  OT_UNUSED_VARIABLE(aLinkMetrics);
  OT_UNUSED_VARIABLE(aShortAddress);
  OT_UNUSED_VARIABLE(aExtAddress);
  return OT_ERROR_NONE;
}

}  // extern "C"
