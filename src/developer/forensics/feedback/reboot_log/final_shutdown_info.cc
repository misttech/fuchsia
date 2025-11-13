// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/reboot_log/final_shutdown_info.h"

#include <lib/syslog/cpp/macros.h>

#include <unordered_set>

#include "src/developer/forensics/feedback/config.h"
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

FinalZirconShutdownInfo::FinalZirconShutdownInfo(const ZirconRebootReason zircon_reason)
    : zircon_reason_(zircon_reason) {
  FX_CHECK(zircon_reason_ != ZirconRebootReason::kNoCrash);
  FX_CHECK(zircon_reason_ != ZirconRebootReason::kNotSet);
}

FinalGracefulShutdownInfo::FinalGracefulShutdownInfo(
    std::optional<GracefulShutdownAction> action,
    const std::vector<GracefulShutdownReason>& reasons, bool not_a_fdr)
    : action_(action), not_a_fdr_(not_a_fdr) {
  final_reason_ = ConsolidateGracefulShutdownReasons(reasons);
}

bool FinalZirconShutdownInfo::IsOom() const { return zircon_reason_ == ZirconRebootReason::kOOM; }

bool FinalGracefulShutdownInfo::IsOom() const {
  return final_reason_ == FinalGracefulShutdownReason::kOutOfMemory;
}

bool FinalZirconShutdownInfo::IsFatal() const {
  switch (zircon_reason_) {
    case ZirconRebootReason::kKernelPanic:
    case ZirconRebootReason::kOOM:
    case ZirconRebootReason::kHwWatchdog:
    case ZirconRebootReason::kSwWatchdog:
    case ZirconRebootReason::kBrownout:
    case ZirconRebootReason::kUnknown:
    case ZirconRebootReason::kRootJobTermination:
    case ZirconRebootReason::kNotParseable:
      return true;
    case ZirconRebootReason::kCold:
      return false;
    case ZirconRebootReason::kNoCrash:
    case ZirconRebootReason::kNotSet:
      FX_LOGS(FATAL) << "FinalZirconShutdownInfo shouldn't be constructed with reason: "
                     << ToRebootReasonString();
      return false;
  }
}

bool FinalGracefulShutdownInfo::IsFatal() const {
  switch (final_reason_) {
    case FinalGracefulShutdownReason::kGenericGraceful:
    case FinalGracefulShutdownReason::kUnexpectedReasonGraceful:
    case FinalGracefulShutdownReason::kRetrySystemUpdate:
    case FinalGracefulShutdownReason::kHighTemperature:
    case FinalGracefulShutdownReason::kSysmgrFailure:
    case FinalGracefulShutdownReason::kCriticalComponentFailure:
    case FinalGracefulShutdownReason::kOutOfMemory:
    case FinalGracefulShutdownReason::kAndroidUnexpectedReason:
    case FinalGracefulShutdownReason::kAndroidRescueParty:
    case FinalGracefulShutdownReason::kAndroidCriticalProcessFailure:
      return true;
    case FinalGracefulShutdownReason::kUserRequest:
    case FinalGracefulShutdownReason::kSystemUpdate:
    case FinalGracefulShutdownReason::kSessionFailure:
    case FinalGracefulShutdownReason::kFdr:
    case FinalGracefulShutdownReason::kZbiSwap:
    case FinalGracefulShutdownReason::kNetstackMigration:
    case FinalGracefulShutdownReason::kDeveloperRequest:
      return false;
  }
}

bool FinalZirconShutdownInfo::IsCrash() const {
  switch (zircon_reason_) {
    case ZirconRebootReason::kKernelPanic:
    case ZirconRebootReason::kOOM:
    case ZirconRebootReason::kHwWatchdog:
    case ZirconRebootReason::kSwWatchdog:
    case ZirconRebootReason::kBrownout:
    case ZirconRebootReason::kUnknown:
    case ZirconRebootReason::kRootJobTermination:
    case ZirconRebootReason::kNotParseable:
      return true;
    case ZirconRebootReason::kCold:
      return false;
    case ZirconRebootReason::kNoCrash:
    case ZirconRebootReason::kNotSet:
      FX_LOGS(FATAL) << "FinalZirconShutdownInfo shouldn't be constructed with reason: "
                     << ToRebootReasonString();
      return false;
  }
}

