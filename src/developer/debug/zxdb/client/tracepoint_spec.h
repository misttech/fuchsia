// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_TRACEPOINT_SPEC_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_TRACEPOINT_SPEC_H_

#include <cstdint>
#include <optional>
#include <string>
#include <vector>

#include "src/developer/debug/ipc/records.h"
#include "src/developer/debug/zxdb/common/err.h"
#include "src/developer/debug/zxdb/symbols/file_line.h"

namespace zxdb {

struct TracepointWatchSpec {
  std::string expression;
  uint32_t byte_size = 4;
  debug_ipc::BreakpointType access_type = debug_ipc::BreakpointType::kWrite;
  std::optional<uint64_t> address;

  bool operator==(const TracepointWatchSpec& other) const = default;
};

struct TracepointSpec {
  std::optional<std::string> function;
  std::optional<FileLine> file_line;
  std::optional<TracepointWatchSpec> watch;
  std::optional<uint64_t> address;

  std::optional<std::string> condition;

  // Capture flags.
  bool frame = true;
  bool backtrace = false;
  bool raw_backtrace = false;
  bool locals = false;
  bool registers = false;

  std::vector<std::string> expressions;

  std::optional<uint32_t> max_matching_frames;

  bool is_valid() const { return Validate().ok(); }

  Err Validate() const;

  bool operator==(const TracepointSpec& other) const = default;
};

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_TRACEPOINT_SPEC_H_
