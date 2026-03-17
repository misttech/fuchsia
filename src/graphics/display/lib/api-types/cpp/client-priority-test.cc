// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/lib/api-types/cpp/client-priority.h"

#include <fidl/fuchsia.hardware.display/cpp/wire.h>

#include <gtest/gtest.h>

#if __cplusplus >= 202002L
#include <format>
#endif

namespace display {

namespace {

constexpr ClientPriority kPrimary2(fuchsia_hardware_display::wire::kPrimaryClientPriorityValue);

TEST(ClientPriorityTest, EqualityIsReflexive) {
  EXPECT_EQ(ClientPriority::kPrimary, ClientPriority::kPrimary);
  EXPECT_EQ(kPrimary2, kPrimary2);
  EXPECT_EQ(ClientPriority::kVirtcon, ClientPriority::kVirtcon);
}

TEST(ClientPriorityTest, EqualityIsSymmetric) {
  EXPECT_EQ(ClientPriority::kPrimary, kPrimary2);
  EXPECT_EQ(kPrimary2, ClientPriority::kPrimary);
}

TEST(ClientPriorityTest, EqualityForDifferentValues) {
  EXPECT_NE(ClientPriority::kPrimary, ClientPriority::kVirtcon);
  EXPECT_NE(ClientPriority::kVirtcon, ClientPriority::kPrimary);
  EXPECT_NE(kPrimary2, ClientPriority::kVirtcon);
  EXPECT_NE(ClientPriority::kVirtcon, kPrimary2);
}

TEST(ClientPriorityTest, Ordering) {
  static_assert(fuchsia_hardware_display::wire::kPrimaryClientPriorityValue >
                fuchsia_hardware_display::wire::kVirtconClientPriorityValue);

  EXPECT_GT(ClientPriority::kPrimary, ClientPriority::kVirtcon);
  EXPECT_FALSE(ClientPriority::kPrimary > ClientPriority::kPrimary);
  EXPECT_FALSE(ClientPriority::kVirtcon > ClientPriority::kPrimary);

  EXPECT_GE(ClientPriority::kPrimary, ClientPriority::kVirtcon);
  EXPECT_GE(ClientPriority::kPrimary, ClientPriority::kPrimary);
  EXPECT_FALSE(ClientPriority::kVirtcon >= ClientPriority::kPrimary);

  EXPECT_LT(ClientPriority::kVirtcon, ClientPriority::kPrimary);
  EXPECT_FALSE(ClientPriority::kVirtcon < ClientPriority::kVirtcon);
  EXPECT_FALSE(ClientPriority::kPrimary < ClientPriority::kVirtcon);

  EXPECT_LE(ClientPriority::kVirtcon, ClientPriority::kPrimary);
  EXPECT_LE(ClientPriority::kPrimary, ClientPriority::kPrimary);
  EXPECT_FALSE(ClientPriority::kPrimary <= ClientPriority::kVirtcon);
}

TEST(ClientPriorityTest, ToFidlClientPriority) {
  EXPECT_EQ(fuchsia_hardware_display::wire::kPrimaryClientPriorityValue,
            ClientPriority::kPrimary.ToFidl().value);
  EXPECT_EQ(fuchsia_hardware_display::wire::kVirtconClientPriorityValue,
            ClientPriority::kVirtcon.ToFidl().value);
}

TEST(ClientPriorityTest, ToFidlValue) {
  EXPECT_EQ(fuchsia_hardware_display::wire::kPrimaryClientPriorityValue,
            ClientPriority::kPrimary.ToFidlValue());
  EXPECT_EQ(fuchsia_hardware_display::wire::kVirtconClientPriorityValue,
            ClientPriority::kVirtcon.ToFidlValue());
}

TEST(ClientPriorityTest, ToClientPriorityWithFidlValue) {
  EXPECT_EQ(ClientPriority::kPrimary,
            ClientPriority(fuchsia_hardware_display::wire::kPrimaryClientPriorityValue));
  EXPECT_EQ(ClientPriority::kVirtcon,
            ClientPriority(fuchsia_hardware_display::wire::kVirtconClientPriorityValue));
}

TEST(ClientPriorityTest, ValueForLogging) {
  EXPECT_EQ(static_cast<uint32_t>(fuchsia_hardware_display::wire::kPrimaryClientPriorityValue),
            ClientPriority::kPrimary.ValueForLogging());
  EXPECT_EQ(static_cast<uint32_t>(fuchsia_hardware_display::wire::kVirtconClientPriorityValue),
            ClientPriority::kVirtcon.ValueForLogging());
}

TEST(ClientPriorityTest, FidlConversionRoundtrip) {
  EXPECT_EQ(ClientPriority::kPrimary, ClientPriority(ClientPriority::kPrimary.ToFidl().value));
  EXPECT_EQ(ClientPriority::kVirtcon, ClientPriority(ClientPriority::kVirtcon.ToFidl().value));

  EXPECT_EQ(ClientPriority::kPrimary, ClientPriority(ClientPriority::kPrimary.ToFidlValue()));
  EXPECT_EQ(ClientPriority::kVirtcon, ClientPriority(ClientPriority::kVirtcon.ToFidlValue()));
}

#if __cplusplus >= 202002L
TEST(ClientPriority, Format) {
  EXPECT_EQ(std::format("{}", ClientPriority::kPrimary),
            std::format("{}", fuchsia_hardware_display::wire::kPrimaryClientPriorityValue));
  EXPECT_EQ(std::format("{}", ClientPriority::kVirtcon),
            std::format("{}", fuchsia_hardware_display::wire::kVirtconClientPriorityValue));
  EXPECT_EQ(std::format("{:>2}", ClientPriority::kPrimary),
            std::format("{:>2}", fuchsia_hardware_display::wire::kPrimaryClientPriorityValue));
}
#endif  // __cplusplus >= 202002L

}  // namespace

}  // namespace display
