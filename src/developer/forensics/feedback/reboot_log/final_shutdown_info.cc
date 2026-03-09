// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/reboot_log/final_shutdown_info.h"

#include <lib/syslog/cpp/macros.h>

#include <unordered_set>

#include "src/developer/forensics/feedback/config.h"
#include "src/developer/forensics/feedback/reboot_log/hw_shutdown_reason.h"
#include "src/developer/forensics/feedback/reboot_log/zircon_shutdown_reason.h"
#include "src/developer/forensics/utils/cobalt/metrics.h"

namespace forensics::feedback {
namespace {

std::string GetSpontaneousRebootCrashSignature(
    const SpontaneousRebootReason spontaneous_reboot_reason) {
  switch (spontaneous_reboot_reason) {
    case SpontaneousRebootReason::kSpontaneous:
      return "fuchsia-spontaneous-reboot";
    case SpontaneousRebootReason::kBriefPowerLoss:
      return "fuchsia-brief-power-loss";
    case SpontaneousRebootReason::kHardReset:
      return "fuchsia-hard-reset";
  }
}

}  // namespace

FinalShutdownInfo::FinalShutdownInfo(FinalShutdownReason reason)
    : reason_(reason), graceful_shutdown_action_(std::nullopt) {}

FinalShutdownInfo::FinalShutdownInfo(FinalShutdownReason reason,
                                     std::optional<GracefulShutdownAction> graceful_shutdown_action)
    : reason_(reason), graceful_shutdown_action_(graceful_shutdown_action) {}

bool FinalShutdownInfo::IsOom() const { return reason_ == FinalShutdownReason::kOom; }

bool FinalShutdownInfo::IsCrash() const {
  switch (reason_) {
    case FinalShutdownReason::kKernelPanic:
    case FinalShutdownReason::kOom:
    case FinalShutdownReason::kHwWatchdog:
    case FinalShutdownReason::kSwWatchdog:
    case FinalShutdownReason::kBrownout:
    case FinalShutdownReason::kSpontaneousReboot:
    case FinalShutdownReason::kRootJobTermination:
    case FinalShutdownReason::kNotParseable:
    case FinalShutdownReason::kGenericGraceful:
    case FinalShutdownReason::kUnexpectedReasonGraceful:
    case FinalShutdownReason::kRetrySystemUpdate:
    case FinalShutdownReason::kHighTemperature:
    case FinalShutdownReason::kSessionFailure:
    case FinalShutdownReason::kSysmgrFailure:
    case FinalShutdownReason::kCriticalComponentFailure:
    case FinalShutdownReason::kAndroidUnexpectedReason:
    case FinalShutdownReason::kAndroidRescueParty:
    case FinalShutdownReason::kAndroidCriticalProcessFailure:
    case FinalShutdownReason::kUserRequestDeviceStuck:
      return true;
    case FinalShutdownReason::kCold:
    case FinalShutdownReason::kUserRequest:
    case FinalShutdownReason::kSystemUpdate:
    case FinalShutdownReason::kFdr:
    case FinalShutdownReason::kZbiSwap:
    case FinalShutdownReason::kNetstackMigration:
    case FinalShutdownReason::kDeveloperRequest:
    case FinalShutdownReason::kAndroidNoReason:
      return false;
  }
}

std::optional<bool> FinalShutdownInfo::OptionallyGraceful() const {
  switch (reason_) {
    case FinalShutdownReason::kCold:
    case FinalShutdownReason::kKernelPanic:
    case FinalShutdownReason::kOom:
    case FinalShutdownReason::kHwWatchdog:
    case FinalShutdownReason::kSwWatchdog:
    case FinalShutdownReason::kBrownout:
    case FinalShutdownReason::kSpontaneousReboot:
    case FinalShutdownReason::kRootJobTermination:
      return false;
    case FinalShutdownReason::kNotParseable:
      return std::nullopt;
    case FinalShutdownReason::kGenericGraceful:
    case FinalShutdownReason::kUnexpectedReasonGraceful:
    case FinalShutdownReason::kUserRequest:
    case FinalShutdownReason::kSystemUpdate:
    case FinalShutdownReason::kRetrySystemUpdate:
    case FinalShutdownReason::kHighTemperature:
    case FinalShutdownReason::kSessionFailure:
    case FinalShutdownReason::kSysmgrFailure:
    case FinalShutdownReason::kCriticalComponentFailure:
    case FinalShutdownReason::kFdr:
    case FinalShutdownReason::kZbiSwap:
    case FinalShutdownReason::kNetstackMigration:
    case FinalShutdownReason::kAndroidUnexpectedReason:
    case FinalShutdownReason::kAndroidNoReason:
    case FinalShutdownReason::kAndroidRescueParty:
    case FinalShutdownReason::kAndroidCriticalProcessFailure:
    case FinalShutdownReason::kDeveloperRequest:
    case FinalShutdownReason::kUserRequestDeviceStuck:
      return true;
  }
}

std::optional<bool> FinalShutdownInfo::OptionallyPlanned() const {
  switch (reason_) {
    case FinalShutdownReason::kCold:
    case FinalShutdownReason::kKernelPanic:
    case FinalShutdownReason::kOom:
    case FinalShutdownReason::kHwWatchdog:
    case FinalShutdownReason::kSwWatchdog:
    case FinalShutdownReason::kBrownout:
    case FinalShutdownReason::kSpontaneousReboot:
    case FinalShutdownReason::kRootJobTermination:
    case FinalShutdownReason::kGenericGraceful:
    case FinalShutdownReason::kUnexpectedReasonGraceful:
    case FinalShutdownReason::kUserRequest:
    case FinalShutdownReason::kRetrySystemUpdate:
    case FinalShutdownReason::kHighTemperature:
    case FinalShutdownReason::kSessionFailure:
    case FinalShutdownReason::kSysmgrFailure:
    case FinalShutdownReason::kCriticalComponentFailure:
    case FinalShutdownReason::kFdr:
    case FinalShutdownReason::kZbiSwap:
    case FinalShutdownReason::kAndroidUnexpectedReason:
    case FinalShutdownReason::kAndroidNoReason:
    case FinalShutdownReason::kAndroidRescueParty:
    case FinalShutdownReason::kAndroidCriticalProcessFailure:
    case FinalShutdownReason::kDeveloperRequest:
    case FinalShutdownReason::kUserRequestDeviceStuck:
      return false;
    case FinalShutdownReason::kNotParseable:
      return std::nullopt;
    case FinalShutdownReason::kSystemUpdate:
    case FinalShutdownReason::kNetstackMigration:
      return true;
  }
}

std::optional<GracefulShutdownAction> FinalShutdownInfo::ToGracefulShutdownAction() const {
  return graceful_shutdown_action_;
}

std::string FinalShutdownInfo::ToRebootReasonString() const {
  switch (reason_) {
    case FinalShutdownReason::kCold:
      return "COLD";
    case FinalShutdownReason::kKernelPanic:
      return "KERNEL PANIC";
    case FinalShutdownReason::kOom:
      return "OOM";
    case FinalShutdownReason::kHwWatchdog:
      return "HARDWARE WATCHDOG TIMEOUT";
    case FinalShutdownReason::kSwWatchdog:
      return "SOFTWARE WATCHDOG TIMEOUT";
    case FinalShutdownReason::kBrownout:
      return "BROWNOUT";
    case FinalShutdownReason::kSpontaneousReboot:
      return "SPONTANEOUS";
    case FinalShutdownReason::kRootJobTermination:
      return "ROOT JOB TERMINATION";
    case FinalShutdownReason::kNotParseable:
      return "NOT PARSEABLE";
    case FinalShutdownReason::kGenericGraceful:
      return "GENERIC GRACEFUL";
    case FinalShutdownReason::kUnexpectedReasonGraceful:
      return "UNEXPECTED REASON GRACEFUL";
    case FinalShutdownReason::kUserRequest:
      return "USER REQUEST";
    case FinalShutdownReason::kSystemUpdate:
      return "SYSTEM UPDATE";
    case FinalShutdownReason::kRetrySystemUpdate:
      return "RETRY SYSTEM UPDATE";
    case FinalShutdownReason::kHighTemperature:
      return "HIGH TEMPERATURE";
    case FinalShutdownReason::kSessionFailure:
      return "SESSION FAILURE";
    case FinalShutdownReason::kSysmgrFailure:
      return "SYSMGR FAILURE";
    case FinalShutdownReason::kCriticalComponentFailure:
      return "CRITICAL COMPONENT FAILURE";
    case FinalShutdownReason::kFdr:
      return "FACTORY DATA RESET";
    case FinalShutdownReason::kZbiSwap:
      return "ZBI SWAP";
    case FinalShutdownReason::kNetstackMigration:
      return "NETSTACK MIGRATION";
    case FinalShutdownReason::kAndroidUnexpectedReason:
      return "ANDROID UNEXPECTED REASON";
    case FinalShutdownReason::kAndroidNoReason:
      return "ANDROID NO REASON";
    case FinalShutdownReason::kAndroidRescueParty:
      return "ANDROID RESCUE PARTY";
    case FinalShutdownReason::kAndroidCriticalProcessFailure:
      return "ANDROID CRITICAL PROCESS FAILURE";
    case FinalShutdownReason::kDeveloperRequest:
      return "DEVELOPER REQUEST";
    case FinalShutdownReason::kUserRequestDeviceStuck:
      return "USER REQUEST DEVICE STUCK";
  }
}

std::optional<fuchsia::feedback::RebootReason> FinalShutdownInfo::ToFidlRebootReason() const {
  switch (reason_) {
    case FinalShutdownReason::kCold:
      return fuchsia::feedback::RebootReason::COLD;
    case FinalShutdownReason::kKernelPanic:
      return fuchsia::feedback::RebootReason::KERNEL_PANIC;
    case FinalShutdownReason::kOom:
      return fuchsia::feedback::RebootReason::SYSTEM_OUT_OF_MEMORY;
    case FinalShutdownReason::kHwWatchdog:
      return fuchsia::feedback::RebootReason::HARDWARE_WATCHDOG_TIMEOUT;
    case FinalShutdownReason::kSwWatchdog:
      return fuchsia::feedback::RebootReason::SOFTWARE_WATCHDOG_TIMEOUT;
    case FinalShutdownReason::kBrownout:
      return fuchsia::feedback::RebootReason::BROWNOUT;
    case FinalShutdownReason::kSpontaneousReboot:
      return fuchsia::feedback::RebootReason::BRIEF_POWER_LOSS;
    case FinalShutdownReason::kRootJobTermination:
      return fuchsia::feedback::RebootReason::ROOT_JOB_TERMINATION;
    case FinalShutdownReason::kNotParseable:
    case FinalShutdownReason::kGenericGraceful:
    case FinalShutdownReason::kUnexpectedReasonGraceful:
      return std::nullopt;
    case FinalShutdownReason::kUserRequest:
      return fuchsia::feedback::RebootReason::USER_REQUEST;
    case FinalShutdownReason::kSystemUpdate:
      return fuchsia::feedback::RebootReason::SYSTEM_UPDATE;
    case FinalShutdownReason::kRetrySystemUpdate:
      return fuchsia::feedback::RebootReason::RETRY_SYSTEM_UPDATE;
    case FinalShutdownReason::kHighTemperature:
      return fuchsia::feedback::RebootReason::HIGH_TEMPERATURE;
    case FinalShutdownReason::kSessionFailure:
      return fuchsia::feedback::RebootReason::SESSION_FAILURE;
    case FinalShutdownReason::kSysmgrFailure:
      return fuchsia::feedback::RebootReason::SYSMGR_FAILURE;
    case FinalShutdownReason::kCriticalComponentFailure:
      return fuchsia::feedback::RebootReason::CRITICAL_COMPONENT_FAILURE;
    case FinalShutdownReason::kFdr:
      return fuchsia::feedback::RebootReason::FACTORY_DATA_RESET;
    case FinalShutdownReason::kZbiSwap:
      return fuchsia::feedback::RebootReason::ZBI_SWAP;
    case FinalShutdownReason::kNetstackMigration:
      return fuchsia::feedback::RebootReason::NETSTACK_MIGRATION;
    case FinalShutdownReason::kAndroidUnexpectedReason:
      return fuchsia::feedback::RebootReason::ANDROID_UNEXPECTED_REASON;
    case FinalShutdownReason::kAndroidNoReason:
      return fuchsia::feedback::RebootReason::ANDROID_NO_REASON;
    case FinalShutdownReason::kAndroidRescueParty:
      return fuchsia::feedback::RebootReason::ANDROID_RESCUE_PARTY;
    case FinalShutdownReason::kAndroidCriticalProcessFailure:
      return fuchsia::feedback::RebootReason::ANDROID_CRITICAL_PROCESS_FAILURE;
    case FinalShutdownReason::kDeveloperRequest:
      return fuchsia::feedback::RebootReason::DEVELOPER_REQUEST;
    case FinalShutdownReason::kUserRequestDeviceStuck:
      return fuchsia::feedback::RebootReason::USER_REQUEST_DEVICE_STUCK;
  }
}

cobalt::LastRebootReason FinalShutdownInfo::ToCobaltLastRebootReason() const {
  switch (reason_) {
    case FinalShutdownReason::kCold:
      return cobalt::LastRebootReason::kCold;
    case FinalShutdownReason::kKernelPanic:
      return cobalt::LastRebootReason::kKernelPanic;
    case FinalShutdownReason::kOom:
      return cobalt::LastRebootReason::kSystemOutOfMemory;
    case FinalShutdownReason::kHwWatchdog:
      return cobalt::LastRebootReason::kHardwareWatchdogTimeout;
    case FinalShutdownReason::kSwWatchdog:
      return cobalt::LastRebootReason::kSoftwareWatchdogTimeout;
    case FinalShutdownReason::kBrownout:
      return cobalt::LastRebootReason::kBrownout;
    case FinalShutdownReason::kSpontaneousReboot:
      return cobalt::LastRebootReason::kBriefPowerLoss;
    case FinalShutdownReason::kRootJobTermination:
      return cobalt::LastRebootReason::kRootJobTermination;
    case FinalShutdownReason::kNotParseable:
      return cobalt::LastRebootReason::kUnknown;
    case FinalShutdownReason::kGenericGraceful:
      return cobalt::LastRebootReason::kGenericGraceful;
    case FinalShutdownReason::kUnexpectedReasonGraceful:
      return cobalt::LastRebootReason::kUnexpectedReasonGraceful;
    case FinalShutdownReason::kUserRequest:
      return cobalt::LastRebootReason::kUserRequest;
    case FinalShutdownReason::kSystemUpdate:
      return cobalt::LastRebootReason::kSystemUpdate;
    case FinalShutdownReason::kRetrySystemUpdate:
      return cobalt::LastRebootReason::kRetrySystemUpdate;
    case FinalShutdownReason::kHighTemperature:
      return cobalt::LastRebootReason::kHighTemperature;
    case FinalShutdownReason::kSessionFailure:
      return cobalt::LastRebootReason::kSessionFailure;
    case FinalShutdownReason::kSysmgrFailure:
      return cobalt::LastRebootReason::kSysmgrFailure;
    case FinalShutdownReason::kCriticalComponentFailure:
      return cobalt::LastRebootReason::kCriticalComponentFailure;
    case FinalShutdownReason::kFdr:
      return cobalt::LastRebootReason::kFactoryDataReset;
    case FinalShutdownReason::kZbiSwap:
      return cobalt::LastRebootReason::kZbiSwap;
    case FinalShutdownReason::kNetstackMigration:
      return cobalt::LastRebootReason::kNetstackMigration;
    case FinalShutdownReason::kAndroidUnexpectedReason:
      return cobalt::LastRebootReason::kAndroidUnexpectedReason;
    case FinalShutdownReason::kAndroidNoReason:
      return cobalt::LastRebootReason::kAndroidNoReason;
    case FinalShutdownReason::kAndroidRescueParty:
      return cobalt::LastRebootReason::kAndroidRescueParty;
    case FinalShutdownReason::kAndroidCriticalProcessFailure:
      return cobalt::LastRebootReason::kAndroidCriticalProcessFailure;
    case FinalShutdownReason::kDeveloperRequest:
      return cobalt::LastRebootReason::kDeveloperRequest;
    case FinalShutdownReason::kUserRequestDeviceStuck:
      return cobalt::LastRebootReason::kUserRequestDeviceStuck;
  }
}

std::string FinalShutdownInfo::ToCrashProgramName() const {
  switch (reason_) {
    case FinalShutdownReason::kNotParseable:
      return "reboot-log";
    case FinalShutdownReason::kKernelPanic:
      return "kernel";
    case FinalShutdownReason::kHwWatchdog:
    case FinalShutdownReason::kBrownout:
    case FinalShutdownReason::kSpontaneousReboot:
      return "device";
    case FinalShutdownReason::kOom:
    case FinalShutdownReason::kSwWatchdog:
    case FinalShutdownReason::kRootJobTermination:
    case FinalShutdownReason::kGenericGraceful:
    case FinalShutdownReason::kUnexpectedReasonGraceful:
    case FinalShutdownReason::kRetrySystemUpdate:
    case FinalShutdownReason::kHighTemperature:
    case FinalShutdownReason::kSessionFailure:
    case FinalShutdownReason::kSysmgrFailure:
    case FinalShutdownReason::kCriticalComponentFailure:
    case FinalShutdownReason::kUserRequestDeviceStuck:
      return "system";
    case FinalShutdownReason::kAndroidUnexpectedReason:
    case FinalShutdownReason::kAndroidNoReason:
    case FinalShutdownReason::kAndroidRescueParty:
    case FinalShutdownReason::kAndroidCriticalProcessFailure:
      return "android";
    case FinalShutdownReason::kCold:
    case FinalShutdownReason::kUserRequest:
    case FinalShutdownReason::kDeveloperRequest:
    case FinalShutdownReason::kSystemUpdate:
    case FinalShutdownReason::kNetstackMigration:
    case FinalShutdownReason::kZbiSwap:
    case FinalShutdownReason::kFdr:
      FX_LOGS(FATAL) << "Not expecting a program name request for reboot reason: "
                     << ToRebootReasonString();
      return "FATAL ERROR";
  }
}

std::string FinalShutdownInfo::ToCrashSignature(
    const SpontaneousRebootReason spontaneous_reboot_reason,
    const std::optional<std::string>& critical_process) const {
  switch (reason_) {
    case FinalShutdownReason::kNotParseable:
      return "fuchsia-reboot-log-not-parseable";
    case FinalShutdownReason::kSpontaneousReboot:
      return GetSpontaneousRebootCrashSignature(spontaneous_reboot_reason);
    case FinalShutdownReason::kKernelPanic:
      return "fuchsia-kernel-panic";
    case FinalShutdownReason::kOom:
      return "fuchsia-oom";
    case FinalShutdownReason::kHwWatchdog:
      return "fuchsia-hw-watchdog-timeout";
    case FinalShutdownReason::kSwWatchdog:
      return "fuchsia-sw-watchdog-timeout";
    case FinalShutdownReason::kBrownout:
      return "fuchsia-brownout";
    case FinalShutdownReason::kRootJobTermination:
      return (!critical_process.has_value())
                 ? "fuchsia-root-job-termination"
                 : std::string("fuchsia-reboot-").append(*critical_process).append("-terminated");
    case FinalShutdownReason::kRetrySystemUpdate:
      return "fuchsia-retry-system-update";
    case FinalShutdownReason::kHighTemperature:
      return "fuchsia-shutdown-high-temperature";
    case FinalShutdownReason::kSessionFailure:
      return "fuchsia-session-failure";
    case FinalShutdownReason::kSysmgrFailure:
      return "fuchsia-sysmgr-failure";
    case FinalShutdownReason::kCriticalComponentFailure:
      return "fuchsia-critical-component-failure";
    case FinalShutdownReason::kAndroidUnexpectedReason:
      return "fuchsia-shutdown-android-unexpected-reason";
    case FinalShutdownReason::kAndroidRescueParty:
      return "fuchsia-shutdown-android-rescue-party";
    case FinalShutdownReason::kAndroidCriticalProcessFailure:
      return "fuchsia-shutdown-android-critical-process-failure";
    case FinalShutdownReason::kUserRequestDeviceStuck:
      return "fuchsia-shutdown-user-request-device-stuck";
    case FinalShutdownReason::kGenericGraceful:
      return "fuchsia-shutdown-undetermined-userspace-reason";
    case FinalShutdownReason::kUnexpectedReasonGraceful:
      return "fuchsia-shutdown-unexpected-userspace-reason";
    case FinalShutdownReason::kCold:
    case FinalShutdownReason::kUserRequest:
    case FinalShutdownReason::kSystemUpdate:
    case FinalShutdownReason::kFdr:
    case FinalShutdownReason::kZbiSwap:
    case FinalShutdownReason::kNetstackMigration:
    case FinalShutdownReason::kDeveloperRequest:
    case FinalShutdownReason::kAndroidNoReason:
      FX_LOGS(FATAL) << "Not expecting a crash for reason: " << ToRebootReasonString();
      return "FATAL ERROR";
  }
}

namespace {

FinalShutdownReason ConsolidateGracefulShutdownReasons(
    const std::vector<GracefulShutdownReason>& reasons) {
  if (reasons.empty()) {
    return FinalShutdownReason::kGenericGraceful;
  }

  // If there's only one reason, consolidation is trivial.
  if (reasons.size() == 1) {
    switch (reasons[0]) {
      case GracefulShutdownReason::kUserRequest:
        return FinalShutdownReason::kUserRequest;
      case GracefulShutdownReason::kSystemUpdate:
        return FinalShutdownReason::kSystemUpdate;
      case GracefulShutdownReason::kRetrySystemUpdate:
        return FinalShutdownReason::kRetrySystemUpdate;
      case GracefulShutdownReason::kHighTemperature:
        return FinalShutdownReason::kHighTemperature;
      case GracefulShutdownReason::kSessionFailure:
        return FinalShutdownReason::kSessionFailure;
      case GracefulShutdownReason::kSysmgrFailure:
        return FinalShutdownReason::kSysmgrFailure;
      case GracefulShutdownReason::kCriticalComponentFailure:
        return FinalShutdownReason::kCriticalComponentFailure;
      case GracefulShutdownReason::kFdr:
        return FinalShutdownReason::kFdr;
      case GracefulShutdownReason::kZbiSwap:
        return FinalShutdownReason::kZbiSwap;
      case GracefulShutdownReason::kNotSupported:
      case GracefulShutdownReason::kNotParseable:
        return FinalShutdownReason::kGenericGraceful;
      case GracefulShutdownReason::kOutOfMemory:
        return FinalShutdownReason::kOom;
      case GracefulShutdownReason::kNetstackMigration:
        return FinalShutdownReason::kNetstackMigration;
      case GracefulShutdownReason::kAndroidUnexpectedReason:
        return FinalShutdownReason::kAndroidUnexpectedReason;
      case GracefulShutdownReason::kAndroidNoReason:
        return FinalShutdownReason::kAndroidNoReason;
      case GracefulShutdownReason::kAndroidRescueParty:
        return FinalShutdownReason::kAndroidRescueParty;
      case GracefulShutdownReason::kAndroidCriticalProcessFailure:
        return FinalShutdownReason::kAndroidCriticalProcessFailure;
      case GracefulShutdownReason::kDeveloperRequest:
        return FinalShutdownReason::kDeveloperRequest;
      case GracefulShutdownReason::kUserRequestDeviceStuck:
        return FinalShutdownReason::kUserRequestDeviceStuck;
      case GracefulShutdownReason::kNotSet:
        FX_LOGS(FATAL) << "Graceful shutdown reason must be set";
        return FinalShutdownReason::kUnexpectedReasonGraceful;
    }
  }

  // Otherwise, verify it's an expected combination of reasons.
  std::unordered_set<GracefulShutdownReason> reasons_set(reasons.begin(), reasons.end());
  if (reasons_set.size() == 2 && reasons_set.contains(GracefulShutdownReason::kNetstackMigration) &&
      reasons_set.contains(GracefulShutdownReason::kSystemUpdate)) {
    // Netstack Migration + System Update is consolidated to System Update.
    return FinalShutdownReason::kSystemUpdate;
  }

  FX_LOGS(WARNING) << "Unexpected combination of graceful shutdown reasons: "
                   << ToRawStrings(reasons);
  return FinalShutdownReason::kUnexpectedReasonGraceful;
}

}  // namespace

// static
std::unique_ptr<FinalShutdownInfo> FinalShutdownInfo::MakeFinalShutdownInfo(
    const HwShutdownReason hw_reason, const ZirconShutdownReason zircon_reason,
    std::optional<GracefulShutdownInfo> graceful_shutdown_info, const bool not_a_fdr) {
  switch (hw_reason) {
    case HwShutdownReason::kNotParseable:
      return std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kNotParseable);
    case HwShutdownReason::kWatchdog:
      return std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kHwWatchdog);
    case HwShutdownReason::kBrownout:
      return std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kBrownout);
    case HwShutdownReason::kNotSet:
    case HwShutdownReason::kUndefined:
    case HwShutdownReason::kCold:
    case HwShutdownReason::kWarm:
      break;
  }

  switch (zircon_reason) {
    case ZirconShutdownReason::kKernelPanic:
      return std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kKernelPanic);
    case ZirconShutdownReason::kOOM:
      return std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kOom);
    case ZirconShutdownReason::kSwWatchdog:
      return std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kSwWatchdog);
    case ZirconShutdownReason::kUnknown:
      return std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kSpontaneousReboot);
    case ZirconShutdownReason::kNotParseable:
      return std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kNotParseable);
    case ZirconShutdownReason::kRootJobTermination:
      return std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kRootJobTermination);
    case ZirconShutdownReason::kNoCrash: {
      if (!not_a_fdr) {
        return std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kFdr);
      }
      if (graceful_shutdown_info.has_value()) {
        return std::make_unique<FinalShutdownInfo>(
            ConsolidateGracefulShutdownReasons(graceful_shutdown_info->reasons),
            graceful_shutdown_info->action);
      }
      return std::make_unique<FinalShutdownInfo>(FinalShutdownReason::kGenericGraceful);
    }
    case ZirconShutdownReason::kNotSet:
      break;
  }

  // Graceful poweroffs will likely result in a cold boot. If so, the graceful info might have
  // reasons more informative than just "cold."
  if (graceful_shutdown_info.has_value() &&
      graceful_shutdown_info->action == GracefulShutdownAction::kPoweroff) {
    return std::make_unique<FinalShutdownInfo>(
        ConsolidateGracefulShutdownReasons(graceful_shutdown_info->reasons),
        graceful_shutdown_info->action);
  }

  // TODO(https://fxbug.dev/489823517): check for FDR and use that instead of cold.

  // While we now distinguish HwShutdownReason being cold, warm, undefined and not set,
  // for now we still report all of them as cold boots.
  return std::make_unique<FinalShutdownInfo>(
      FinalShutdownReason::kCold, graceful_shutdown_info.has_value()
                                      ? std::make_optional(graceful_shutdown_info->action)
                                      : std::nullopt);
}

}  // namespace forensics::feedback
