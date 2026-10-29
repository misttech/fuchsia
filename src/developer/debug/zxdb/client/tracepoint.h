// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_TRACEPOINT_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_TRACEPOINT_H_

#include <algorithm>
#include <cstdint>

#include "lib/fit/function.h"
#include "src/developer/debug/zxdb/client/client_object.h"
#include "src/developer/debug/zxdb/client/execution_scope.h"
#include "src/developer/debug/zxdb/client/tracepoint_spec.h"
#include "src/lib/fxl/macros.h"
#include "src/lib/fxl/memory/weak_ptr.h"

namespace zxdb {

class Breakpoint;
class Err;

// Non-stopping tracepoint managed by System and backed by an internal StopMode::kNone Breakpoint.
// When hit, Session captures the frame state requested in TracepointSpec and automatically resumes
// the stopped thread unless another stopping breakpoint hits at the same address.
class Tracepoint : public ClientObject {
 public:
  using SetCallback = fit::callback<void(const Err&)>;

  static constexpr uint32_t kDefaultMaxHits = 20;

  Tracepoint(Session* session, uint32_t id);
  ~Tracepoint() override;

  fxl::WeakPtr<Tracepoint> GetWeakPtr();

  // Returns the unique session-scoped identifier assigned by System.
  uint32_t id() const { return id_; }

  // Configures the target selector and frame capture options, updating the underlying breakpoint.
  const TracepointSpec& spec() const { return spec_; }
  void SetSpec(const TracepointSpec& spec, SetCallback cb = SetCallback());

  // Scopes the underlying breakpoint to all processes, a single target, or a single thread.
  const ExecutionScope& scope() const { return scope_; }
  void SetScope(const ExecutionScope& scope);

  // Enables or disables hit collection. Re-enabling resets hit_count and clears auto_disabled.
  // It also re-creates the internal breakpoint from the spec if the breakpoint is gone.
  bool enabled() const;
  void SetEnabled(bool enabled);

  // Indicates whether the tracepoint disabled itself after reaching max_hits.
  bool auto_disabled() const { return auto_disabled_; }
  void set_auto_disabled(bool auto_disabled) { auto_disabled_ = auto_disabled; }

  // Caps the number of hits before auto-disabling. Values of zero clamp to one. A hit is one thread
  // stop: when several inline frames match in the same stop, each frame gets its own hit record,
  // but all of them share one hit_count.
  uint32_t max_hits() const { return max_hits_; }
  void set_max_hits(uint32_t max_hits) { max_hits_ = std::max(1u, max_hits); }

  uint32_t hit_count() const { return hit_count_; }
  void set_hit_count(uint32_t hit_count) { hit_count_ = hit_count; }

  // Returns the internal breakpoint that drives target hit notifications.
  Breakpoint* breakpoint() const { return breakpoint_.get(); }

  // Returns true when enabled and resolved to at least one target breakpoint location.
  bool verified() const;

 private:
  Breakpoint* EnsureBreakpoint();
  void UpdateBreakpoint(SetCallback cb);

  uint32_t id_ = 0;
  TracepointSpec spec_;
  ExecutionScope scope_;
  bool enabled_ = true;
  bool auto_disabled_ = false;
  uint32_t max_hits_ = kDefaultMaxHits;
  uint32_t hit_count_ = 0;

  fxl::WeakPtr<Breakpoint> breakpoint_;

  fxl::WeakPtrFactory<Tracepoint> weak_factory_;

  FXL_DISALLOW_COPY_AND_ASSIGN(Tracepoint);
};

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_TRACEPOINT_H_
