// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/bringup/bin/critical-services/storage-watchdog/diagnostics_text.h"

#include <ctype.h>
#include <inttypes.h>
#include <stdio.h>

#include <algorithm>
#include <functional>

#include <rapidjson/document.h>
#include <rapidjson/stringbuffer.h>
#include <rapidjson/writer.h>

namespace storage_watchdog {
namespace {

// A repeated message is folded into an earlier line if that line is this recent.
constexpr size_t kFoldWindow = 16;

// Whether the last moniker segment belongs to a storage or suspend component. Drivers look like
// "boot-drivers:ufs-3c400000". The Starnix kernels ("kernels:...") don't count.
bool IsStorageComponent(std::string_view segment) {
  constexpr std::string_view kComponents[] = {
      "fshost", "fxfs", "system-activity-governor", "power-broker", "starnix_runner",
  };
  constexpr std::string_view kDrivers[] = {"ufs", "sdmmc", "nvme"};
  if (std::ranges::any_of(kComponents, [&](auto name) { return segment.starts_with(name); })) {
    return true;
  }
  const size_t colon = segment.find("-drivers:");
  return colon != std::string_view::npos &&
         std::ranges::any_of(kDrivers, [name = segment.substr(colon + 9)](auto driver) {
           return name.starts_with(driver);
         });
}

const rapidjson::Value* Member(const rapidjson::Value& object, const char* name) {
  if (!object.IsObject()) {
    return nullptr;
  }
  auto it = object.FindMember(name);
  return it == object.MemberEnd() ? nullptr : &it->value;
}

std::string_view String(const rapidjson::Value* value) {
  return value && value->IsString() ? std::string_view(value->GetString(), value->GetStringLength())
                                    : std::string_view();
}

// Calls |fn| on each element of a JSON array batch, or on the batch itself if it's one object.
template <typename Fn>
void ForEachItem(rapidjson::Document& document, Fn fn) {
  if (document.HasParseError()) {
    return;
  }
  if (!document.IsArray()) {
    fn(document);
    return;
  }
  for (rapidjson::Value& item : document.GetArray()) {
    fn(item);
  }
}

std::string FormatTime(int64_t ns) {
  char buf[32];
  snprintf(buf, sizeof(buf), "%05" PRId64 ".%03" PRId64, ns / 1'000'000'000,
           (ns / 1'000'000) % 1000);
  return buf;
}

// Removes Inspect histograms ({floor, step, counts, indexes, ...}), which are big and rarely
// what matters in a stall.
void StripHistograms(rapidjson::Value& root) {
  std::vector<rapidjson::Value*> stack = {&root};
  while (!stack.empty()) {
    rapidjson::Value* value = stack.back();
    stack.pop_back();
    if (!value->IsObject()) {
      continue;
    }
    for (auto it = value->MemberBegin(); it != value->MemberEnd();) {
      if (Member(it->value, "counts") &&
          (Member(it->value, "floor") || Member(it->value, "indexes"))) {
        it = value->EraseMember(it);
      } else {
        stack.push_back(&it->value);
        ++it;
      }
    }
  }
}

// Keeps only what matters in a stall from power-broker's Inspect data: the broker's lease map,
// and the elements that aren't at their required level or have leases.
void CompactPowerBroker(rapidjson::Value& payload, rapidjson::Document::AllocatorType& allocator) {
  auto path = [](const rapidjson::Value* value, std::initializer_list<const char*> names) {
    for (const char* name : names) {
      value = value ? Member(*value, name) : nullptr;
    }
    return value;
  };
  const rapidjson::Value* broker = path(&payload, {"root", "broker"});
  rapidjson::Value compact(rapidjson::kObjectType);
  if (const rapidjson::Value* leases = path(broker, {"leases"})) {
    compact.AddMember("leases", rapidjson::Value(*leases, allocator), allocator);
  }
  rapidjson::Value elements(rapidjson::kArrayType);
  const rapidjson::Value* topology =
      path(broker, {"topology", "fuchsia.inspect.Graph", "topology"});
  if (topology && topology->IsObject()) {
    for (const auto& node : topology->GetObject()) {
      const rapidjson::Value* meta = Member(node.value, "meta");
      const rapidjson::Value* current = path(meta, {"current_level"});
      const rapidjson::Value* required = path(meta, {"required_level"});
      const rapidjson::Value* leases = path(meta, {"leases"});
      if ((!current || !required || *current == *required) &&
          (!leases || !leases->IsObject() || leases->MemberCount() == 0)) {
        continue;
      }
      rapidjson::Value element(rapidjson::kObjectType);
      for (const char* name : {"name", "current_level", "required_level", "leases"}) {
        if (const rapidjson::Value* value = path(meta, {name})) {
          element.AddMember(rapidjson::StringRef(name), rapidjson::Value(*value, allocator),
                            allocator);
        }
      }
      elements.PushBack(element, allocator);
    }
  }
  compact.AddMember("elements", elements, allocator);
  payload = compact;
}

// Whether an Inspect payload has no data: null, or only empty trees.
bool IsEmpty(const rapidjson::Value* payload) {
  return !payload || !payload->IsObject() ||
         std::all_of(payload->MemberBegin(), payload->MemberEnd(), [](const auto& tree) {
           return tree.value.IsObject() && tree.value.MemberCount() == 0;
         });
}

}  // namespace

void LogTail::Add(int64_t timestamp_ns, std::string_view moniker, std::string_view severity,
                  std::string_view message) {
  moniker = moniker.substr(moniker.rfind('/') + 1);
  const bool storage = IsStorageComponent(moniker);
  if (!storage && severity != "WARN" && severity != "ERROR" && severity != "FATAL") {
    return;
  }
  // Long driver monikers keep their end.
  moniker = moniker.substr(moniker.size() - std::min<size_t>(moniker.size(), 40));

  // Messages match if they're the same apart from space-separated tokens with digits in them
  // (counts, ids, "cpufreq-cpu4", paths with hashes) and URLs.
  std::string key(moniker);
  key += severity;
  for (size_t i = 0; i < message.size();) {
    const size_t end = std::min(message.find(' ', i), message.size());
    const std::string_view token = message.substr(i, end - i);
    if (!token.contains("://") && std::ranges::none_of(token, [](char c) {
          return isdigit(static_cast<unsigned char>(c));
        })) {
      key += token;
      key += ' ';
    }
    i = end + 1;
  }
  std::deque<Entry>& entries = storage ? storage_ : other_;
  const auto recent = entries.end() - static_cast<ptrdiff_t>(std::min(entries.size(), kFoldWindow));
  if (auto it = std::find_if(recent, entries.end(), [&](const Entry& e) { return e.key == key; });
      it != entries.end()) {
    it->repeats++;
    it->last_timestamp = timestamp_ns;
    return;
  }

  Entry entry{.timestamp = timestamp_ns, .key = std::move(key)};
  entry.prefix = "[" + FormatTime(timestamp_ns) + "][" + std::string(moniker) + "][" +
                 (severity.empty() ? '?' : severity[0]) + "] ";
  entry.message = message.substr(0, kMaxLineLength);
  std::ranges::replace(entry.message, '\n', ' ');
  bytes_ += entry.prefix.size() + entry.message.size();
  entries.push_back(std::move(entry));
  Trim();
}

void LogTail::Trim() {
  auto pop = [this](std::deque<Entry>& entries) {
    bytes_ -= entries.front().prefix.size() + entries.front().message.size();
    entries.pop_front();
  };
  while (storage_.size() > max_storage_lines_) {
    pop(storage_);
  }
  while (storage_.size() + other_.size() > max_lines_ || bytes_ > max_bytes_) {
    pop(other_.empty() ? storage_ : other_);
  }
}

void LogTail::AddJson(std::string_view json) {
  rapidjson::Document document;
  document.Parse(json.data(), json.size());
  ForEachItem(document, [this](const rapidjson::Value& log) {
    const rapidjson::Value* metadata = Member(log, "metadata");
    const rapidjson::Value* timestamp = metadata ? Member(*metadata, "timestamp") : nullptr;
    const rapidjson::Value* payload = Member(log, "payload");
    const rapidjson::Value* root = payload ? Member(*payload, "root") : nullptr;
    const rapidjson::Value* message = root ? Member(*root, "message") : nullptr;
    Add(timestamp && timestamp->IsInt64() ? timestamp->GetInt64() : 0,
        String(Member(log, "moniker")), metadata ? String(Member(*metadata, "severity")) : "",
        message ? String(Member(*message, "value")) : "");
  });
}

std::vector<std::string> LogTail::Lines() const {
  std::vector<const Entry*> entries;
  for (const auto* list : {&storage_, &other_}) {
    for (const Entry& entry : *list) {
      entries.push_back(&entry);
    }
  }
  std::ranges::stable_sort(entries, {}, &Entry::timestamp);
  std::vector<std::string> lines;
  for (const Entry* entry : entries) {
    std::string suffix;
    if (entry->repeats > 0) {
      suffix = " (+" + std::to_string(entry->repeats) + " similar until " +
               FormatTime(entry->last_timestamp) + ")";
    }
    const size_t room =
        kMaxLineLength - std::min(kMaxLineLength, entry->prefix.size() + suffix.size());
    lines.push_back(entry->prefix + entry->message.substr(0, room) + suffix);
  }
  return lines;
}

std::vector<std::string> InspectLines(std::span<const std::string> batches,
                                      size_t max_bytes_per_component, size_t max_bytes) {
  std::vector<rapidjson::Document> documents(batches.size());
  std::vector<std::pair<rapidjson::Value*, rapidjson::Document*>> items;
  for (size_t i = 0; i < batches.size(); i++) {
    documents[i].Parse(batches[i].data(), batches[i].size());
    ForEachItem(documents[i],
                [&](rapidjson::Value& item) { items.emplace_back(&item, &documents[i]); });
  }
  auto is_power_broker = [](const rapidjson::Value* item) {
    return String(Member(*item, "moniker")).ends_with("power-broker");
  };
  std::ranges::stable_partition(items, std::not_fn(is_power_broker),
                                &std::pair<rapidjson::Value*, rapidjson::Document*>::first);

  std::vector<std::string> lines;
  for (auto [item, document] : items) {
    auto payload = item->FindMember("payload");
    if (payload == item->MemberEnd() || IsEmpty(&payload->value)) {
      continue;
    }
    if (is_power_broker(item)) {
      CompactPowerBroker(payload->value, document->GetAllocator());
    }
    StripHistograms(payload->value);
    rapidjson::StringBuffer buffer;
    rapidjson::Writer<rapidjson::StringBuffer> writer(buffer);
    payload->value.Accept(writer);
    std::string text = "inspect " + std::string(String(Member(*item, "moniker"))) + ": " +
                       std::string(buffer.GetString(), buffer.GetSize());
    const size_t limit = std::min(max_bytes_per_component, max_bytes);
    if (text.size() > limit) {
      const std::string note = " ...(" + std::to_string(text.size()) + " bytes total)";
      text.resize(limit > note.size() ? limit - note.size() : 0);
      text += note;
    }
    max_bytes -= std::min(max_bytes, text.size());
    for (size_t i = 0; i < text.size(); i += kMaxLineLength) {
      lines.push_back(text.substr(i, kMaxLineLength));
    }
  }
  return lines;
}

size_t DebuglogCost(std::span<const std::string> lines) {
  size_t cost = 0;
  for (const std::string& line : lines) {
    cost += line.size() + kDebuglogRecordHeader;
  }
  return cost;
}

std::span<const std::string> NewestWithinBudget(std::span<const std::string> lines, size_t budget) {
  size_t first = lines.size();
  for (size_t cost = 0; first > 0; first--) {
    cost += lines[first - 1].size() + kDebuglogRecordHeader;
    if (cost > budget) {
      break;
    }
  }
  return lines.subspan(first);
}

}  // namespace storage_watchdog
