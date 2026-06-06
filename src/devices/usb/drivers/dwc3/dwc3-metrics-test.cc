// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/dwc3/dwc3-metrics.h"

#include <lib/inspect/cpp/hierarchy.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/inspect/cpp/reader.h>

#include <gtest/gtest.h>

namespace dwc3 {

TEST(Dwc3MetricsTest, InitResetsAndRecordsCorrectly) {
  Dwc3Metrics metrics;
  metrics.Init();

  // Increment some events
  metrics.IncrementEventCount(static_cast<uint32_t>(MetricEventType::kDevtDisconnect));
  metrics.IncrementEventCount(static_cast<uint32_t>(MetricEventType::kDevtDisconnect));
  metrics.IncrementEventCount(static_cast<uint32_t>(MetricEventType::kDevtUsbReset));

  // Call RecordMetrics (without mmio and dwc3 pointers)
  inspect::Inspector inspector = metrics.RecordMetrics(nullptr, nullptr);

  // Read hierarchy
  auto hierarchy = inspect::ReadFromVmo(inspector.DuplicateVmo()).take_value();

  // 1. Check root level properties
  const auto* time_start = hierarchy.node().get_property<inspect::UintPropertyValue>("time_start");
  ASSERT_NE(nullptr, time_start);
  EXPECT_GT(time_start->value(), 0u);

  const auto* time_stats = hierarchy.node().get_property<inspect::UintPropertyValue>("time_stats");
  ASSERT_NE(nullptr, time_stats);
  EXPECT_GE(time_stats->value(), time_start->value());

  // 2. Check event_counts node
  const auto* event_counts = hierarchy.GetByPath({"event_counts"});
  ASSERT_NE(nullptr, event_counts);

  // Verify disconnect count is 2
  const auto* disconnect =
      event_counts->node().get_property<inspect::UintPropertyValue>("DEVT_DISCONNECT");
  ASSERT_NE(nullptr, disconnect);
  EXPECT_EQ(2u, disconnect->value());

  // Verify usb reset count is 1
  const auto* usb_reset =
      event_counts->node().get_property<inspect::UintPropertyValue>("DEVT_USB_RESET");
  ASSERT_NE(nullptr, usb_reset);
  EXPECT_EQ(1u, usb_reset->value());

  // Verify other event types are initialized to 0
  const auto* sof = event_counts->node().get_property<inspect::UintPropertyValue>("DEVT_SOF");
  ASSERT_NE(nullptr, sof);
  EXPECT_EQ(0u, sof->value());
}

}  // namespace dwc3