bool FinalGracefulShutdownInfo::IsCrash() const {
  switch (final_reason_) {
    case FinalGracefulShutdownReason::kGenericGraceful:
    case FinalGracefulShutdownReason::kUnexpectedReasonGraceful:
    case FinalGracefulShutdownReason::kRetrySystemUpdate:
    case FinalGracefulShutdownReason::kHighTemperature:
    case FinalGracefulShutdownReason::kSessionFailure:
    case FinalGracefulShutdownReason::kSysmgrFailure:
    case FinalGracefulShutdownReason::kCriticalComponentFailure:
    case FinalGracefulShutdownReason::kOutOfMemory:
    case FinalGracefulShutdownReason::kAndroidUnexpectedReason:
    case FinalGracefulShutdownReason::kAndroidRescueParty:
    case FinalGracefulShutdownReason::kAndroidCriticalProcessFailure:
      return true;
    case FinalGracefulShutdownReason::kUserRequest:
    case FinalGracefulShutdownReason::kSystemUpdate:
    case FinalGracefulShutdownReason::kFdr:
    case FinalGracefulShutdownReason::kZbiSwap:
    case FinalGracefulShutdownReason::kNetstackMigration:
    case FinalGracefulShutdownReason::kDeveloperRequest:
      return false;
  }
}

std::optional<bool> FinalZirconShutdownInfo::OptionallyGraceful() const {
  switch (zircon_reason_) {
    case ZirconRebootReason::kCold:
    case ZirconRebootReason::kKernelPanic:
    case ZirconRebootReason::kOOM:
    case ZirconRebootReason::kHwWatchdog:
    case ZirconRebootReason::kSwWatchdog:
    case ZirconRebootReason::kBrownout:
    case ZirconRebootReason::kUnknown:
    case ZirconRebootReason::kRootJobTermination:
      return false;
    case ZirconRebootReason::kNotParseable:
      return std::nullopt;
    case ZirconRebootReason::kNoCrash:
    case ZirconRebootReason::kNotSet:
      FX_LOGS(FATAL) << "FinalZirconShutdownInfo shouldn't be constructed with reason: "
                     << ToRebootReasonString();
      return std::nullopt;
  }
}

std::optional<bool> FinalGracefulShutdownInfo::OptionallyGraceful() const {
  switch (final_reason_) {
    case FinalGracefulShutdownReason::kGenericGraceful:
    case FinalGracefulShutdownReason::kUnexpectedReasonGraceful:
    case FinalGracefulShutdownReason::kUserRequest:
    case FinalGracefulShutdownReason::kSystemUpdate:
    case FinalGracefulShutdownReason::kRetrySystemUpdate:
    case FinalGracefulShutdownReason::kHighTemperature:
    case FinalGracefulShutdownReason::kSessionFailure:
    case FinalGracefulShutdownReason::kSysmgrFailure:
    case FinalGracefulShutdownReason::kCriticalComponentFailure:
    case FinalGracefulShutdownReason::kFdr:
    case FinalGracefulShutdownReason::kZbiSwap:
    case FinalGracefulShutdownReason::kNetstackMigration:
    case FinalGracefulShutdownReason::kAndroidUnexpectedReason:
    case FinalGracefulShutdownReason::kAndroidRescueParty:
    case FinalGracefulShutdownReason::kAndroidCriticalProcessFailure:
    case FinalGracefulShutdownReason::kDeveloperRequest:
      return true;
    case FinalGracefulShutdownReason::kOutOfMemory:
      return false;
  }
}

