// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_TRACEPOINT_CAPTURE_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_TRACEPOINT_CAPTURE_H_

#include <stdint.h>
#include <zircon/types.h>

#include <cstdint>
#include <map>
#include <optional>
#include <span>
#include <string>
#include <vector>

#include "lib/fit/function.h"
#include "src/developer/debug/ipc/records.h"
#include "src/developer/debug/zxdb/client/frame.h"
#include "src/developer/debug/zxdb/client/tracepoint_spec.h"
#include "src/developer/debug/zxdb/common/err_or.h"
#include "src/lib/fxl/memory/weak_ptr.h"

namespace zxdb {

class Frame;
class Stack;
class Thread;

enum class FrameMatchMode : uint8_t {
  kLiveStopTopFrame,
  kAllMatchingFrames,
};

struct TracepointMatchResult {
  std::vector<Frame*> frames;
  bool truncated = false;
};

struct TracepointFrameSummary {
  std::string name;
  std::optional<std::string> file;
  std::optional<int> line;
  uint64_t ip = 0;
  uint64_t sp = 0;
  debug_ipc::StackFrame::Trust trust = debug_ipc::StackFrame::Trust::kUnknown;
};

struct TracepointFrameRecord {
  size_t frame_index = 0;
  std::string name;
  std::optional<std::string> file;
  std::optional<int> line;
  uint64_t ip = 0;
  uint64_t sp = 0;
  debug_ipc::StackFrame::Trust trust = debug_ipc::StackFrame::Trust::kUnknown;
  bool elided = false;
  std::string elide_description;
};

struct TracepointValue {
  std::string value;
  std::string type;
};

struct TracepointEvaluation {
  std::optional<std::string> result;
  std::optional<std::string> type;
  std::optional<std::string> error;
};

struct TracepointFrameCapture {
  bool matched = true;
  std::optional<TracepointFrameSummary> frame;
  std::optional<std::vector<TracepointFrameRecord>> backtrace;
  std::optional<std::map<std::string, TracepointValue>> locals;
  std::optional<std::map<std::string, std::string>> registers;
  std::map<std::string, TracepointEvaluation> evaluations;
  std::vector<std::string> errors;
};

struct TracepointHitRecord {
  uint32_t tracepoint_id = 0;
  std::string location;
  uint32_t hit_count = 0;
  uint32_t max_hits = 0;
  bool auto_disabled = false;
  bool truncated = false;
  zx_koid_t thread_koid = 0;
  double dwell_ms = 0.0;
  TracepointFrameCapture capture;
};

// Matches frames according to the target spec.
ErrOr<TracepointMatchResult> MatchTracepointFrames(Stack* stack, const TracepointSpec& spec,
                                                   FrameMatchMode mode);

// Captures the backtrace of a thread, applying pretty-stack elision unless raw_backtrace is true.
// Any elided frame group that contains a frame in matched_frames stays un-elided so matched frames
// remain visible.
std::vector<TracepointFrameRecord> CaptureThreadBacktrace(
    const Thread* thread, bool raw_backtrace = false,
    std::span<const Frame* const> matched_frames = {});

// Asynchronously captures frame state requested by |spec|.
void CaptureTracepointFrame(fxl::WeakPtr<Thread> thread, fxl::WeakPtr<Frame> frame,
                            const TracepointSpec& spec,
                            fit::callback<void(ErrOr<TracepointFrameCapture>)> cb);

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_TRACEPOINT_CAPTURE_H_
