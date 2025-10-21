// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/reboot_log/graceful_shutdown_info.h"

#include <fcntl.h>
#include <lib/syslog/cpp/macros.h>

#include <string>

#include <fbl/unique_fd.h>

#include "src/lib/files/file_descriptor.h"
#include "src/lib/fxl/strings/join_strings.h"
#include "src/lib/fxl/strings/split_string.h"
#include "third_party/rapidjson/include/rapidjson/document.h"
#include "third_party/rapidjson/include/rapidjson/error/en.h"
#include "third_party/rapidjson/include/rapidjson/prettywriter.h"
#include "third_party/rapidjson/include/rapidjson/schema.h"

namespace forensics {
namespace feedback {
namespace {

constexpr char kActionPoweroff[] = "POWEROFF";
constexpr char kActionReboot[] = "REBOOT";
constexpr char kActionRebootToRecovery[] = "REBOOT_TO_RECOVERY";
constexpr char kActionRebootToBootloader[] = "REBOOT_TO_BOOTLOADER";

constexpr char kReasonNotSet[] = "NOT SET";
constexpr char kReasonUserRequest[] = "USER REQUEST";
constexpr char kReasonSystemUpdate[] = "SYSTEM UPDATE";
constexpr char kReasonRetrySystemUpdate[] = "RETRY SYSTEM UPDATE";
constexpr char kReasonHighTemperature[] = "HIGH TEMPERATURE";
constexpr char kReasonSessionFailure[] = "SESSION FAILURE";
constexpr char kReasonSysmgrFailure[] = "SYSMGR FAILURE";
constexpr char kReasonCriticalComponentFailure[] = "CRITICAL COMPONENT FAILURE";
constexpr char kReasonFdr[] = "FACTORY DATA RESET";
constexpr char kReasonZbiSwap[] = "ZBI SWAP";
constexpr char kOutOfMemory[] = "OUT OF MEMORY";
constexpr char kReasonNetstackMigration[] = "NETSTACK MIGRATION";
constexpr char kAndroidUnexpectedReason[] = "ANDROID UNEXPECTED REASON";
constexpr char kAndroidRescueParty[] = "ANDROID RESCUE PARTY";
constexpr char kAndroidCriticalProcessFailure[] = "ANDROID CRITICAL PROCESS FAILURE";
constexpr char kDeveloperRequest[] = "DEVELOPER REQUEST";
constexpr char kReasonNotSupported[] = "NOT SUPPORTED";
constexpr char kReasonNotParseable[] = "NOT PARSEABLE";

// Used to separate multiple `GracefulShutdownReasons` when written to file.
constexpr char kDeliminator[] = ",";

// Used to represent the absence of a `GracefulShutdownReasons` when written to
// file. Unlike the other "reason" strings above, this is translated to an
// empty vector rather than a `GracefulShutdownReasons`, when read from file.
constexpr char kNoReasons[] = "NONE";

constexpr char kActionKey[] = "action";
constexpr char kReasonsKey[] = "reasons";

// This schema cannot become more strict over time because future versions of Fuchsia may read json
// written by a previous version. For example, we cannot add any more required fields because
// "reasons" is the only field that will be written by the original version of Fuchsia that persists
// this file to disk.
constexpr char kJsonSchema[] = R"({
  "type": "object",
  "properties": {
    "action": {
      "type": "string",
      "enum": [
          "POWEROFF",
          "REBOOT",
          "REBOOT_TO_RECOVERY",
          "REBOOT_TO_BOOTLOADER"
        ]
    },
    "reasons": {
      "type": "array",
      "items": {
        "type": "string"
      }
    }
  },
  "required": [
    "reasons"
  ],
  "additionalProperties": false
})";

bool IsSchemaValid(const rapidjson::Document& json) {
  rapidjson::Document schema;
  if (const rapidjson::ParseResult result = schema.Parse(kJsonSchema); !result) {
    FX_LOGS(ERROR) << "Error parsing shutdown info schema at offset " << result.Offset() << " "
                   << rapidjson::GetParseError_En(result.Code());
    return false;
  }

  rapidjson::SchemaDocument schema_doc(schema);
  if (rapidjson::SchemaValidator validator(schema_doc); !json.Accept(validator)) {
    rapidjson::StringBuffer buf;
    validator.GetInvalidSchemaPointer().StringifyUriFragment(buf);
    FX_LOGS(ERROR) << "Shutdown info json does not match schema, violating '"
                   << validator.GetInvalidSchemaKeyword() << "' rule";
    return false;
  }

  return true;
}

GracefulShutdownAction GracefulShutdownActionFromString(const std::string_view action) {
  if (action == kActionPoweroff) {
    return GracefulShutdownAction::kPoweroff;
  }
  if (action == kActionReboot) {
    return GracefulShutdownAction::kReboot;
  }
  if (action == kActionRebootToRecovery) {
    return GracefulShutdownAction::kRebootToRecovery;
  }
  if (action == kActionRebootToBootloader) {
    return GracefulShutdownAction::kRebootToBootloader;
  }

  FX_LOGS(ERROR) << "Invalid persisted graceful shutdown action: " << action;
  return GracefulShutdownAction::kNotParseable;
}

}  // namespace