std::optional<bool> FinalZirconShutdownInfo::OptionallyPlanned() const {
  switch (zircon_reason_) {
    case ZirconRebootReason::kCold:
    case ZirconRebootReason::kKernelPanic:
    case ZirconRebootReason::kOOM:
    case ZirconRebootReason::kHwWatchdog:
    case ZirconRebootReason::kSwWatchdog:
    case ZirconRebootReason::kBrownout:
    case ZirconRebootReason::kUnknown:
    case ZirconRebootReason::kRootJobTermination:
      return false;
    case ZirconRebootReason::kNotParseable:
      return std::nullopt;
    case ZirconRebootReason::kNoCrash:
    case ZirconRebootReason::kNotSet:
      FX_LOGS(FATAL) << "FinalZirconShutdownInfo shouldn't be constructed with reason: "
                     << ToRebootReasonString();
      return std::nullopt;
  }
}

std::optional<bool> FinalGracefulShutdownInfo::OptionallyPlanned() const {
  switch (final_reason_) {
    case FinalGracefulShutdownReason::kSystemUpdate:
    case FinalGracefulShutdownReason::kNetstackMigration:
      return true;
    case FinalGracefulShutdownReason::kGenericGraceful:
    case FinalGracefulShutdownReason::kUnexpectedReasonGraceful:
    case FinalGracefulShutdownReason::kUserRequest:
    case FinalGracefulShutdownReason::kRetrySystemUpdate:
    case FinalGracefulShutdownReason::kHighTemperature:
    case FinalGracefulShutdownReason::kSessionFailure:
    case FinalGracefulShutdownReason::kSysmgrFailure:
    case FinalGracefulShutdownReason::kCriticalComponentFailure:
    case FinalGracefulShutdownReason::kFdr:
    case FinalGracefulShutdownReason::kZbiSwap:
    case FinalGracefulShutdownReason::kOutOfMemory:
    case FinalGracefulShutdownReason::kAndroidUnexpectedReason:
    case FinalGracefulShutdownReason::kAndroidRescueParty:
    case FinalGracefulShutdownReason::kAndroidCriticalProcessFailure:
    case FinalGracefulShutdownReason::kDeveloperRequest:
      return false;
  }
}

std::string FinalZirconShutdownInfo::ToRebootReasonString() const {
  switch (zircon_reason_) {
    case ZirconRebootReason::kCold:
      return "COLD";
    case ZirconRebootReason::kKernelPanic:
      return "KERNEL PANIC";
    case ZirconRebootReason::kOOM:
      return "OOM";
    case ZirconRebootReason::kHwWatchdog:
      return "HARDWARE WATCHDOG TIMEOUT";
    case ZirconRebootReason::kSwWatchdog:
      return "SOFTWARE WATCHDOG TIMEOUT";
    case ZirconRebootReason::kBrownout:
      return "BROWNOUT";
    case ZirconRebootReason::kUnknown:
      return "SPONTANEOUS";
    case ZirconRebootReason::kRootJobTermination:
      return "ROOT JOB TERMINATION";
    case ZirconRebootReason::kNotParseable:
      return "NOT PARSEABLE";
    case ZirconRebootReason::kNoCrash:
      FX_LOGS(FATAL) << "FinalZirconShutdownInfo shouldn't be constructed with kNoCrash";
      return "FATAL ERROR";
    case ZirconRebootReason::kNotSet:
      FX_LOGS(FATAL) << "FinalZirconShutdownInfo shouldn't be constructed with kNotSet";
      return "FATAL ERROR";
  }
}

