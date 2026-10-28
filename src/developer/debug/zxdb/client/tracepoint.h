// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_TRACEPOINT_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_TRACEPOINT_H_

#include <stdint.h>

#include "lib/fit/function.h"
#include "src/developer/debug/zxdb/client/client_object.h"
#include "src/developer/debug/zxdb/client/tracepoint_spec.h"
#include "src/lib/fxl/macros.h"
#include "src/lib/fxl/memory/weak_ptr.h"

namespace zxdb {

class Breakpoint;
class Err;

class Tracepoint : public ClientObject {
 public:
  using SetCallback = fit::callback<void(const Err&)>;

  Tracepoint(Session* session, uint32_t id);
  ~Tracepoint() override;

  fxl::WeakPtr<Tracepoint> GetWeakPtr();

  uint32_t id() const { return id_; }

  const TracepointSpec& spec() const { return spec_; }
  void SetSpec(const TracepointSpec& spec, SetCallback cb = SetCallback());

  bool enabled() const { return enabled_; }
  void SetEnabled(bool enabled);

  bool auto_disabled() const { return auto_disabled_; }
  void set_auto_disabled(bool auto_disabled) { auto_disabled_ = auto_disabled; }

  uint32_t max_hits() const { return max_hits_; }
  void set_max_hits(uint32_t max_hits) { max_hits_ = max_hits; }

  uint32_t hit_count() const { return hit_count_; }
  void set_hit_count(uint32_t hit_count) { hit_count_ = hit_count; }

  Breakpoint* breakpoint() const { return breakpoint_.get(); }
  Breakpoint* EnsureBreakpoint();

  bool verified() const;

 private:
  void UpdateBreakpoint(SetCallback cb);

  uint32_t id_ = 0;
  TracepointSpec spec_;
  bool enabled_ = true;
  bool auto_disabled_ = false;
  uint32_t max_hits_ = 20;
  uint32_t hit_count_ = 0;

  fxl::WeakPtr<Breakpoint> breakpoint_;

  fxl::WeakPtrFactory<Tracepoint> weak_factory_;

  FXL_DISALLOW_COPY_AND_ASSIGN(Tracepoint);
};

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_TRACEPOINT_H_