std::string ToString(const GracefulShutdownReason reason) {
  switch (reason) {
    case GracefulShutdownReason::kNotSet:
      return kReasonNotSet;
    case GracefulShutdownReason::kUserRequest:
      return kReasonUserRequest;
    case GracefulShutdownReason::kSystemUpdate:
      return kReasonSystemUpdate;
    case GracefulShutdownReason::kRetrySystemUpdate:
      return kReasonRetrySystemUpdate;
    case GracefulShutdownReason::kHighTemperature:
      return kReasonHighTemperature;
    case GracefulShutdownReason::kSessionFailure:
      return kReasonSessionFailure;
    case GracefulShutdownReason::kSysmgrFailure:
      return kReasonSysmgrFailure;
    case GracefulShutdownReason::kCriticalComponentFailure:
      return kReasonCriticalComponentFailure;
    case GracefulShutdownReason::kFdr:
      return kReasonFdr;
    case GracefulShutdownReason::kZbiSwap:
      return kReasonZbiSwap;
    case GracefulShutdownReason::kOutOfMemory:
      return kOutOfMemory;
    case GracefulShutdownReason::kNetstackMigration:
      return kReasonNetstackMigration;
    case GracefulShutdownReason::kAndroidUnexpectedReason:
      return kAndroidUnexpectedReason;
    case GracefulShutdownReason::kAndroidRescueParty:
      return kAndroidRescueParty;
    case GracefulShutdownReason::kAndroidCriticalProcessFailure:
      return kAndroidCriticalProcessFailure;
    case GracefulShutdownReason::kDeveloperRequest:
      return kDeveloperRequest;
    case GracefulShutdownReason::kNotSupported:
      return kReasonNotSupported;
    case GracefulShutdownReason::kNotParseable:
      return kReasonNotParseable;
  }

  return kReasonNotSet;
}