std::string FinalGracefulShutdownInfo::ToRebootReasonString() const {
  switch (final_reason_) {
    case FinalGracefulShutdownReason::kGenericGraceful:
      return "GENERIC GRACEFUL";
    case FinalGracefulShutdownReason::kUnexpectedReasonGraceful:
      return "UNEXPECTED REASON GRACEFUL";
    case FinalGracefulShutdownReason::kUserRequest:
      return "USER REQUEST";
    case FinalGracefulShutdownReason::kSystemUpdate:
      return "SYSTEM UPDATE";
    case FinalGracefulShutdownReason::kRetrySystemUpdate:
      return "RETRY SYSTEM UPDATE";
    case FinalGracefulShutdownReason::kHighTemperature:
      return "HIGH TEMPERATURE";
    case FinalGracefulShutdownReason::kSessionFailure:
      return "SESSION FAILURE";
    case FinalGracefulShutdownReason::kSysmgrFailure:
      return "SYSMGR FAILURE";
    case FinalGracefulShutdownReason::kCriticalComponentFailure:
      return "CRITICAL COMPONENT FAILURE";
    case FinalGracefulShutdownReason::kFdr:
      return "FACTORY DATA RESET";
    case FinalGracefulShutdownReason::kZbiSwap:
      return "ZBI SWAP";
    case FinalGracefulShutdownReason::kOutOfMemory:
      return "OOM";
    case FinalGracefulShutdownReason::kNetstackMigration:
      return "NETSTACK MIGRATION";
    case FinalGracefulShutdownReason::kAndroidUnexpectedReason:
      return "ANDROID UNEXPECTED REASON";
    case FinalGracefulShutdownReason::kAndroidRescueParty:
      return "ANDROID RESCUE PARTY";
    case FinalGracefulShutdownReason::kAndroidCriticalProcessFailure:
      return "ANDROID CRITICAL PROCESS FAILURE";
    case FinalGracefulShutdownReason::kDeveloperRequest:
      return "DEVELOPER REQUEST";
  }
}

std::optional<fuchsia::feedback::RebootReason> FinalZirconShutdownInfo::ToFidlRebootReason() const {
  switch (zircon_reason_) {
    case ZirconRebootReason::kCold:
      return fuchsia::feedback::RebootReason::COLD;
    case ZirconRebootReason::kKernelPanic:
      return fuchsia::feedback::RebootReason::KERNEL_PANIC;
    case ZirconRebootReason::kOOM:
      return fuchsia::feedback::RebootReason::SYSTEM_OUT_OF_MEMORY;
    case ZirconRebootReason::kHwWatchdog:
      return fuchsia::feedback::RebootReason::HARDWARE_WATCHDOG_TIMEOUT;
    case ZirconRebootReason::kSwWatchdog:
      return fuchsia::feedback::RebootReason::SOFTWARE_WATCHDOG_TIMEOUT;
    case ZirconRebootReason::kBrownout:
      return fuchsia::feedback::RebootReason::BROWNOUT;
    case ZirconRebootReason::kUnknown:
      return fuchsia::feedback::RebootReason::BRIEF_POWER_LOSS;
    case ZirconRebootReason::kRootJobTermination:
      return fuchsia::feedback::RebootReason::ROOT_JOB_TERMINATION;
    case ZirconRebootReason::kNotParseable:
      return std::nullopt;
    case ZirconRebootReason::kNoCrash:
    case ZirconRebootReason::kNotSet:
      FX_LOGS(FATAL) << "FinalZirconShutdownInfo shouldn't be constructed with reason: "
                     << ToRebootReasonString();
      return std::nullopt;
  }
}

