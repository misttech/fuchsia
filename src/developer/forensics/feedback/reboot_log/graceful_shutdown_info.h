// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_FEEDBACK_REBOOT_LOG_GRACEFUL_SHUTDOWN_INFO_H_
#define SRC_DEVELOPER_FORENSICS_FEEDBACK_REBOOT_LOG_GRACEFUL_SHUTDOWN_INFO_H_

#include <fuchsia/hardware/power/statecontrol/cpp/fidl.h>

#include <string>

#include "src/developer/forensics/utils/cobalt/logger.h"

namespace forensics {
namespace feedback {

// Feedback's internal representation of why a device shutdown gracefully.
//
// These values should not be used to understand why a device has shutdown outside of this
// component.
enum class GracefulShutdownReason {
  kNotSet,
  kUserRequest,
  kSystemUpdate,
  kRetrySystemUpdate,
  kHighTemperature,
  kSessionFailure,
  // TODO(https://fxbug.dev/394392398): kSysmgrFailure is from CFv1, remove once it's no longer
  // written as a graceful shutdown reason or once deprecated reasons can be ignored.
  kSysmgrFailure,
  kCriticalComponentFailure,
  kFdr,
  kZbiSwap,
  kOutOfMemory,
  // TODO(https://fxbug.dev/42081574): Remove this reason once Netstack2 is
  // fully migrated to Netstack3.
  kNetstackMigration,
  kAndroidUnexpectedReason,
  kAndroidRescueParty,
  kAndroidCriticalProcessFailure,
  kDeveloperRequest,
  kNotSupported,
  kNotParseable,
};

std::string ToString(GracefulShutdownReason reason);

std::vector<GracefulShutdownReason> ToGracefulShutdownReasons(
    fuchsia::hardware::power::statecontrol::ShutdownOptions options);

// The input is limited to values corresponding to |power::statecontrol::ShutdownReason|.
std::vector<GracefulShutdownReason> FromLegacyTxtFile(std::string content);

// Only used for testing legacy functionality. The input is limited to GracefulShutdownReasons that
// map to |power::statecontrol::ShutdownReason|.
std::string ToLegacyFileContentForTesting(const std::vector<GracefulShutdownReason>& reasons);

// The input is limited to GracefulShutdownReasons that map to
// |power::statecontrol::ShutdownReason|.
//
// Note that some variants that should not be persisted (e.g. `kNotParseable`) are translated to
// `kNotSupported`.
std::vector<std::string> ToReasonStrings(const std::vector<GracefulShutdownReason>& reasons);

// The input is limited to values corresponding to |power::statecontrol::ShutdownReason|.
//
// The format is expected to be:
// {
//   reasons: [
//     "Reason 1",
//     "Reason 2"
//   ]
// }
std::vector<GracefulShutdownReason> FromJson(const std::string& content);

// The input is limited to GracefulShutdownReasons that map to
// |power::statecontrol::ShutdownReason|.
std::string ToJson(const std::vector<GracefulShutdownReason>& reasons);

// Converts the list of `GracefulShutdownReasons` into a single comma-separated string, like
// "Reason 1,Reason 2,Reason 3".
std::string ToRawStrings(const std::vector<GracefulShutdownReason>& reasons);

// Writes the graceful shutdown reasons to `path` and records metrics about the write.
void WriteGracefulShutdownInfo(const std::vector<GracefulShutdownReason>& reasons,
                               cobalt::Logger* cobalt, const std::string& path);

}  // namespace feedback
}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_FEEDBACK_REBOOT_LOG_GRACEFUL_SHUTDOWN_INFO_H_