GracefulShutdownReason GracefulShutdownReasonFromString(const std::string_view reason) {
  if (reason == kReasonUserRequest) {
    return GracefulShutdownReason::kUserRequest;
  } else if (reason == kReasonSystemUpdate) {
    return GracefulShutdownReason::kSystemUpdate;
  } else if (reason == kReasonRetrySystemUpdate) {
    return GracefulShutdownReason::kRetrySystemUpdate;
  } else if (reason == kReasonHighTemperature) {
    return GracefulShutdownReason::kHighTemperature;
  } else if (reason == kReasonSessionFailure) {
    return GracefulShutdownReason::kSessionFailure;
  } else if (reason == kReasonSysmgrFailure) {
    return GracefulShutdownReason::kSysmgrFailure;
  } else if (reason == kReasonCriticalComponentFailure) {
    return GracefulShutdownReason::kCriticalComponentFailure;
  } else if (reason == kReasonFdr) {
    return GracefulShutdownReason::kFdr;
  } else if (reason == kReasonZbiSwap) {
    return GracefulShutdownReason::kZbiSwap;
  } else if (reason == kReasonNetstackMigration) {
    return GracefulShutdownReason::kNetstackMigration;
  } else if (reason == kAndroidUnexpectedReason) {
    return GracefulShutdownReason::kAndroidUnexpectedReason;
  } else if (reason == kAndroidRescueParty) {
    return GracefulShutdownReason::kAndroidRescueParty;
  } else if (reason == kAndroidCriticalProcessFailure) {
    return GracefulShutdownReason::kAndroidCriticalProcessFailure;
  } else if (reason == kDeveloperRequest) {
    return GracefulShutdownReason::kDeveloperRequest;
  } else if (reason == kReasonNotSupported) {
    return GracefulShutdownReason::kNotSupported;
  } else if (reason == kOutOfMemory) {
    return GracefulShutdownReason::kOutOfMemory;
  }

  FX_LOGS(ERROR) << "Invalid persisted graceful shutdown reason: " << reason;
  return GracefulShutdownReason::kNotParseable;
}

// Converts the list of `GracefulShutdownReasonss` into a single string.
//
// The format is:
// "Reason 1,Reason 2,Reason 3"
std::string ToLegacyFileContentForTesting(const std::vector<GracefulShutdownReason>& reasons) {
  if (reasons.empty()) {
    return kNoReasons;
  }

  return fxl::JoinStrings(ToReasonStrings(reasons), ",");
}

std::vector<std::string> ToReasonStrings(const std::vector<GracefulShutdownReason>& reasons) {
  if (reasons.empty()) {
    return {};
  }
  std::vector<std::string> reason_strings;
  reason_strings.reserve(reasons.size());
  for (const auto& reason : reasons) {
    std::string reason_string;
    switch (reason) {
      case GracefulShutdownReason::kUserRequest:
      case GracefulShutdownReason::kSystemUpdate:
      case GracefulShutdownReason::kRetrySystemUpdate:
      case GracefulShutdownReason::kHighTemperature:
      case GracefulShutdownReason::kSessionFailure:
      case GracefulShutdownReason::kSysmgrFailure:
      case GracefulShutdownReason::kCriticalComponentFailure:
      case GracefulShutdownReason::kFdr:
      case GracefulShutdownReason::kZbiSwap:
      case GracefulShutdownReason::kOutOfMemory:
      case GracefulShutdownReason::kNetstackMigration:
      case GracefulShutdownReason::kAndroidUnexpectedReason:
      case GracefulShutdownReason::kAndroidRescueParty:
      case GracefulShutdownReason::kAndroidCriticalProcessFailure:
      case GracefulShutdownReason::kDeveloperRequest:
      case GracefulShutdownReason::kNotSupported:
        reason_string = ToString(reason);
        break;
      case GracefulShutdownReason::kNotSet:
      case GracefulShutdownReason::kNotParseable:
        FX_LOGS(ERROR) << "Invalid persisted graceful shutdown reason: " << ToString(reason);
        reason_string = kReasonNotSupported;
        break;
    }
    if (reason_string.empty()) {
      // The reason was out of the valid bounds of a `GracefulShutdownReasons`
      // (None of the switch cases above applied).
      reason_string = kReasonNotSupported;
    }

    reason_strings.push_back(reason_string);
  }

  return reason_strings;
}

