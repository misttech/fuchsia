// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_BRINGUP_BIN_CRITICAL_SERVICES_STORAGE_WATCHDOG_DIAGNOSTICS_TEXT_H_
#define SRC_BRINGUP_BIN_CRITICAL_SERVICES_STORAGE_WATCHDOG_DIAGNOSTICS_TEXT_H_

#include <stddef.h>
#include <stdint.h>

#include <deque>
#include <span>
#include <string>
#include <string_view>
#include <vector>

// Turns Archivist's JSON log and Inspect snapshots into short debuglog lines for the wedge dump.
namespace storage_watchdog {

// Each line written to stdout becomes one debuglog record, which costs a header in the ring.
constexpr size_t kDebuglogRecordHeader = 40;
// Lines are kept below the 216 byte record limit: log messages are cut, Inspect JSON is split.
constexpr size_t kMaxLineLength = 200;

// Keeps the newest log lines worth having after a storage stall: the last |max_storage_lines|
// from storage and suspend components at any severity, and WARN and above from everything else,
// up to |max_lines| and |max_bytes| in total. Messages that only differ in tokens with digits
// (e.g. the same error for every CPU) are folded into a recent earlier line as a count.
class LogTail {
 public:
  LogTail(size_t max_lines, size_t max_storage_lines, size_t max_bytes)
      : max_lines_(max_lines), max_storage_lines_(max_storage_lines), max_bytes_(max_bytes) {}

  // Adds the messages in one JSON batch from fuchsia.diagnostics/ArchiveAccessor.
  void AddJson(std::string_view json);
  void Add(int64_t timestamp_ns, std::string_view moniker, std::string_view severity,
           std::string_view message);

  // Oldest first.
  std::vector<std::string> Lines() const;

 private:
  struct Entry {
    int64_t timestamp = 0;
    std::string prefix;
    std::string message;
    std::string key;
    size_t repeats = 0;
    int64_t last_timestamp = 0;
  };
  void Trim();

  const size_t max_lines_;
  const size_t max_storage_lines_;
  const size_t max_bytes_;
  std::deque<Entry> storage_;
  std::deque<Entry> other_;
  size_t bytes_ = 0;
};

// Formats the components in Inspect JSON batches from fuchsia.diagnostics/ArchiveAccessor as
// "inspect <moniker>: <payload as compact JSON>", without histograms or empty trees, split into
// lines of kMaxLineLength. Each component gets at most |max_bytes_per_component|, and all of them
// together |max_bytes|. power-broker goes last, cut down to its leases and the elements that are
// changing level or have leases.
std::vector<std::string> InspectLines(std::span<const std::string> batches,
                                      size_t max_bytes_per_component, size_t max_bytes);

// The debuglog ring space that writing |lines| takes.
size_t DebuglogCost(std::span<const std::string> lines);

// The newest |lines| that fit in |budget| bytes of debuglog ring.
std::span<const std::string> NewestWithinBudget(std::span<const std::string> lines, size_t budget);

}  // namespace storage_watchdog

#endif  // SRC_BRINGUP_BIN_CRITICAL_SERVICES_STORAGE_WATCHDOG_DIAGNOSTICS_TEXT_H_
