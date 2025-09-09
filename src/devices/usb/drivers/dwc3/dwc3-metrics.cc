// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/dwc3/dwc3-metrics.h"

#include <zircon/syscalls.h>
#include <zircon/syscalls/clock.h>

namespace dwc3 {

void Dwc3Metrics::Init() {
  // Initialize the local stats data
  time_start_ = zx_clock_get_boot();
  for (unsigned long& event_count : event_counts_) {
    event_count = 0;
  }
}

// Lazily produce the Inspect data.
inspect::Inspector Dwc3Metrics::RecordMetrics() {
  inspect::Inspector inspector;

  inspector.GetRoot().RecordUint("time_start", time_start_);
  inspector.GetRoot().RecordUint("time_stats", zx_clock_get_boot());

  for (uint32_t i = 0; i < static_cast<uint32_t>(MetricEventType::kDevtNumEventTypes); i++) {
    MetricEventType type = static_cast<MetricEventType>(i);
    inspector.GetRoot().RecordUint(std::format("{}", type), event_counts_[i]);
  }

  return (inspector);
}

}  // namespace dwc3