std::string ToJson(const std::vector<GracefulShutdownReason>& reasons) {
  rapidjson::Document json;
  json.SetObject();
  auto& allocator = json.GetAllocator();

  rapidjson::Value json_reasons(rapidjson::kArrayType);
  const std::vector<std::string> reason_strings = ToReasonStrings(reasons);

  for (const std::string& reason : reason_strings) {
    json_reasons.PushBack(rapidjson::Value(reason, allocator), allocator);
  }
  json.AddMember(kReasonsKey, json_reasons, allocator);

  if (!IsSchemaValid(json)) {
    FX_LOGS(ERROR) << "Failed to create json matching the schema.";
    return "";
  }

  rapidjson::StringBuffer buffer;
  rapidjson::PrettyWriter<rapidjson::StringBuffer> writer(buffer);
  json.Accept(writer);

  return buffer.GetString();
}

std::string ToRawStrings(const std::vector<GracefulShutdownReason>& reasons) {
  if (reasons.empty()) {
    return kNoReasons;
  }
  std::vector<std::string> reason_strings;
  reason_strings.reserve(reasons.size());
  for (const auto& reason : reasons) {
    reason_strings.push_back(ToString(reason));
  }
  return fxl::JoinStrings(reason_strings, kDeliminator);
}

// Converts the file contents into a list of `GracefulShutdownReasons`.
//
// The expected format is:
// "Reason 1,Reason 2,Reason 3"
//
// If the given string is empty, the returned list will be empty.
std::vector<GracefulShutdownReason> FromLegacyTxtFile(const std::string reasons) {
  if (reasons == kNoReasons) {
    return {};
  }

  const std::vector<std::string_view> reason_strings =
      fxl::SplitString(reasons, kDeliminator, fxl::WhiteSpaceHandling::kTrimWhitespace,
                       fxl::SplitResult::kSplitWantNonEmpty);
  std::vector<GracefulShutdownReason> graceful_reasons;
  graceful_reasons.reserve(reason_strings.size());
  for (const auto& reason : reason_strings) {
    graceful_reasons.push_back(GracefulShutdownReasonFromString(reason));
  }
  return graceful_reasons;
}

GracefulShutdownInfo FromJson(const std::string& content) {
  rapidjson::Document json;
  if (const rapidjson::ParseResult result = json.Parse(content.c_str()); !result) {
    FX_LOGS(ERROR) << "Error parsing shutdown info as JSON at offset " << result.Offset() << " "
                   << rapidjson::GetParseError_En(result.Code());
    return GracefulShutdownInfo{
        .action = GracefulShutdownAction::kNotParseable,
        .reasons = {GracefulShutdownReason::kNotParseable},
    };
  }

  if (!IsSchemaValid(json)) {
    FX_LOGS(ERROR) << "Failed to parse content: " << content;
    return GracefulShutdownInfo{
        .action = GracefulShutdownAction::kNotParseable,
        .reasons = {GracefulShutdownReason::kNotParseable},
    };
  }

  rapidjson::Document schema;
  if (const rapidjson::ParseResult result = schema.Parse(kJsonSchema); !result) {
    FX_LOGS(ERROR) << "Error parsing shutdown info schema at offset " << result.Offset() << " "
                   << rapidjson::GetParseError_En(result.Code());
    return GracefulShutdownInfo{
        .action = GracefulShutdownAction::kNotParseable,
        .reasons = {GracefulShutdownReason::kNotParseable},
    };
  }

  rapidjson::SchemaDocument schema_doc(schema);
  if (rapidjson::SchemaValidator validator(schema_doc); !json.Accept(validator)) {
    rapidjson::StringBuffer buf;
    validator.GetInvalidSchemaPointer().StringifyUriFragment(buf);
    FX_LOGS(ERROR) << "Shutdown info json does not match schema, violating '"
                   << validator.GetInvalidSchemaKeyword() << "' rule";
    return GracefulShutdownInfo{
        .action = GracefulShutdownAction::kNotParseable,
        .reasons = {GracefulShutdownReason::kNotParseable},
    };
  }

  GracefulShutdownInfo shutdown_info;
  if (json.HasMember(kActionKey) && json[kActionKey].IsString()) {
    shutdown_info.action = GracefulShutdownActionFromString(json[kActionKey].GetString());
  } else {
    // The fuchsia.hardware.power.statecontrol/Admin Shutdown method requires that clients supply an
    // action. If the file is present but the action is missing, that means that the build
    // persisting the json was new enough to be persisting the reasons as json, but not new enough
    // to be persisting the action as well. During this timeframe, only 'REBOOT' actions were
    // triggering the ShutdownWatcher protocol so we can infer that the action in this case is
    // 'REBOOT.'
    shutdown_info.action = GracefulShutdownAction::kReboot;
  }

  for (const auto& k : json[kReasonsKey].GetArray()) {
    shutdown_info.reasons.push_back(GracefulShutdownReasonFromString(k.GetString()));
  }

  return shutdown_info;
}

