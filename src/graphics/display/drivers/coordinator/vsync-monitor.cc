// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/coordinator/vsync-monitor.h"

#include <lib/driver/logging/cpp/logger.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/zx/clock.h>
#include <lib/zx/result.h>
#include <lib/zx/time.h>

#include <atomic>

#include "src/graphics/display/lib/api-types/cpp/driver-config-stamp.h"

namespace display_coordinator {

namespace {

// vsync delivery is considered to be stalled if at least this amount of time
// has elapsed since vsync was last observed.
constexpr zx::duration kVsyncStallThreshold = zx::sec(10);
constexpr zx::duration kVsyncMonitorInterval = kVsyncStallThreshold / 2;

}  // namespace

VsyncMonitor::VsyncMonitor(inspect::Node inspect_root, async_dispatcher_t* dispatcher)
    : inspect_root_(std::move(inspect_root)),
      last_vsync_ns_property_(inspect_root_.CreateUint("last_vsync_timestamp_ns", 0)),
      last_vsync_timestamp_mono_ns_property_(
          inspect_root_.CreateUint("last_vsync_timestamp_mono_ns", 0)),
      last_vsync_timestamp_approximate_boot_ns_property_(
          inspect_root_.CreateUint("last_vsync_timestamp_approximate_boot_ns", 0)),
      last_vsync_interval_ns_property_(inspect_root_.CreateUint("last_vsync_interval_ns", 0)),
      last_vsync_interval_mono_ns_property_(
          inspect_root_.CreateUint("last_vsync_interval_mono_ns", 0)),
      last_vsync_interval_boot_ns_property_(
          inspect_root_.CreateUint("last_vsync_interval_boot_ns", 0)),
      last_vsync_config_stamp_property_(inspect_root_.CreateUint(
          "last_vsync_config_stamp", display::kInvalidDriverConfigStamp.value())),
      vsync_stalls_detected_(inspect_root_.CreateUint("vsync_stalls", 0)),
      dispatcher_(*dispatcher) {
  ZX_DEBUG_ASSERT(dispatcher != nullptr);
}

VsyncMonitor::~VsyncMonitor() { Deinitialize(); }

zx::result<> VsyncMonitor::Initialize() {
  zx_status_t post_status = updater_.PostDelayed(&dispatcher_, kVsyncMonitorInterval);
  if (post_status != ZX_OK) {
    fdf::error("Failed to schedule vsync monitor: {}", zx::make_result(post_status));
    return zx::error(post_status);
  }

  return zx::ok();
}

void VsyncMonitor::Deinitialize() { updater_.Cancel(); }

void VsyncMonitor::UpdateStatistics() {
  if (vsync_stalled_) {
    return;
  }

  zx::time_monotonic now = zx::clock::get_monotonic();
  zx::duration since_last_vsync = now - last_vsync_timestamp_mono_.load();

  if (since_last_vsync > kVsyncStallThreshold) {
    vsync_stalled_ = true;
    vsync_stalls_detected_.Add(1);
  }

  zx_status_t status = updater_.PostDelayed(&dispatcher_, kVsyncMonitorInterval);
  if (status != ZX_OK) {
    fdf::error("Failed to schedule vsync monitor: {}", zx::make_result(status));
  }
}

void VsyncMonitor::OnVsync(zx::time_monotonic vsync_timestamp_mono,
                           zx::time_boot vsync_timestamp_approximate_boot,
                           display::DriverConfigStamp vsync_config_stamp) {
  last_vsync_ns_property_.Set(vsync_timestamp_mono.get());
  last_vsync_timestamp_mono_ns_property_.Set(vsync_timestamp_mono.get());
  last_vsync_timestamp_approximate_boot_ns_property_.Set(vsync_timestamp_approximate_boot.get());

  zx::duration vsync_interval_duration_mono =
      vsync_timestamp_mono - last_vsync_timestamp_mono_.load(std::memory_order_relaxed);
  last_vsync_interval_ns_property_.Set(vsync_interval_duration_mono.to_nsecs());
  last_vsync_interval_mono_ns_property_.Set(vsync_interval_duration_mono.to_nsecs());
  last_vsync_config_stamp_property_.Set(vsync_config_stamp.value());

  zx::duration vsync_interval_duration_boot =
      vsync_timestamp_approximate_boot - last_vsync_timestamp_boot_.load(std::memory_order_relaxed);
  last_vsync_interval_boot_ns_property_.Set(vsync_interval_duration_boot.to_nsecs());

  last_vsync_timestamp_mono_.store(vsync_timestamp_mono);
  last_vsync_timestamp_boot_.store(vsync_timestamp_approximate_boot);
  vsync_stalled_ = false;
}

}  // namespace display_coordinator
