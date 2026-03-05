// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_FEEDBACK_REBOOT_LOG_FINAL_SHUTDOWN_INFO_H_
#define SRC_DEVELOPER_FORENSICS_FEEDBACK_REBOOT_LOG_FINAL_SHUTDOWN_INFO_H_

#include <fuchsia/feedback/cpp/fidl.h>

#include <memory>
#include <optional>
#include <string>

#include "src/developer/forensics/feedback/config.h"
#include "src/developer/forensics/feedback/reboot_log/graceful_shutdown_info.h"
#include "src/developer/forensics/feedback/reboot_log/zircon_shutdown_reason.h"
#include "src/developer/forensics/utils/cobalt/metrics.h"

namespace forensics::feedback {

enum class FinalShutdownReason : std::uint8_t {
  // Should map to ZirconRebootReason without kNotSet and kNoCrash.
  kCold,
  kBrownout,
  kHwWatchdog,
  kSpontaneousReboot,
  kKernelPanic,
  kOom,
  kSwWatchdog,
  kRootJobTermination,
  kZirconNotParseable,

  // Should map to GracefulShutdownReason without kNotSet, kNotSupported and kNotParseable.
  kGenericGraceful,
  kUnexpectedReasonGraceful,
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
  kNetstackMigration,
  kAndroidUnexpectedReason,
  kAndroidNoReason,
  kAndroidRescueParty,
  kAndroidCriticalProcessFailure,
  kDeveloperRequest,
  kUserRequestDeviceStuck,
};

// Encapsulates the final information about why a device shutdown regardless of its source, e.g.
// Zircon's shutdown information or shutdown information from userspace.
class FinalShutdownInfo {
 public:
  static std::unique_ptr<FinalShutdownInfo> MakeFinalShutdownInfo(
      const ZirconShutdownReason zircon_reason,
      std::optional<GracefulShutdownInfo> graceful_shutdown_info, const bool not_a_fdr);

  // For testing purposes.
  FinalShutdownInfo(FinalShutdownReason reason);
  FinalShutdownInfo(FinalShutdownReason reason,
                    std::optional<GracefulShutdownAction> graceful_shutdown_action);

  // Whether the reason is "out of memory."
  bool IsOom() const;

  // Whether the reason justifies a crash report.
  bool IsCrash() const;

  // Whether the reboot is graceful, ungraceful or undetermined.
  std::optional<bool> OptionallyGraceful() const;

  // Whether the reboot is planned, unplanned or undetermined.
  std::optional<bool> OptionallyPlanned() const;

  // Returns the graceful shutdown action, if the action was available and deemed relevant.
  std::optional<GracefulShutdownAction> ToGracefulShutdownAction() const;

  // Returns the string representation of the reboot reason.
  std::string ToRebootReasonString() const;

  // Returns the reboot reason, translated into fuchsia::feedback::RebootReason. Returns
  // std::nullopt if an appropriate translation isn't possible.
  // TODO(https://fxbug.dev/441569016): Spontaneous reasons shouldn't all map to brief power loss.
  std::optional<fuchsia::feedback::RebootReason> ToFidlRebootReason() const;

  // Returns the reboot reason, translated into cobalt::LastRebootReason.
  cobalt::LastRebootReason ToCobaltLastRebootReason() const;

  // Returns the program name that should be used for the crash.
  std::string ToCrashProgramName() const;

  // Creates a crash signature for the underlying shutdown reason and action, if applicable.
  //
  // Note: |critical_process| is only supported for |FinalShutdownReason::kRootJobTermination|.
  std::string ToCrashSignature(SpontaneousRebootReason spontaneous_reboot_reason,
                               const std::optional<std::string>& critical_process) const;

 private:
  FinalShutdownReason reason_;
  std::optional<GracefulShutdownAction> graceful_shutdown_action_;
};

}  // namespace forensics::feedback

#endif  // SRC_DEVELOPER_FORENSICS_FEEDBACK_REBOOT_LOG_FINAL_SHUTDOWN_INFO_H_
