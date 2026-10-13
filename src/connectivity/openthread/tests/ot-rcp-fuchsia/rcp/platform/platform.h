// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_PLATFORM_PLATFORM_H_
#define SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_PLATFORM_PLATFORM_H_

#include <lib/async/cpp/task.h>
#include <lib/async/dispatcher.h>

#include <cstdint>
#include <optional>
#include <string>

#include <openthread/instance.h>
#include <openthread/platform/misc.h>

namespace ot_rcp {
// OpenThread platform state for the ot-rcp-fuchsia RCP: tasklet scheduling, the millisecond
// alarm, time, and the per-node identity (node id, log tag, reset reason).
//
// There is at most one Platform at a time, reachable through `Get()`. Not thread-safe and
// should be called on the same dispatcher.
class Platform {
 public:
  static void Create(async_dispatcher_t* dispatcher, uint32_t node_id);
  static void Destroy();
  static Platform& Get();

  Platform(async_dispatcher_t* dispatcher, uint32_t node_id);
  Platform(const Platform&) = delete;
  Platform& operator=(const Platform&) = delete;

  uint32_t node_id() const { return node_id_; }
  const std::string& log_tag() const { return log_tag_; }
  otPlatResetReason reset_reason() const { return reset_reason_; }
  void set_reset_reason(otPlatResetReason reason) { reset_reason_ = reason; }

  // Backing for the `otPlat*` entry points.
  void SignalTaskletPending(otInstance* instance);
  void StartMilliAlarm(otInstance* instance, uint32_t t0, uint32_t dt);
  void StopMilliAlarm();
  uint32_t NowMillis() const;
  uint64_t NowMicros() const;

 private:
  // Single instance. Heap-allocated and never freed so that no destructor runs at process exit.
  static std::optional<Platform>& Instance();

  static bool IsLive(otInstance* instance);

  int64_t NowNanos() const;

  async_dispatcher_t* const dispatcher_;
  const uint32_t node_id_;
  const std::string log_tag_ = "rcp-" + std::to_string(node_id_);
  otPlatResetReason reset_reason_ = OT_PLAT_RESET_REASON_POWER_ON;

  otInstance* tasklet_instance_ = nullptr;
  async::TaskClosure tasklet_task_;

  otInstance* milli_instance_ = nullptr;
  async::TaskClosure milli_task_;
};

}  // namespace ot_rcp

#endif  // SRC_CONNECTIVITY_OPENTHREAD_TESTS_OT_RCP_FUCHSIA_RCP_PLATFORM_PLATFORM_H_