std::optional<fuchsia::feedback::RebootReason> FinalGracefulShutdownInfo::ToFidlRebootReason()
    const {
  switch (final_reason_) {
    case FinalGracefulShutdownReason::kGenericGraceful:
    case FinalGracefulShutdownReason::kUnexpectedReasonGraceful:
      return std::nullopt;
    case FinalGracefulShutdownReason::kUserRequest:
      return fuchsia::feedback::RebootReason::USER_REQUEST;
    case FinalGracefulShutdownReason::kSystemUpdate:
      return fuchsia::feedback::RebootReason::SYSTEM_UPDATE;
    case FinalGracefulShutdownReason::kRetrySystemUpdate:
      return fuchsia::feedback::RebootReason::RETRY_SYSTEM_UPDATE;
    case FinalGracefulShutdownReason::kHighTemperature:
      return fuchsia::feedback::RebootReason::HIGH_TEMPERATURE;
    case FinalGracefulShutdownReason::kSessionFailure:
      return fuchsia::feedback::RebootReason::SESSION_FAILURE;
    case FinalGracefulShutdownReason::kSysmgrFailure:
      return fuchsia::feedback::RebootReason::SYSMGR_FAILURE;
    case FinalGracefulShutdownReason::kCriticalComponentFailure:
      return fuchsia::feedback::RebootReason::CRITICAL_COMPONENT_FAILURE;
    case FinalGracefulShutdownReason::kFdr:
      return fuchsia::feedback::RebootReason::FACTORY_DATA_RESET;
    case FinalGracefulShutdownReason::kZbiSwap:
      return fuchsia::feedback::RebootReason::ZBI_SWAP;
    case FinalGracefulShutdownReason::kOutOfMemory:
      return fuchsia::feedback::RebootReason::SYSTEM_OUT_OF_MEMORY;
    case FinalGracefulShutdownReason::kNetstackMigration:
      return fuchsia::feedback::RebootReason::NETSTACK_MIGRATION;
    case FinalGracefulShutdownReason::kAndroidUnexpectedReason:
      return fuchsia::feedback::RebootReason::ANDROID_UNEXPECTED_REASON;
    case FinalGracefulShutdownReason::kAndroidRescueParty:
      return fuchsia::feedback::RebootReason::ANDROID_RESCUE_PARTY;
    case FinalGracefulShutdownReason::kAndroidCriticalProcessFailure:
      return fuchsia::feedback::RebootReason::ANDROID_CRITICAL_PROCESS_FAILURE;
    case FinalGracefulShutdownReason::kDeveloperRequest:
      return fuchsia::feedback::RebootReason::DEVELOPER_REQUEST;
  }
}

cobalt::LastRebootReason FinalZirconShutdownInfo::ToCobaltLastRebootReason() const {
  switch (zircon_reason_) {
    case ZirconRebootReason::kCold:
      return cobalt::LastRebootReason::kCold;
    case ZirconRebootReason::kKernelPanic:
      return cobalt::LastRebootReason::kKernelPanic;
    case ZirconRebootReason::kOOM:
      return cobalt::LastRebootReason::kSystemOutOfMemory;
    case ZirconRebootReason::kHwWatchdog:
      return cobalt::LastRebootReason::kHardwareWatchdogTimeout;
    case ZirconRebootReason::kSwWatchdog:
      return cobalt::LastRebootReason::kSoftwareWatchdogTimeout;
    case ZirconRebootReason::kBrownout:
      return cobalt::LastRebootReason::kBrownout;
    case ZirconRebootReason::kUnknown:
      return cobalt::LastRebootReason::kBriefPowerLoss;
    case ZirconRebootReason::kRootJobTermination:
      return cobalt::LastRebootReason::kRootJobTermination;
    case ZirconRebootReason::kNotParseable:
      return cobalt::LastRebootReason::kUnknown;
    case ZirconRebootReason::kNoCrash:
    case ZirconRebootReason::kNotSet:
      FX_LOGS(FATAL) << "FinalZirconShutdownInfo shouldn't be constructed with reason: "
                     << ToRebootReasonString();
      return cobalt::LastRebootReason::kUnknown;
  }
}

