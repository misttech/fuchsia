// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/diagnostics/fidl_log_message_parser/test_support.h"

#include <lib/syslog/cpp/log_level.h>

#include <algorithm>
#include <cstring>
#include <optional>
#include <sstream>

#include <rapidjson/document.h>
#include <rapidjson/error/en.h>
#include <rapidjson/pointer.h>

#include "src/lib/diagnostics/fidl_log_message_parser/log_message.h"
#include "src/lib/fsl/vmo/strings.h"

namespace diagnostics::fidl_log_message_parser::testing {

namespace {

const char kPidLabel[] = "pid";
const char kTidLabel[] = "tid";
const char kFileLabel[] = "file";
const char kLineLabel[] = "line";
const char kTagsLabel[] = "tags";
const char kMessageLabel[] = "value";
const char kVerbosityLabel[] = "verbosity";

inline fuchsia::logger::LogLevelFilter StringToSeverity(const std::string& input) {
  if (strcasecmp(input.c_str(), "trace") == 0) {
    return fuchsia::logger::LogLevelFilter::TRACE;
  }
  if (strcasecmp(input.c_str(), "debug") == 0) {
    return fuchsia::logger::LogLevelFilter::DEBUG;
  }
  if (strcasecmp(input.c_str(), "info") == 0) {
    return fuchsia::logger::LogLevelFilter::INFO;
  }
  if (strcasecmp(input.c_str(), "warn") == 0 || strcasecmp(input.c_str(), "warning") == 0) {
    return fuchsia::logger::LogLevelFilter::WARN;
  }
  if (strcasecmp(input.c_str(), "error") == 0) {
    return fuchsia::logger::LogLevelFilter::ERROR;
  }
  if (strcasecmp(input.c_str(), "fatal") == 0) {
    return fuchsia::logger::LogLevelFilter::FATAL;
  }

  return fuchsia::logger::LOG_LEVEL_DEFAULT;
}

std::string GetComponentName(const std::string& moniker) {
  size_t pos = moniker.rfind('/');
  if (pos == std::string::npos) {
    return moniker;
  }
  if (pos + 1 < moniker.size()) {
    return moniker.substr(pos + 1);
  }
  return moniker;
}

struct ParsedJsonLog {
  zx::basic_time<ZX_CLOCK_BOOT> timestamp{0};
  int32_t severity{static_cast<int32_t>(fuchsia::logger::LogLevelFilter::INFO)};
  uint64_t pid{0};
  uint64_t tid{0};
  std::vector<std::string> tags;
  uint32_t dropped_logs{0};
  std::string msg;
  std::string file;
  std::optional<uint64_t> line;
};

inline fpromise::result<ParsedJsonLog, std::string> ParseSingleJsonLog(rapidjson::Value& value) {
  ParsedJsonLog ret;
  std::stringstream kv_mapping;

  if (!value.IsObject()) {
    return fpromise::error("Value is not an object");
  }

  auto metadata = value.FindMember("metadata");
  auto payload = value.FindMember("payload");

  if (metadata == value.MemberEnd() || payload == value.MemberEnd() ||
      !metadata->value.IsObject() || !payload->value.IsObject()) {
    return fpromise::error("Expected metadata and payload objects");
  }
  auto root = payload->value.FindMember("root");
  if (root == payload->value.MemberEnd() || !root->value.IsObject()) {
    return fpromise::error("Expected payload.root to be an object if present");
  }

  auto timestamp = metadata->value.FindMember("timestamp");
  if (timestamp == metadata->value.MemberEnd() || !timestamp->value.IsUint64()) {
    return fpromise::error("Expected metadata.timestamp key");
  }
  ret.timestamp = zx::basic_time<ZX_CLOCK_BOOT>(timestamp->value.GetInt64());

  auto severity = metadata->value.FindMember("severity");
  if (severity == metadata->value.MemberEnd() || !severity->value.IsString()) {
    return fpromise::error("Expected metadata.severity key");
  }
  ret.severity = static_cast<int32_t>(StringToSeverity(severity->value.GetString()));

  auto moniker = value.FindMember("moniker");
  std::string moniker_string;
  if (moniker != value.MemberEnd() && moniker->value.IsString()) {
    moniker_string = moniker->value.GetString();
  }

  uint32_t dropped_logs = 0;
  if (metadata->value.HasMember("errors")) {
    auto& errors = metadata->value["errors"];
    if (errors.IsArray()) {
      for (rapidjson::SizeType i = 0; i < errors.Size(); i++) {
        auto* val = rapidjson::Pointer("/dropped_logs/count").Get(errors[i]);
        if (val && val->IsUint()) {
          dropped_logs += val->GetUint();
        }
      }
    }
  }

  if (payload->value.MemberCount() == 1 && payload->value.HasMember("root")) {
    auto root_member = payload->value.FindMember("root");
    if (root_member == payload->value.MemberEnd() || !root_member->value.IsObject()) {
      return fpromise::error("Expected payload.root to be an object if present");
    }
    auto msg_member = root_member->value.FindMember("message");
    if (msg_member == root_member->value.MemberEnd() || !msg_member->value.IsObject()) {
      return fpromise::error("Expected payload.root.message to be an object if present");
    }
    payload = msg_member;
  }

  std::string msg;
  std::optional<int> verbosity;

  for (auto it = payload->value.MemberBegin(); it != payload->value.MemberEnd(); ++it) {
    if (!it->name.IsString()) {
      return fpromise::error("A key is not a string");
    }
    std::string name = it->name.GetString();
    if (name == kMessageLabel && it->value.IsString()) {
      msg = it->value.GetString();
    }

    if ((name == kVerbosityLabel) && it->value.IsInt()) {
      verbosity = it->value.GetInt();
      if (ret.severity != verbosity.value_or(ret.severity)) {
        ret.severity = static_cast<int32_t>(fidl_log_message_parser::GetSeverityFromVerbosity(
            static_cast<uint8_t>(verbosity.value())));
      }
    }
  }

  for (auto it = metadata->value.MemberBegin(); it != metadata->value.MemberEnd(); ++it) {
    if (!it->name.IsString()) {
      return fpromise::error("A key is not a string");
    }
    std::string name = it->name.GetString();
    if (name == kTagsLabel) {
      if (it->value.IsString()) {
        ret.tags.emplace_back(it->value.GetString());
      } else if (it->value.IsArray()) {
        for (rapidjson::SizeType i = 0; i < it->value.Size(); ++i) {
          auto& val = it->value[i];
          if (!val.IsString()) {
            return fpromise::error("Tags array must contain strings");
          }
          ret.tags.emplace_back(val.GetString());
        }
      } else {
        return fpromise::error("Tags must be a string or array of strings");
      }
    } else if (name == kTidLabel && it->value.IsUint64()) {
      ret.tid = it->value.GetUint64();
    } else if (name == kPidLabel && it->value.IsUint64()) {
      ret.pid = it->value.GetUint64();
    } else if (name == kFileLabel && it->value.IsString()) {
      ret.file = it->value.GetString();
    } else if (name == kLineLabel && it->value.IsUint64()) {
      ret.line = it->value.GetUint64();
    }
  }

  auto kvps = root->value.FindMember("keys");
  if ((kvps != root->value.MemberEnd()) && kvps->value.IsObject() && (kvps->name == "keys")) {
    for (auto it = kvps->value.MemberBegin(); it != kvps->value.MemberEnd(); ++it) {
      if (!it->name.IsString()) {
        return fpromise::error("A key is not a string");
      }
      std::string name = it->name.GetString();
      kv_mapping << " " << std::move(name) << "=";
      if (it->value.IsInt64()) {
        kv_mapping << it->value.GetInt64();
      } else if (it->value.IsUint64()) {
        kv_mapping << it->value.GetUint64();
      } else if (it->value.IsDouble()) {
        kv_mapping << it->value.GetDouble();
      } else if (it->value.IsString()) {
        kv_mapping << "\"";
        auto str_value = it->value.GetString();
        if (strchr(str_value, '"') != nullptr) {
          size_t len = strlen(str_value);
          for (size_t i = 0; i < len; ++i) {
            char c = str_value[i];
            if (c == '"') {
              kv_mapping << '\\';
            }
            kv_mapping << c;
          }
        } else {
          kv_mapping << str_value;
        }
        kv_mapping << "\"";
      } else {
        kv_mapping << "<unknown>";
      }
    }
  }

  if (!ret.file.empty() && ret.line.has_value()) {
    std::stringstream enc;
    enc << "[" << ret.file << "(" << ret.line.value() << ")] ";
    ret.msg = enc.str();
  }

  ret.msg += msg;
  bool needs_add = true;

  if (!ret.msg.empty()) {
    if (ret.msg[ret.msg.size() - 1] == ' ') {
      std::string truncated = kv_mapping.str();
      truncated.erase(0, 1);
      ret.msg += truncated;
      needs_add = false;
    }
  }

  if (needs_add) {
    ret.msg += kv_mapping.str();
  }

  auto component_name = GetComponentName(moniker_string);
  if ((ret.tags.empty() && !component_name.empty()) ||
      (!ret.tags.empty() &&
       (std::find(ret.tags.begin(), ret.tags.end(), component_name) == ret.tags.end()) &&
       !component_name.empty())) {
    if (component_name != ".") {
      ret.tags.emplace(ret.tags.begin(), component_name);
    }
  }

  if (dropped_logs > 0) {
    ret.dropped_logs = dropped_logs;
  }

  return fpromise::ok(std::move(ret));
}

std::vector<uint8_t> BuildFxtRecordFromParsed(const ParsedJsonLog& parsed) {
  fuchsia_logging::LogBuffer buffer;
  buffer.BeginRecord(parsed.timestamp, static_cast<FuchsiaLogSeverity>(parsed.severity),
                     std::nullopt, 0, parsed.msg, 0, parsed.pid, parsed.tid);
  if (parsed.dropped_logs > 0) {
    buffer.WriteKeyValue("num_dropped", static_cast<uint64_t>(parsed.dropped_logs));
  }
  for (const auto& tag : parsed.tags) {
    buffer.WriteKeyValue("tag", tag);
  }
  auto span = buffer.EndRecord();
  return std::vector<uint8_t>(span.begin(), span.end());
}

}  // namespace

std::vector<uint8_t> BuildFxtRecord(zx::basic_time<ZX_CLOCK_BOOT> timestamp,
                                    FuchsiaLogSeverity severity, std::string_view message,
                                    zx_koid_t pid, zx_koid_t tid,
                                    const std::vector<std::string>& tags) {
  fuchsia_logging::LogBuffer buffer;
  buffer.BeginRecord(timestamp, severity, std::nullopt, 0, message, 0, pid, tid);
  for (const auto& tag : tags) {
    buffer.WriteKeyValue("tag", tag);
  }
  auto span = buffer.EndRecord();
  return std::vector<uint8_t>(span.begin(), span.end());
}

zx::vmo BuildFxtVmo(zx::basic_time<ZX_CLOCK_BOOT> timestamp, FuchsiaLogSeverity severity,
                    std::string_view message, zx_koid_t pid, zx_koid_t tid,
                    const std::vector<std::string>& tags) {
  auto bytes = BuildFxtRecord(timestamp, severity, message, pid, tid, tags);
  zx::vmo vmo;
  zx_status_t status = zx::vmo::create(bytes.size(), 0, &vmo);
  if (status != ZX_OK) {
    return zx::vmo();
  }
  status = vmo.write(bytes.data(), 0, bytes.size());
  if (status != ZX_OK) {
    return zx::vmo();
  }
  status = vmo.set_prop_content_size(bytes.size());
  if (status != ZX_OK) {
    return zx::vmo();
  }
  return vmo;
}

zx::vmo EncodeJsonLogToFxtVmo(std::string_view json_str) {
  rapidjson::Document doc;
  doc.Parse(json_str.data(), json_str.size());
  if (doc.HasParseError()) {
    return zx::vmo();
  }
  std::vector<uint8_t> fxt_bytes;
  if (doc.IsArray()) {
    for (rapidjson::SizeType i = 0; i < doc.Size(); ++i) {
      auto parsed = ParseSingleJsonLog(doc[i]);
      if (parsed.is_ok()) {
        auto record = BuildFxtRecordFromParsed(parsed.value());
        fxt_bytes.insert(fxt_bytes.end(), record.begin(), record.end());
      }
    }
  } else if (doc.IsObject()) {
    auto parsed = ParseSingleJsonLog(doc);
    if (parsed.is_ok()) {
      auto record = BuildFxtRecordFromParsed(parsed.value());
      fxt_bytes.insert(fxt_bytes.end(), record.begin(), record.end());
    }
  }
  if (fxt_bytes.empty()) {
    return zx::vmo();
  }
  zx::vmo vmo;
  zx_status_t status = zx::vmo::create(fxt_bytes.size(), 0, &vmo);
  if (status != ZX_OK) {
    return zx::vmo();
  }
  status = vmo.write(fxt_bytes.data(), 0, fxt_bytes.size());
  if (status != ZX_OK) {
    return zx::vmo();
  }
  status = vmo.set_prop_content_size(fxt_bytes.size());
  if (status != ZX_OK) {
    return zx::vmo();
  }
  return vmo;
}

std::vector<zx::vmo> EncodeJsonBatchToFxtVmos(const std::vector<std::string>& json_batch) {
  std::vector<zx::vmo> vmos;
  vmos.reserve(json_batch.size());
  for (const auto& json_str : json_batch) {
    auto vmo = EncodeJsonLogToFxtVmo(json_str);
    if (vmo.is_valid()) {
      vmos.push_back(std::move(vmo));
    }
  }
  return vmos;
}

fpromise::result<std::vector<fpromise::result<fuchsia::logger::LogMessage, std::string>>,
                 std::string>
ConvertFormattedContentForTest(fuchsia::diagnostics::FormattedContent content) {
  if (content.is_fxt()) {
    return fidl_log_message_parser::ConvertFormattedContentToLogMessages(std::move(content));
  }
  if (!content.is_json()) {
    return fpromise::error("Expected json or FXT content");
  }

  std::string data;
  if (!fsl::StringFromVmo(content.json(), &data)) {
    return fpromise::error("Failed to read string from VMO");
  }
  content.json().vmo.reset();

  rapidjson::Document d;
  d.Parse(std::move(data));
  if (d.HasParseError()) {
    std::string error = "Failed to parse content as JSON. Offset " +
                        std::to_string(d.GetErrorOffset()) + ": " +
                        rapidjson::GetParseError_En(d.GetParseError());
    return fpromise::error(std::move(error));
  }

  if (!d.IsArray()) {
    return fpromise::error("Expected content to contain an array");
  }

  std::vector<fpromise::result<fuchsia::logger::LogMessage, std::string>> output;
  for (rapidjson::SizeType i = 0; i < d.Size(); ++i) {
    auto parsed = ParseSingleJsonLog(d[i]);
    if (parsed.is_error()) {
      output.emplace_back(fpromise::error(parsed.error()));
      continue;
    }
    // Encode to FXT and decode through production fidl_log_message_parser.
    auto fxt_bytes = BuildFxtRecordFromParsed(parsed.value());
    zx::vmo vmo;
    zx_status_t status = zx::vmo::create(fxt_bytes.size(), 0, &vmo);
    if (status != ZX_OK) {
      output.emplace_back(fpromise::error("Failed to create FXT VMO"));
      continue;
    }
    status = vmo.write(fxt_bytes.data(), 0, fxt_bytes.size());
    if (status != ZX_OK) {
      output.emplace_back(fpromise::error("Failed to write FXT VMO"));
      continue;
    }
    status = vmo.set_prop_content_size(fxt_bytes.size());
    if (status != ZX_OK) {
      output.emplace_back(fpromise::error("Failed to set content size on FXT VMO"));
      continue;
    }
    fuchsia::diagnostics::FormattedContent fxt_content;
    fxt_content.set_fxt(std::move(vmo));
    auto decoded_results =
        fidl_log_message_parser::ConvertFormattedContentToLogMessages(std::move(fxt_content));
    if (decoded_results.is_error()) {
      output.emplace_back(fpromise::error(decoded_results.error()));
    } else if (decoded_results.value().empty()) {
      output.emplace_back(fpromise::error("No messages decoded from FXT"));
    } else {
      output.emplace_back(std::move(decoded_results.value()[0]));
    }
  }

  return fpromise::ok(std::move(output));
}

}  // namespace diagnostics::fidl_log_message_parser::testing