GracefulShutdownReason FromReason(
    const fuchsia::hardware::power::statecontrol::ShutdownReason& reason) {
  using fuchsia::hardware::power::statecontrol::ShutdownReason;
  switch (reason) {
    case ShutdownReason::USER_REQUEST:
      return GracefulShutdownReason::kUserRequest;
    case ShutdownReason::SYSTEM_UPDATE:
      return GracefulShutdownReason::kSystemUpdate;
    case ShutdownReason::RETRY_SYSTEM_UPDATE:
      return GracefulShutdownReason::kRetrySystemUpdate;
    case ShutdownReason::HIGH_TEMPERATURE:
      return GracefulShutdownReason::kHighTemperature;
    case ShutdownReason::SESSION_FAILURE:
      return GracefulShutdownReason::kSessionFailure;
    case ShutdownReason::CRITICAL_COMPONENT_FAILURE:
      return GracefulShutdownReason::kCriticalComponentFailure;
    case ShutdownReason::FACTORY_DATA_RESET:
      return GracefulShutdownReason::kFdr;
    case ShutdownReason::ZBI_SWAP:
      return GracefulShutdownReason::kZbiSwap;
    case ShutdownReason::OUT_OF_MEMORY:
      return GracefulShutdownReason::kOutOfMemory;
    case ShutdownReason::NETSTACK_MIGRATION:
      return GracefulShutdownReason::kNetstackMigration;
    case ShutdownReason::ANDROID_UNEXPECTED_REASON:
      return GracefulShutdownReason::kAndroidUnexpectedReason;
    case ShutdownReason::ANDROID_RESCUE_PARTY:
      return GracefulShutdownReason::kAndroidRescueParty;
    case ShutdownReason::ANDROID_CRITICAL_PROCESS_FAILURE:
      return GracefulShutdownReason::kAndroidCriticalProcessFailure;
    case ShutdownReason::DEVELOPER_REQUEST:
      return GracefulShutdownReason::kDeveloperRequest;
    default:
      return GracefulShutdownReason::kNotSupported;
  }
}

std::vector<GracefulShutdownReason> ToGracefulShutdownReasons(
    const fuchsia::hardware::power::statecontrol::ShutdownOptions options) {
  if (!options.has_reasons()) {
    return {};
  }

  std::vector<GracefulShutdownReason> reasons;
  reasons.reserve(options.reasons().size());
  for (const auto& reason : options.reasons()) {
    reasons.push_back(FromReason(reason));
  }
  return reasons;
}

void WriteGracefulShutdownInfo(const std::vector<GracefulShutdownReason>& reasons,
                               cobalt::Logger* cobalt, const std::string& path) {
  fbl::unique_fd fd(open(path.c_str(), O_CREAT | O_TRUNC | O_WRONLY, S_IRUSR | S_IWUSR));
  if (!fd.is_valid()) {
    FX_LOGS(INFO) << "Failed to open shutdown info file: " << path;
    return;
  }

  if (const std::string content = ToJson(reasons);
      !fxl::WriteFileDescriptor(fd.get(), content.data(), content.size())) {
    FX_LOGS(ERROR) << "Failed to write shutdown info to: " << path;
  }

  // Force the flush as we want to persist the content asap and we don't have more content to
  // write.
  fsync(fd.get());
}

}  // namespace feedback
}  // namespace forensics