cobalt::LastRebootReason FinalGracefulShutdownInfo::ToCobaltLastRebootReason() const {
  switch (final_reason_) {
    case FinalGracefulShutdownReason::kGenericGraceful:
      return cobalt::LastRebootReason::kGenericGraceful;
    case FinalGracefulShutdownReason::kUnexpectedReasonGraceful:
      return cobalt::LastRebootReason::kUnexpectedReasonGraceful;
    case FinalGracefulShutdownReason::kUserRequest:
      return cobalt::LastRebootReason::kUserRequest;
    case FinalGracefulShutdownReason::kSystemUpdate:
      return cobalt::LastRebootReason::kSystemUpdate;
    case FinalGracefulShutdownReason::kRetrySystemUpdate:
      return cobalt::LastRebootReason::kRetrySystemUpdate;
    case FinalGracefulShutdownReason::kHighTemperature:
      return cobalt::LastRebootReason::kHighTemperature;
    case FinalGracefulShutdownReason::kSessionFailure:
      return cobalt::LastRebootReason::kSessionFailure;
    case FinalGracefulShutdownReason::kSysmgrFailure:
      return cobalt::LastRebootReason::kSysmgrFailure;
    case FinalGracefulShutdownReason::kCriticalComponentFailure:
      return cobalt::LastRebootReason::kCriticalComponentFailure;
    case FinalGracefulShutdownReason::kFdr:
      return cobalt::LastRebootReason::kFactoryDataReset;
    case FinalGracefulShutdownReason::kZbiSwap:
      return cobalt::LastRebootReason::kZbiSwap;
    case FinalGracefulShutdownReason::kOutOfMemory:
      return cobalt::LastRebootReason::kSystemOutOfMemory;
    case FinalGracefulShutdownReason::kNetstackMigration:
      return cobalt::LastRebootReason::kNetstackMigration;
    case FinalGracefulShutdownReason::kAndroidUnexpectedReason:
      return cobalt::LastRebootReason::kAndroidUnexpectedReason;
    case FinalGracefulShutdownReason::kAndroidRescueParty:
      return cobalt::LastRebootReason::kAndroidRescueParty;
    case FinalGracefulShutdownReason::kAndroidCriticalProcessFailure:
      return cobalt::LastRebootReason::kAndroidCriticalProcessFailure;
    case FinalGracefulShutdownReason::kDeveloperRequest:
      return cobalt::LastRebootReason::kDeveloperRequest;
  }
}

std::string FinalZirconShutdownInfo::ToCrashProgramName() const {
  switch (zircon_reason_) {
    case ZirconRebootReason::kNotParseable:
      return "reboot-log";
    case ZirconRebootReason::kKernelPanic:
      return "kernel";
    case ZirconRebootReason::kHwWatchdog:
    case ZirconRebootReason::kBrownout:
    case ZirconRebootReason::kUnknown:
      return "device";
    case ZirconRebootReason::kOOM:
    case ZirconRebootReason::kSwWatchdog:
    case ZirconRebootReason::kRootJobTermination:
      return "system";
    case ZirconRebootReason::kCold:
      FX_LOGS(FATAL) << "Not expecting a program name request for cold boot";
      return "FATAL ERROR";
    case ZirconRebootReason::kNoCrash:
    case ZirconRebootReason::kNotSet:
      FX_LOGS(FATAL) << "FinalZirconShutdownInfo shouldn't be constructed with reason: "
                     << ToRebootReasonString();
      return "FATAL ERROR";
  }
}

std::string FinalGracefulShutdownInfo::ToCrashProgramName() const {
  switch (final_reason_) {
    case FinalGracefulShutdownReason::kGenericGraceful:
    case FinalGracefulShutdownReason::kUnexpectedReasonGraceful:
    case FinalGracefulShutdownReason::kRetrySystemUpdate:
    case FinalGracefulShutdownReason::kHighTemperature:
    case FinalGracefulShutdownReason::kSessionFailure:
    case FinalGracefulShutdownReason::kSysmgrFailure:
    case FinalGracefulShutdownReason::kCriticalComponentFailure:
    case FinalGracefulShutdownReason::kOutOfMemory:
      return "system";
    case FinalGracefulShutdownReason::kAndroidUnexpectedReason:
    case FinalGracefulShutdownReason::kAndroidRescueParty:
    case FinalGracefulShutdownReason::kAndroidCriticalProcessFailure:
      return "android";
    case FinalGracefulShutdownReason::kUserRequest:
    case FinalGracefulShutdownReason::kDeveloperRequest:
    case FinalGracefulShutdownReason::kSystemUpdate:
    case FinalGracefulShutdownReason::kNetstackMigration:
    case FinalGracefulShutdownReason::kZbiSwap:
    case FinalGracefulShutdownReason::kFdr:
      FX_LOGS(FATAL) << "Not expecting a program name request for reboot reason: "
                     << ToRebootReasonString();
      return "FATAL ERROR";
  }
}

