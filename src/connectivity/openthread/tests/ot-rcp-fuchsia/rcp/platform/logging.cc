// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// OpenThread logging support.

#include <lib/syslog/cpp/macros.h>

#include <cstdarg>
#include <cstdio>
#include <string>

#include <openthread/platform/logging.h>

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/platform/platform.h"

namespace {

constexpr size_t kLogMaxSize = 512;

}  // namespace

extern "C" void otPlatLog(otLogLevel aLogLevel, otLogRegion aLogRegion, const char* aFormat, ...) {
  OT_UNUSED_VARIABLE(aLogRegion);
  char buffer[kLogMaxSize];
  va_list args;
  va_start(args, aFormat);
  vsnprintf(buffer, sizeof(buffer), aFormat, args);
  va_end(args);

  const char* tag = ot_rcp::Platform::Get().log_tag().c_str();
  switch (aLogLevel) {
    case OT_LOG_LEVEL_CRIT:
      FX_LOGST(ERROR, tag) << buffer;
      break;
    case OT_LOG_LEVEL_WARN:
      FX_LOGST(WARNING, tag) << buffer;
      break;
    case OT_LOG_LEVEL_NOTE:
    case OT_LOG_LEVEL_INFO:
      FX_LOGST(INFO, tag) << buffer;
      break;
    case OT_LOG_LEVEL_DEBG:
    default:
      FX_LOGST(DEBUG, tag) << buffer;
      break;
  }
}
