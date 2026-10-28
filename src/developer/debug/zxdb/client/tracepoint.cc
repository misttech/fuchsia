// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/client/tracepoint.h"

#include <utility>

#include "src/developer/debug/zxdb/client/breakpoint.h"
#include "src/developer/debug/zxdb/client/breakpoint_settings.h"
#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/system.h"
#include "src/developer/debug/zxdb/common/err.h"
#include "src/developer/debug/zxdb/expr/expr_parser.h"
#include "src/developer/debug/zxdb/symbols/identifier.h"
#include "src/developer/debug/zxdb/symbols/input_location.h"

namespace zxdb {

Tracepoint::Tracepoint(Session* session, uint32_t id)
    : ClientObject(session), id_(id), weak_factory_(this) {
  EnsureBreakpoint();
  UpdateBreakpoint(SetCallback());
}

Tracepoint::~Tracepoint() {
  if (breakpoint_ && session()) {
    session()->system().DeleteBreakpoint(breakpoint_.get());
    breakpoint_.reset();
  }
}

fxl::WeakPtr<Tracepoint> Tracepoint::GetWeakPtr() { return weak_factory_.GetWeakPtr(); }

Breakpoint* Tracepoint::EnsureBreakpoint() {
  if (!breakpoint_ && session()) {
    Breakpoint* bp = session()->system().CreateNewInternalBreakpoint();
    breakpoint_ = bp->GetWeakPtr();
  }
  return breakpoint_.get();
}

void Tracepoint::SetSpec(const TracepointSpec& spec, SetCallback cb) {
  if (Err err = spec.Validate(); err.has_error()) {
    spec_ = spec;
    if (breakpoint_) {
      BreakpointSettings settings = breakpoint_->GetSettings();
      settings.enabled = false;
      settings.locations.clear();
      breakpoint_->SetSettings(settings);
    }
    if (cb)
      cb(err);
    return;
  }
  spec_ = spec;
  UpdateBreakpoint(std::move(cb));
}

void Tracepoint::SetEnabled(bool enabled) {
  if (enabled) {
    auto_disabled_ = false;
    hit_count_ = 0;
  }
  if (enabled_ == enabled)
    return;
  enabled_ = enabled;
  if (breakpoint_) {
    BreakpointSettings settings = breakpoint_->GetSettings();
    settings.enabled = enabled_;
    breakpoint_->SetSettings(settings);
  }
}

bool Tracepoint::verified() const {
  if (!breakpoint_)
    return false;
  if (!enabled_ || !breakpoint_->GetSettings().enabled)
    return false;
  return !breakpoint_->GetLocations().empty();
}

void Tracepoint::UpdateBreakpoint(SetCallback cb) {
  if (!breakpoint_) {
    if (cb)
      cb(Err("No underlying breakpoint attached to tracepoint."));
    return;
  }

  BreakpointSettings settings;
  settings.enabled = enabled_;
  settings.stop_mode = BreakpointSettings::StopMode::kNone;

  if (spec_.function.has_value() && !spec_.function->empty()) {
    Identifier ident;
    if (Err err = ExprParser::ParseIdentifier(*spec_.function, &ident); err.has_error()) {
      ident = Identifier(*spec_.function);
    }
    settings.locations.emplace_back(std::move(ident));
  } else if (spec_.file_line.has_value() && spec_.file_line->is_valid()) {
    settings.locations.emplace_back(*spec_.file_line);
  } else if (spec_.address.has_value()) {
    settings.locations.emplace_back(*spec_.address);
  } else if (spec_.watch.has_value()) {
    settings.type = spec_.watch->access_type;
    settings.byte_size = spec_.watch->byte_size;
    if (spec_.watch->address.has_value()) {
      settings.locations.emplace_back(*spec_.watch->address);
    }
  }

  breakpoint_->SetSettings(settings, std::move(cb));
}

}  // namespace zxdb