std::string FinalZirconShutdownInfo::ToCrashSignature(
    const SpontaneousRebootReason spontaneous_reboot_reason,
    const std::optional<std::string>& critical_process) const {
  switch (zircon_reason_) {
    case ZirconRebootReason::kNotParseable:
      return "fuchsia-reboot-log-not-parseable";
    case ZirconRebootReason::kUnknown:
      return GetSpontaneousRebootCrashSignature(spontaneous_reboot_reason);
    case ZirconRebootReason::kKernelPanic:
      return "fuchsia-kernel-panic";
    case ZirconRebootReason::kOOM:
      return "fuchsia-oom";
    case ZirconRebootReason::kHwWatchdog:
      return "fuchsia-hw-watchdog-timeout";
    case ZirconRebootReason::kSwWatchdog:
      return "fuchsia-sw-watchdog-timeout";
    case ZirconRebootReason::kBrownout:
      return "fuchsia-brownout";
    case ZirconRebootReason::kRootJobTermination:
      return (!critical_process.has_value())
                 ? "fuchsia-root-job-termination"
                 : std::string("fuchsia-reboot-").append(*critical_process).append("-terminated");
    case ZirconRebootReason::kCold:
      FX_LOGS(FATAL) << "Not expecting a crash for reason: kCold";
      return "FATAL ERROR";
    case ZirconRebootReason::kNoCrash:
    case ZirconRebootReason::kNotSet:
      FX_LOGS(FATAL) << "FinalZirconShutdownInfo shouldn't be constructed with reason: "
                     << ToRebootReasonString();
      return "FATAL ERROR";
  }
}

std::string FinalGracefulShutdownInfo::ToCrashSignature(
    const SpontaneousRebootReason spontaneous_reboot_reason,
    const std::optional<std::string>& critical_process) const {
  switch (final_reason_) {
    case FinalGracefulShutdownReason::kRetrySystemUpdate:
      return "fuchsia-retry-system-update";
    case FinalGracefulShutdownReason::kHighTemperature:
      return "fuchsia-reboot-high-temperature";
    case FinalGracefulShutdownReason::kSessionFailure:
      return "fuchsia-session-failure";
    case FinalGracefulShutdownReason::kSysmgrFailure:
      return "fuchsia-sysmgr-failure";
    case FinalGracefulShutdownReason::kCriticalComponentFailure:
      return "fuchsia-critical-component-failure";
    case FinalGracefulShutdownReason::kOutOfMemory:
      return "fuchsia-oom";
    case FinalGracefulShutdownReason::kAndroidUnexpectedReason:
      return "fuchsia-reboot-android-unexpected-reason";
    case FinalGracefulShutdownReason::kAndroidRescueParty:
      return "fuchsia-reboot-android-rescue-party";
    case FinalGracefulShutdownReason::kAndroidCriticalProcessFailure:
      return "fuchsia-reboot-android-critical-process-failure";
    case FinalGracefulShutdownReason::kGenericGraceful:
      return "fuchsia-undetermined-userspace-reboot";
    case FinalGracefulShutdownReason::kUnexpectedReasonGraceful:
      return "fuchsia-unexpected-reason-userspace-reboot";
    case FinalGracefulShutdownReason::kUserRequest:
    case FinalGracefulShutdownReason::kSystemUpdate:
    case FinalGracefulShutdownReason::kFdr:
    case FinalGracefulShutdownReason::kZbiSwap:
    case FinalGracefulShutdownReason::kNetstackMigration:
    case FinalGracefulShutdownReason::kDeveloperRequest:
      FX_LOGS(FATAL) << "Not expecting a crash for reason: " << ToRebootReasonString();
      return "FATAL ERROR";
  }
}

