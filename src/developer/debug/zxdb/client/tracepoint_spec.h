// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_TRACEPOINT_SPEC_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_TRACEPOINT_SPEC_H_

#include <stdint.h>

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

  bool operator==(const TracepointWatchSpec& other) const {
    return expression == other.expression && byte_size == other.byte_size &&
           access_type == other.access_type && address == other.address;
  }
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

  Err Validate() const {
    if (max_matching_frames.has_value() && *max_matching_frames == 0) {
      return Err("max_matching_frames must be at least 1.");
    }

    size_t target_count = 0;
    if (function.has_value()) {
      if (function->empty()) {
        return Err("Function name cannot be empty for a tracepoint.");
      }
      target_count++;
    }
    if (file_line.has_value()) {
      if (!file_line->is_valid()) {
        return Err("Invalid file/line location specified for tracepoint.");
      }
      target_count++;
    }
    if (address.has_value()) {
      target_count++;
    }
    if (watch.has_value()) {
      if (watch->expression.empty() && !watch->address.has_value()) {
        return Err("Watchpoint tracepoints require either an expression or address.");
      }
      if (watch->access_type != debug_ipc::BreakpointType::kWrite &&
          watch->access_type != debug_ipc::BreakpointType::kReadWrite) {
        return Err("Unsupported watchpoint access type (only write or read-write allowed).");
      }
      target_count++;
    }
    if (target_count == 0) {
      return Err("A tracepoint must specify a function, file/line, address, or watch target.");
    }
    if (target_count > 1) {
      return Err(
          "A tracepoint can specify at most one of a function, file/line, address, or watch "
          "target.");
    }
    return Err();
  }

  bool operator==(const TracepointSpec& other) const {
    return function == other.function && file_line == other.file_line && watch == other.watch &&
           address == other.address && condition == other.condition && frame == other.frame &&
           backtrace == other.backtrace && raw_backtrace == other.raw_backtrace &&
           locals == other.locals && registers == other.registers &&
           expressions == other.expressions && max_matching_frames == other.max_matching_frames;
  }
};

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_TRACEPOINT_SPEC_H_
