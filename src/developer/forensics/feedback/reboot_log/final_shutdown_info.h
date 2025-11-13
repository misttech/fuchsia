// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_FEEDBACK_REBOOT_LOG_FINAL_SHUTDOWN_INFO_H_
#define SRC_DEVELOPER_FORENSICS_FEEDBACK_REBOOT_LOG_FINAL_SHUTDOWN_INFO_H_

#include <fuchsia/feedback/cpp/fidl.h>

#include <optional>
#include <string>

#include "src/developer/forensics/feedback/config.h"
#include "src/developer/forensics/feedback/reboot_log/graceful_shutdown_info.h"
#include "src/developer/forensics/utils/cobalt/metrics.h"

namespace forensics::feedback {

// Encapsulates the final information about why a device shutdown regardless of its source, e.g.
// Zircon's shutdown information or shutdown information from userspace.
class FinalShutdownInfo {
 public:
  virtual ~FinalShutdownInfo() = default;

  // Whether the reason is "out of memory."
  virtual bool IsOom() const = 0;

  // Whether the reason is deemed fatal.
  virtual bool IsFatal() const = 0;

  // Whether the reason justifies a crash report.
  virtual bool IsCrash() const = 0;

  // Whether the reboot is graceful, ungraceful or undetermined.
  virtual std::optional<bool> OptionallyGraceful() const = 0;

  // Whether the reboot is planned, unplanned or undetermined.
  virtual std::optional<bool> OptionallyPlanned() const = 0;

  // Returns the graceful shutdown action, if the shutdown was graceful and the action was
  // available.
  virtual std::optional<GracefulShutdownAction> ToGracefulShutdownAction() const = 0;

  // Returns the string representation of the reboot reason.
  virtual std::string ToRebootReasonString() const = 0;

  // Returns the reboot reason, translated into fuchsia::feedback::RebootReason. Returns
  // std::nullopt if an appropriate translation isn't possible.
  virtual std::optional<fuchsia::feedback::RebootReason> ToFidlRebootReason() const = 0;

  // Returns the reboot reason, translated into cobalt::LastRebootReason.
  virtual cobalt::LastRebootReason ToCobaltLastRebootReason() const = 0;

  // Returns the program name that should be used for the crash.
  virtual std::string ToCrashProgramName() const = 0;

  // Creates a crash signature for the underlying shutdown reason and action, if applicable.
  //
  // Note: |critical_process| is only supported for |ZirconRebootReason::kRootJobTermination|.
  virtual std::string ToCrashSignature(
      SpontaneousRebootReason spontaneous_reboot_reason,
      const std::optional<std::string>& critical_process) const = 0;
};

enum class ZirconRebootReason : std::uint8_t {
  kNotSet,
  kCold,
  kNoCrash,
  kKernelPanic,
  kOOM,
  kHwWatchdog,
  kSwWatchdog,
  kBrownout,
  kUnknown,
  kRootJobTermination,
  kNotParseable,
};

class FinalZirconShutdownInfo : public FinalShutdownInfo {
 public:
  // |zircon_reason| cannot be kNotSet nor kNoCrash.
  explicit FinalZirconShutdownInfo(ZirconRebootReason zircon_reason);

  bool IsOom() const override;

  bool IsFatal() const override;

  bool IsCrash() const override;

  std::optional<bool> OptionallyGraceful() const override;

  std::optional<bool> OptionallyPlanned() const override;

  std::optional<GracefulShutdownAction> ToGracefulShutdownAction() const override {
    return std::nullopt;
  }

  std::string ToRebootReasonString() const override;

  // TODO(https://fxbug.dev/441569016): Spontaneous reasons shouldn't all map to brief power loss.
  std::optional<fuchsia::feedback::RebootReason> ToFidlRebootReason() const override;

  cobalt::LastRebootReason ToCobaltLastRebootReason() const override;

  std::string ToCrashProgramName() const override;

  std::string ToCrashSignature(SpontaneousRebootReason spontaneous_reboot_reason,
                               const std::optional<std::string>& critical_process) const override;

 private:
  ZirconRebootReason zircon_reason_;
};

class FinalGracefulShutdownInfo : public FinalShutdownInfo {
 public:
  FinalGracefulShutdownInfo(std::optional<GracefulShutdownAction> action,
                            const std::vector<GracefulShutdownReason>& reasons, bool not_a_fdr);

  bool IsOom() const override;

  bool IsFatal() const override;

  bool IsCrash() const override;

  std::optional<bool> OptionallyGraceful() const override;

  std::optional<bool> OptionallyPlanned() const override;

  std::optional<GracefulShutdownAction> ToGracefulShutdownAction() const override {
    return action_;
  }

  std::string ToRebootReasonString() const override;

  std::optional<fuchsia::feedback::RebootReason> ToFidlRebootReason() const override;

  cobalt::LastRebootReason ToCobaltLastRebootReason() const override;

  std::string ToCrashProgramName() const override;

  std::string ToCrashSignature(SpontaneousRebootReason spontaneous_reboot_reason,
                               const std::optional<std::string>& critical_process) const override;

 private:
  enum class FinalGracefulShutdownReason : std::uint8_t {
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
    kOutOfMemory,
    kNetstackMigration,
    kAndroidUnexpectedReason,
    kAndroidRescueParty,
    kAndroidCriticalProcessFailure,
    kDeveloperRequest,
  };

  FinalGracefulShutdownReason ConsolidateGracefulShutdownReasons(
      const std::vector<GracefulShutdownReason>& reasons) const;

  std::optional<GracefulShutdownAction> action_;
  FinalGracefulShutdownReason final_reason_;
  bool not_a_fdr_;
};

}  // namespace forensics::feedback

#endif  // SRC_DEVELOPER_FORENSICS_FEEDBACK_REBOOT_LOG_FINAL_SHUTDOWN_INFO_H_
