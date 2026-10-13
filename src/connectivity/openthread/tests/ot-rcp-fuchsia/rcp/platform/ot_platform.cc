// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// OpenThread platform entry points (otPlat*, otTasklets*) for ot-rcp-fuchsia: tasklets, alarms,
// time, entropy, heap and reset reason. These are C functions without a context pointer, so
// each one is a one-line call into the single ot_rcp::Platform. Logging is in logging.cc.

#include <lib/syslog/cpp/macros.h>
#include <zircon/syscalls.h>

#include <cstdlib>

#include <openthread/instance.h>
#include <openthread/platform/alarm-milli.h>
#include <openthread/platform/entropy.h>
#include <openthread/platform/memory.h>
#include <openthread/platform/misc.h>
#include <openthread/platform/time.h>
#include <openthread/tasklet.h>

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/platform/platform.h"

using ot_rcp::Platform;

extern "C" {

// Tasklets

void otTaskletsSignalPending(otInstance* aInstance) {
  Platform::Get().SignalTaskletPending(aInstance);
}

// Alarms

uint32_t otPlatAlarmMilliGetNow(void) { return Platform::Get().NowMillis(); }

void otPlatAlarmMilliStartAt(otInstance* aInstance, uint32_t aT0, uint32_t aDt) {
  Platform::Get().StartMilliAlarm(aInstance, aT0, aDt);
}

void otPlatAlarmMilliStop(otInstance* aInstance) { Platform::Get().StopMilliAlarm(); }

// Time

uint64_t otPlatTimeGet(void) { return Platform::Get().NowMicros(); }

// Entropy

otError otPlatEntropyGet(uint8_t* aOutput, uint16_t aOutputLength) {
  if (aOutput == nullptr) {
    return OT_ERROR_INVALID_ARGS;
  }
  zx_cprng_draw(aOutput, aOutputLength);
  return OT_ERROR_NONE;
}

// Heap

void* otPlatCAlloc(size_t aNum, size_t aSize) { return calloc(aNum, aSize); }

void otPlatFree(void* aPtr) { free(aPtr); }

// Misc

otPlatResetReason otPlatGetResetReason(otInstance* aInstance) {
  return Platform::Get().reset_reason();
}

void otPlatAssertFail(const char* aFilename, int aLineNumber) {
  FX_LOGS(FATAL) << "OpenThread assertion failed at " << aFilename << ":" << aLineNumber;
}

}  // extern "C"
