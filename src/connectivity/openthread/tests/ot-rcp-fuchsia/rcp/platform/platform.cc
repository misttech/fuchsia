// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/connectivity/openthread/tests/ot-rcp-fuchsia/rcp/platform/platform.h"

#include <lib/async/cpp/time.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/assert.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>

#include <algorithm>
#include <optional>

#include <openthread/platform/alarm-milli.h>
#include <openthread/tasklet.h>

namespace ot_rcp {

void Platform::Create(async_dispatcher_t* dispatcher, uint32_t node_id) {
  Instance().emplace(dispatcher, node_id);
}

void Platform::Destroy() { Instance().reset(); }

Platform& Platform::Get() {
  std::optional<Platform>& instance = Instance();
  ZX_ASSERT_MSG(instance.has_value(), "Platform::Create() has not been called");
  return *instance;
}

std::optional<Platform>& Platform::Instance() {
  static std::optional<Platform>& instance = *new std::optional<Platform>();
  return instance;
}

Platform::Platform(async_dispatcher_t* dispatcher, uint32_t node_id)
    : dispatcher_(dispatcher), node_id_(node_id) {
  tasklet_task_.set_handler([this]() {
    if (IsLive(tasklet_instance_)) {
      otTaskletsProcess(tasklet_instance_);
    }
  });
  milli_task_.set_handler([this]() {
    if (IsLive(milli_instance_)) {
      otPlatAlarmMilliFired(milli_instance_);
    }
  });
}

void Platform::SignalTaskletPending(otInstance* instance) {
  // A single otInstance should be used between Create() and Destroy().
  ZX_ASSERT_MSG(tasklet_instance_ == nullptr || tasklet_instance_ == instance,
                "otInstance changed without Platform::Destroy()");
  tasklet_instance_ = instance;
  if (tasklet_task_.is_pending()) {
    return;
  }
  zx_status_t status = tasklet_task_.Post(dispatcher_);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to post tasklet task: " << zx_status_get_string(status);
  }
}

void Platform::StartMilliAlarm(otInstance* instance, uint32_t t0, uint32_t dt) {
  ZX_ASSERT_MSG(milli_instance_ == nullptr || milli_instance_ == instance,
                "otInstance changed without Platform::Destroy()");
  milli_instance_ = instance;
  milli_task_.Cancel();

  const int64_t now_ns = NowNanos();
  const uint32_t now_ms = static_cast<uint32_t>(now_ns / ZX_MSEC(1));
  const int32_t remaining_ms = std::max<int32_t>(static_cast<int32_t>(t0 + dt - now_ms), 0);
  const zx::time deadline(now_ns + ZX_MSEC(remaining_ms));
  zx_status_t status = milli_task_.PostForTime(dispatcher_, deadline);
  if (status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to post alarm task: " << zx_status_get_string(status);
  }
}

void Platform::StopMilliAlarm() {
  milli_task_.Cancel();
  milli_instance_ = nullptr;
}

uint32_t Platform::NowMillis() const { return static_cast<uint32_t>(NowNanos() / ZX_MSEC(1)); }

uint64_t Platform::NowMicros() const { return static_cast<uint64_t>(NowNanos() / ZX_USEC(1)); }

bool Platform::IsLive(otInstance* instance) {
  return instance != nullptr && otInstanceIsInitialized(instance);
}

int64_t Platform::NowNanos() const { return async::Now(dispatcher_).get(); }

}  // namespace ot_rcp
