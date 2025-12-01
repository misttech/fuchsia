// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/reboot_log/annotations.h"

#include <gtest/gtest.h>

#include "src/developer/forensics/feedback/config.h"
#include "src/developer/forensics/feedback/reboot_log/final_shutdown_info.h"

namespace forensics::feedback {
namespace {

TEST(AnnotationsTest, LastRebootReasonAnnotationSpontaneous) {
  EXPECT_EQ(LastRebootReasonAnnotation(FinalZirconShutdownInfo(ZirconRebootReason::kUnknown),
                                       SpontaneousRebootReason::kSpontaneous),
            "spontaneous");
}

TEST(AnnotationsTest, LastRebootReasonAnnotationBriefPowerLoss) {
  EXPECT_EQ(LastRebootReasonAnnotation(FinalZirconShutdownInfo(ZirconRebootReason::kUnknown),
                                       SpontaneousRebootReason::kBriefPowerLoss),
            "brief loss of power");
}

TEST(AnnotationsTest, LastRebootReasonAnnotationHardReset) {
  EXPECT_EQ(LastRebootReasonAnnotation(FinalZirconShutdownInfo(ZirconRebootReason::kUnknown),
                                       SpontaneousRebootReason::kHardReset),
            "hard reset");
}

}  // namespace
}  // namespace forensics::feedback
