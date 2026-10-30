// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <stdio.h>

#include <dev/arm_smmu/smmu_mode.h>
#include <zxtest/zxtest.h>

#include "cross_test_ffi.h"

namespace arm_smmu {
namespace {
constexpr ArmSmmuMode kInvalidArmSmmuMode = static_cast<ArmSmmuMode>(0xbaadf00d);

ktl::optional<ArmSmmuMode> GetModeForBackend(BackendKind kind, const char* mode_str,
                                             ktl::optional<uint64_t> base_addr = ktl::nullopt) {
  if (kind == BackendKind::kRust) {
    uint32_t raw_mode = 0;
    if (!rust_smmu_get_mode(mode_str, base_addr.has_value(), base_addr.value_or(0), &raw_mode)) {
      return ktl::nullopt;
    }
    return static_cast<ArmSmmuMode>(raw_mode);
  }
  return GetSmmuMode(mode_str, base_addr);
}

bool ValidateModeForBackend(BackendKind kind, const char* mode_str) {
  if (kind == BackendKind::kRust) {
    return rust_smmu_validate_mode_string(mode_str);
  }
  return ValidateSmmuModeString(mode_str);
}

TEST(SmmuModeStringTest, Defaults) {
  for (BackendKind kind : {BackendKind::kCpp, BackendKind::kRust}) {
    printf("[Backend: %s] SmmuModeStringTest.Defaults\n", BackendTag(kind));
    fflush(stdout);

    // Test basic token parsing for the default mode.
    EXPECT_EQ(ArmSmmuMode::kDisabled,
              GetModeForBackend(kind, "disabled").value_or(kInvalidArmSmmuMode));
    EXPECT_EQ(ArmSmmuMode::kPassthru,
              GetModeForBackend(kind, "passthru").value_or(kInvalidArmSmmuMode));
    EXPECT_EQ(ArmSmmuMode::kEnforced,
              GetModeForBackend(kind, "enforced").value_or(kInvalidArmSmmuMode));

    // Test some invalid strings.  We expect anything invalid to come back as ktl::nullopt.
    EXPECT_FALSE(GetModeForBackend(kind, "").has_value());
    EXPECT_FALSE(GetModeForBackend(kind, "pass thru").has_value());
    EXPECT_FALSE(GetModeForBackend(kind, "pass-thru").has_value());
    EXPECT_FALSE(GetModeForBackend(kind, "asdfjkldsf").has_value());
  }
}

TEST(SmmuModeStringTest, Advanced) {
  for (BackendKind kind : {BackendKind::kCpp, BackendKind::kRust}) {
    printf("[Backend: %s] SmmuModeStringTest.Advanced\n", BackendTag(kind));
    fflush(stdout);

    // Test base_addr matching.
    EXPECT_EQ(
        ArmSmmuMode::kPassthru,
        GetModeForBackend(kind, "disabled,0x1234=passthru", 0x1234).value_or(kInvalidArmSmmuMode));
    EXPECT_EQ(ArmSmmuMode::kDisabled, GetModeForBackend(kind, "disabled,0x1234=passthru", 0x5678)
                                          .value_or(kInvalidArmSmmuMode));  // Fallback to default

    // Test multiple entries.
    EXPECT_EQ(ArmSmmuMode::kEnforced,
              GetModeForBackend(kind, "disabled,0x1234=passthru,0x5678=enforced", 0x5678)
                  .value_or(kInvalidArmSmmuMode));
    EXPECT_EQ(ArmSmmuMode::kPassthru,
              GetModeForBackend(kind, "disabled,0x1234=passthru,0x5678=enforced", 0x1234)
                  .value_or(kInvalidArmSmmuMode));

    // Test first match wins.
    EXPECT_EQ(ArmSmmuMode::kPassthru,
              GetModeForBackend(kind, "disabled,0x1234=passthru,0x1234=enforced", 0x1234)
                  .value_or(kInvalidArmSmmuMode));

    // Test invalid entries.
    EXPECT_EQ(
        ArmSmmuMode::kDisabled,
        GetModeForBackend(kind, "disabled,0x1234passthru", 0x1234).value_or(kInvalidArmSmmuMode));
    EXPECT_FALSE(GetModeForBackend(kind, "disabled,0x1234=invalid", 0x1234).has_value());

    // Test hex vs decimal parsing.
    EXPECT_EQ(
        ArmSmmuMode::kPassthru,
        GetModeForBackend(kind, "disabled,4660=passthru", 0x1234).value_or(kInvalidArmSmmuMode));

    // Test skipping bad address segments to find a match.
    EXPECT_EQ(ArmSmmuMode::kPassthru,
              GetModeForBackend(kind, "disabled,invalid_segment,0x1234=passthru", 0x1234)
                  .value_or(kInvalidArmSmmuMode));
    EXPECT_EQ(ArmSmmuMode::kEnforced,
              GetModeForBackend(kind, "disabled,0x1234passthru,0x5678=enforced", 0x5678)
                  .value_or(kInvalidArmSmmuMode));

    // Test empty specific mode token.
    EXPECT_FALSE(GetModeForBackend(kind, "disabled,0x1234=", 0x1234).has_value());
  }
}

TEST(SmmuModeStringTest, CaseInsensitive) {
  for (BackendKind kind : {BackendKind::kCpp, BackendKind::kRust}) {
    printf("[Backend: %s] SmmuModeStringTest.CaseInsensitive\n", BackendTag(kind));
    fflush(stdout);

    // Test mixed case for default mode.
    EXPECT_EQ(ArmSmmuMode::kDisabled,
              GetModeForBackend(kind, "DiSaBlEd").value_or(kInvalidArmSmmuMode));
    EXPECT_EQ(ArmSmmuMode::kPassthru,
              GetModeForBackend(kind, "PASSTHRU").value_or(kInvalidArmSmmuMode));
    EXPECT_EQ(ArmSmmuMode::kEnforced,
              GetModeForBackend(kind, "EnFoRcEd").value_or(kInvalidArmSmmuMode));

    // Test mixed case in specific entries.
    EXPECT_EQ(
        ArmSmmuMode::kPassthru,
        GetModeForBackend(kind, "disabled,0x1234=PaSsThRu", 0x1234).value_or(kInvalidArmSmmuMode));
    EXPECT_EQ(
        ArmSmmuMode::kEnforced,
        GetModeForBackend(kind, "disabled,0x1234=ENFORCED", 0x1234).value_or(kInvalidArmSmmuMode));
  }
}

TEST(SmmuModeStringTest, Whitespace) {
  for (BackendKind kind : {BackendKind::kCpp, BackendKind::kRust}) {
    printf("[Backend: %s] SmmuModeStringTest.Whitespace\n", BackendTag(kind));
    fflush(stdout);

    // Test leading/trailing whitespace for default mode.
    EXPECT_EQ(ArmSmmuMode::kPassthru,
              GetModeForBackend(kind, "  passthru  ").value_or(kInvalidArmSmmuMode));
    EXPECT_EQ(ArmSmmuMode::kDisabled,
              GetModeForBackend(kind, " disabled ").value_or(kInvalidArmSmmuMode));

    // Test whitespace around delimiters in specific entries.
    EXPECT_EQ(ArmSmmuMode::kPassthru,
              GetModeForBackend(kind, "disabled , 0x1234 = passthru", 0x1234)
                  .value_or(kInvalidArmSmmuMode));
    EXPECT_EQ(ArmSmmuMode::kEnforced,
              GetModeForBackend(kind, "disabled, 0x1234 = enforced ", 0x1234)
                  .value_or(kInvalidArmSmmuMode));
    EXPECT_EQ(ArmSmmuMode::kPassthru, GetModeForBackend(kind, "disabled ,0x1234= passthru", 0x1234)
                                          .value_or(kInvalidArmSmmuMode));

    // Test mixed tabs and spaces.
    EXPECT_EQ(ArmSmmuMode::kPassthru,
              GetModeForBackend(kind, " \t disabled \t , \t 0x1234 \t = \t passthru \t ", 0x1234)
                  .value_or(kInvalidArmSmmuMode));

    // Test whitespace in multiple entries.
    EXPECT_EQ(ArmSmmuMode::kEnforced,
              GetModeForBackend(kind, "disabled, 0x1234 = passthru , 0x5678 = enforced", 0x5678)
                  .value_or(kInvalidArmSmmuMode));
    EXPECT_EQ(ArmSmmuMode::kPassthru,
              GetModeForBackend(kind, "disabled, 0x1234 = passthru , 0x5678 = enforced", 0x1234)
                  .value_or(kInvalidArmSmmuMode));

    // Test whitespace with Fallback.
    EXPECT_EQ(ArmSmmuMode::kPassthru,
              GetModeForBackend(kind, " passthru , 0x1234 = enforced", 0x5678)
                  .value_or(kInvalidArmSmmuMode));
  }
}

TEST(SmmuModeStringTest, Validation) {
  for (BackendKind kind : {BackendKind::kCpp, BackendKind::kRust}) {
    printf("[Backend: %s] SmmuModeStringTest.Validation\n", BackendTag(kind));
    fflush(stdout);

    // Test valid strings.
    EXPECT_TRUE(ValidateModeForBackend(kind, "disabled"));
    EXPECT_TRUE(ValidateModeForBackend(kind, "passthru"));
    EXPECT_TRUE(ValidateModeForBackend(kind, "enforced"));
    EXPECT_TRUE(ValidateModeForBackend(kind, "disabled,0x1234=passthru"));
    EXPECT_TRUE(ValidateModeForBackend(kind, "disabled,0x1234=passthru,0x5678=enforced"));
    EXPECT_TRUE(ValidateModeForBackend(kind, "  disabled  "));
    EXPECT_TRUE(ValidateModeForBackend(kind, "disabled , 0x1234 = passthru"));

    // Test invalid strings.
    EXPECT_FALSE(ValidateModeForBackend(kind, ""));
    EXPECT_FALSE(ValidateModeForBackend(kind, "pass thru"));
    EXPECT_FALSE(ValidateModeForBackend(kind, "disabled,0x1234passthru"));  // Malformed address
    EXPECT_FALSE(ValidateModeForBackend(kind, "disabled,0x1234=invalid"));  // Invalid mode
    EXPECT_FALSE(ValidateModeForBackend(kind, "disabled,0x1234="));         // Empty mode
    EXPECT_FALSE(ValidateModeForBackend(kind, "disabled,"));                // Trailing comma
    EXPECT_FALSE(ValidateModeForBackend(kind, "disabled,,"));               // Double comma
    EXPECT_FALSE(
        ValidateModeForBackend(kind, "invalid_segment,0x1234=passthru"));  // Bad segment first
  }
}

TEST(SmmuModeStringTest, RustCommonSmokeTest) { EXPECT_TRUE(rust_arm_smmu_common_smoke_test()); }

}  // namespace
}  // namespace arm_smmu