FinalGracefulShutdownInfo::FinalGracefulShutdownReason
FinalGracefulShutdownInfo::ConsolidateGracefulShutdownReasons(
    const std::vector<GracefulShutdownReason>& reasons) const {
  if (!not_a_fdr_) {
    return FinalGracefulShutdownReason::kFdr;
  }

  if (reasons.empty()) {
    return FinalGracefulShutdownReason::kGenericGraceful;
  }

  // If there's only one reason, consolidation is trivial.
  if (reasons.size() == 1) {
    switch (reasons[0]) {
      case GracefulShutdownReason::kUserRequest:
        return FinalGracefulShutdownReason::kUserRequest;
      case GracefulShutdownReason::kSystemUpdate:
        return FinalGracefulShutdownReason::kSystemUpdate;
      case GracefulShutdownReason::kRetrySystemUpdate:
        return FinalGracefulShutdownReason::kRetrySystemUpdate;
      case GracefulShutdownReason::kHighTemperature:
        return FinalGracefulShutdownReason::kHighTemperature;
      case GracefulShutdownReason::kSessionFailure:
        return FinalGracefulShutdownReason::kSessionFailure;
      case GracefulShutdownReason::kSysmgrFailure:
        return FinalGracefulShutdownReason::kSysmgrFailure;
      case GracefulShutdownReason::kCriticalComponentFailure:
        return FinalGracefulShutdownReason::kCriticalComponentFailure;
      case GracefulShutdownReason::kFdr:
        return FinalGracefulShutdownReason::kFdr;
      case GracefulShutdownReason::kZbiSwap:
        return FinalGracefulShutdownReason::kZbiSwap;
      case GracefulShutdownReason::kNotSupported:
      case GracefulShutdownReason::kNotParseable:
        return FinalGracefulShutdownReason::kGenericGraceful;
      case GracefulShutdownReason::kOutOfMemory:
        return FinalGracefulShutdownReason::kOutOfMemory;
      case GracefulShutdownReason::kNetstackMigration:
        return FinalGracefulShutdownReason::kNetstackMigration;
      case GracefulShutdownReason::kAndroidUnexpectedReason:
        return FinalGracefulShutdownReason::kAndroidUnexpectedReason;
      case GracefulShutdownReason::kAndroidRescueParty:
        return FinalGracefulShutdownReason::kAndroidRescueParty;
      case GracefulShutdownReason::kAndroidCriticalProcessFailure:
        return FinalGracefulShutdownReason::kAndroidCriticalProcessFailure;
      case GracefulShutdownReason::kDeveloperRequest:
        return FinalGracefulShutdownReason::kDeveloperRequest;
      case GracefulShutdownReason::kNotSet:
        FX_LOGS(FATAL) << "Graceful shutdown reason must be set";
        return FinalGracefulShutdownReason::kUnexpectedReasonGraceful;
    }
  }

  // Otherwise, verify it's an expected combination of reasons.
  std::unordered_set<GracefulShutdownReason> reasons_set(reasons.begin(), reasons.end());
  if (reasons_set.size() == 2 && reasons_set.contains(GracefulShutdownReason::kNetstackMigration) &&
      reasons_set.contains(GracefulShutdownReason::kSystemUpdate)) {
    // Netstack Migration + System Update is consolidated to System Update.
    return FinalGracefulShutdownReason::kSystemUpdate;
  }

  FX_LOGS(WARNING) << "Unexpected combination of graceful shutdown reasons: "
                   << ToRawStrings(reasons);
  return FinalGracefulShutdownReason::kUnexpectedReasonGraceful;
}

}  // namespace forensics::feedback
