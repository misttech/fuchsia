// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/client/tracepoint_capture.h"

#include <inttypes.h>

#include <algorithm>
#include <ranges>

#include "src/developer/debug/shared/register_info.h"
#include "src/developer/debug/zxdb/client/frame.h"
#include "src/developer/debug/zxdb/client/pretty_stack_manager.h"
#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/stack.h"
#include "src/developer/debug/zxdb/client/system.h"
#include "src/developer/debug/zxdb/client/thread.h"
#include "src/developer/debug/zxdb/common/file_util.h"
#include "src/developer/debug/zxdb/common/join_callbacks.h"
#include "src/developer/debug/zxdb/expr/cast.h"
#include "src/developer/debug/zxdb/expr/eval_context.h"
#include "src/developer/debug/zxdb/expr/expr.h"
#include "src/developer/debug/zxdb/expr/expr_parser.h"
#include "src/developer/debug/zxdb/expr/format.h"
#include "src/developer/debug/zxdb/expr/format_node.h"
#include "src/developer/debug/zxdb/expr/format_options.h"
#include "src/developer/debug/zxdb/expr/parsed_identifier.h"
#include "src/developer/debug/zxdb/format/format.h"
#include "src/developer/debug/zxdb/symbols/base_type.h"
#include "src/developer/debug/zxdb/symbols/function.h"
#include "src/developer/debug/zxdb/symbols/location.h"
#include "src/developer/debug/zxdb/symbols/variable.h"
#include "src/lib/fxl/memory/ref_counted.h"
#include "src/lib/fxl/strings/string_printf.h"

namespace zxdb {

namespace {

bool MatchParsedIdentifier(const ParsedIdentifier& target, const ParsedIdentifier& candidate) {
  if (target.components().empty())
    return true;
  if (target.components().size() > candidate.components().size())
    return false;

  size_t offset = candidate.components().size() - target.components().size();
  if (target.qualification() == IdentifierQualification::kGlobal && offset != 0)
    return false;

  for (size_t i = 0; i < target.components().size(); ++i) {
    const auto& tc = target.components()[i];
    const auto& cc = candidate.components()[offset + i];

    if (tc.name() != cc.name())
      return false;

    if (tc.has_template()) {
      if (!cc.has_template() || tc.template_contents() != cc.template_contents())
        return false;
    }
  }
  return true;
}

bool FrameMatchesFunction(const Frame* frame, const ParsedIdentifier& target_ident) {
  const Location& loc = frame->GetLocation();
  if (!loc.has_symbols() || !loc.symbol())
    return false;
  const Function* func = loc.symbol().Get()->As<Function>();
  if (!func)
    return false;
  return MatchParsedIdentifier(target_ident, ToParsedIdentifier(func->GetIdentifier()));
}

bool FrameMatchesFileLine(const Frame* frame, const FileLine& target_file_line) {
  const Location& loc = frame->GetLocation();
  if (!loc.file_line().is_valid())
    return false;
  if (loc.file_line().line() != target_file_line.line())
    return false;
  return PathEndsWith(loc.file_line().file(), target_file_line.file());
}

void ExtractLocals(const fxl::RefPtr<JoinCallbacks<void>>& joiner, TracepointFrameCapture* capture,
                   const fxl::RefPtr<EvalContext>& eval_context, Frame* frame) {
  ErrOr<std::map<std::string, fxl::RefPtr<Variable>>> vars_or = frame->GetLocalVariables();
  if (vars_or.has_error() || vars_or.value().empty()) {
    capture->locals = std::map<std::string, TracepointValue>();
    return;
  }
  const auto& vars = vars_or.value();
  capture->locals = std::map<std::string, TracepointValue>();

  for (const auto& [name, var] : vars) {
    auto cb = joiner->AddCallback();
    eval_context->GetVariableValue(
        var, [name = name, eval_context, capture, cb = std::move(cb)](ErrOrValue val_res) mutable {
          auto node = std::make_unique<FormatNode>(std::string(), val_res);
          auto* node_ptr = node.get();
          FormatBufferOptions options;
          options.pointer_expand_depth = 0;
          DescribeFormatTreeNode(node_ptr, options, eval_context,
                                 fit::defer_callback([name, options, node = std::move(node),
                                                      capture, cb = std::move(cb)]() mutable {
                                   TracepointValue tv;
                                   if (node->err().has_error()) {
                                     tv.value = "<" + node->err().msg() + ">";
                                   } else {
                                     tv.value = FormatTreeNode(*node, options).AsString();
                                     tv.type = node->type();
                                   }
                                   (*capture->locals)[name] = std::move(tv);
                                   cb();
                                 }));
        });
  }
}

void ExtractExpressions(const fxl::RefPtr<JoinCallbacks<void>>& joiner,
                        TracepointFrameCapture* capture,
                        const fxl::RefPtr<EvalContext>& eval_context,
                        const std::vector<std::string>& expressions) {
  if (expressions.empty())
    return;

  capture->evaluations = std::map<std::string, TracepointEvaluation>();

  for (const auto& expr : expressions) {
    auto node = std::make_unique<FormatNode>(std::string(), expr);
    auto* node_ptr = node.get();
    FormatBufferOptions options;
    options.pointer_expand_depth = 0;
    auto cb = joiner->AddCallback();
    DescribeFormatTreeNode(node_ptr, options, eval_context,
                           fit::defer_callback([expr, options, node = std::move(node), capture,
                                                cb = std::move(cb)]() mutable {
                             TracepointEvaluation te;
                             if (node->err().has_error()) {
                               te.error = node->err().msg();
                               capture->errors.push_back(node->err().msg());
                             } else {
                               te.result = FormatTreeNode(*node, options).AsString();
                               te.type = node->type();
                             }
                             capture->evaluations[expr] = std::move(te);
                             cb();
                           }));
  }
}

void ExtractRegisters(const fxl::RefPtr<JoinCallbacks<void>>& joiner,
                      TracepointFrameCapture* capture, Frame* frame) {
  const auto* sync_regs = frame->GetRegisterCategorySync(debug::RegisterCategory::kGeneral);
  if (sync_regs && !sync_regs->empty()) {
    capture->registers = FormatRegisterValues(*sync_regs);
    return;
  }

  frame->GetRegisterCategoryAsync(
      debug::RegisterCategory::kGeneral, false,
      [capture, cb = joiner->AddCallback()](const Err& err,
                                            const std::vector<debug::RegisterValue>& regs) mutable {
        if (err.has_error()) {
          capture->errors.push_back(err.msg());
        } else {
          capture->registers = FormatRegisterValues(regs);
        }
        cb();
      });
}

TracepointFrameRecord MakeFrameRecord(const Frame* f, size_t frame_index) {
  TracepointFrameRecord rec;
  rec.frame_index = frame_index;
  rec.ip = f->GetAddress();
  rec.sp = f->GetStackPointer();
  rec.trust = f->GetTrust();
  const Location& loc = f->GetLocation();
  if (loc.has_symbols() && loc.symbol()) {
    rec.name = loc.symbol().Get()->GetFullName();
  }
  if (loc.file_line().is_valid()) {
    rec.file = loc.file_line().file();
    rec.line = loc.file_line().line();
  }
  rec.elided = false;
  return rec;
}

void DoCaptureFrameState(fxl::WeakPtr<Thread> thread, fxl::WeakPtr<Frame> frame,
                         const TracepointSpec& spec,
                         fit::callback<void(ErrOr<TracepointFrameCapture>)> cb) {
  if (!frame) {
    cb(Err("Frame destroyed during tracepoint capture."));
    return;
  }

  auto capture = std::make_shared<TracepointFrameCapture>();

  if (spec.frame) {
    TracepointFrameSummary summary;
    summary.ip = frame->GetAddress();
    summary.sp = frame->GetStackPointer();
    summary.trust = frame->GetTrust();
    const Location& loc = frame->GetLocation();
    if (loc.has_symbols() && loc.symbol()) {
      summary.name = loc.symbol().Get()->GetFullName();
    }
    if (loc.file_line().is_valid()) {
      summary.file = loc.file_line().file();
      summary.line = loc.file_line().line();
    }
    capture->frame = std::move(summary);
  }

  if (spec.backtrace || spec.raw_backtrace) {
    if (thread) {
      const Frame* matched = frame.get();
      capture->backtrace = CaptureThreadBacktrace(thread.get(), spec.raw_backtrace, {&matched, 1});
    } else {
      capture->errors.push_back("Thread not available for backtrace capture.");
    }
  }

  auto joiner = fxl::MakeRefCounted<JoinCallbacks<void>>();

  if (spec.registers) {
    ExtractRegisters(joiner, capture.get(), frame.get());
  }

  auto eval_context = frame->GetEvalContext();
  if (spec.locals && eval_context) {
    ExtractLocals(joiner, capture.get(), eval_context, frame.get());
  }

  if (!spec.expressions.empty() && eval_context) {
    ExtractExpressions(joiner, capture.get(), eval_context, spec.expressions);
  }

  joiner->Ready([capture, cb = std::move(cb)]() mutable { cb(std::move(*capture)); });
}

TracepointMatchResult MatchAddressFrames(Stack* stack, TargetPointer address,
                                         std::optional<uint32_t> max_matching_frames,
                                         FrameMatchMode mode) {
  TracepointMatchResult result;
  if (mode == FrameMatchMode::kLiveStopTopFrame) {
    for (size_t i = 0; i < stack->size(); ++i) {
      Frame* f = (*stack)[i];
      if (f->GetAddress() != address)
        break;
      if (max_matching_frames.has_value() && result.frames.size() >= *max_matching_frames) {
        result.truncated = true;
        break;
      }
      result.frames.push_back(f);
    }
    return result;
  }

  for (size_t i = 0; i < stack->size(); ++i) {
    Frame* f = (*stack)[i];
    if (f->GetAddress() == address) {
      if (max_matching_frames.has_value() && result.frames.size() >= *max_matching_frames) {
        result.truncated = true;
        break;
      }
      result.frames.push_back(f);
    }
  }
  return result;
}

TracepointMatchResult MatchFileLineFrames(Stack* stack, const FileLine& file_line,
                                          std::optional<uint32_t> max_matching_frames,
                                          FrameMatchMode mode) {
  TracepointMatchResult result;
  if (mode == FrameMatchMode::kLiveStopTopFrame) {
    TargetPointer top_ip = (*stack)[0]->GetAddress();
    for (size_t i = 0; i < stack->size(); ++i) {
      Frame* f = (*stack)[i];
      if (f->GetAddress() != top_ip)
        break;
      if (FrameMatchesFileLine(f, file_line)) {
        if (max_matching_frames.has_value() && result.frames.size() >= *max_matching_frames) {
          result.truncated = true;
          break;
        }
        result.frames.push_back(f);
      }
    }
    if (result.frames.empty()) {
      result.frames.push_back((*stack)[0]);
    }
    return result;
  }

  for (size_t i = 0; i < stack->size(); ++i) {
    Frame* f = (*stack)[i];
    if (FrameMatchesFileLine(f, file_line)) {
      if (max_matching_frames.has_value() && result.frames.size() >= *max_matching_frames) {
        result.truncated = true;
        break;
      }
      result.frames.push_back(f);
    }
  }
  return result;
}

ErrOr<TracepointMatchResult> MatchFunctionFrames(Stack* stack, const std::string& function_name,
                                                 std::optional<uint32_t> max_matching_frames,
                                                 FrameMatchMode mode) {
  ParsedIdentifier target_ident;
  if (Err err = ExprParser::ParseIdentifier(function_name, &target_ident); err.has_error())
    return err;

  TracepointMatchResult result;
  if (mode == FrameMatchMode::kLiveStopTopFrame) {
    TargetPointer top_ip = (*stack)[0]->GetAddress();
    for (size_t i = 0; i < stack->size(); ++i) {
      Frame* f = (*stack)[i];
      if (f->GetAddress() != top_ip)
        break;
      if (FrameMatchesFunction(f, target_ident)) {
        if (max_matching_frames.has_value() && result.frames.size() >= *max_matching_frames) {
          result.truncated = true;
          break;
        }
        result.frames.push_back(f);
      }
    }
    if (result.frames.empty()) {
      result.frames.push_back((*stack)[0]);
    }
    return result;
  }

  for (size_t i = 0; i < stack->size(); ++i) {
    Frame* f = (*stack)[i];
    if (FrameMatchesFunction(f, target_ident)) {
      if (max_matching_frames.has_value() && result.frames.size() >= *max_matching_frames) {
        result.truncated = true;
        break;
      }
      result.frames.push_back(f);
    }
  }
  return result;
}

}  // namespace

std::vector<TracepointFrameRecord> CaptureThreadBacktrace(
    const Thread* thread, bool raw_backtrace, std::span<const Frame* const> matched_frames) {
  std::vector<TracepointFrameRecord> records;
  if (!thread)
    return records;

  const Stack& stack = thread->GetStack();
  records.reserve(stack.size());

  if (raw_backtrace) {
    for (size_t i = 0; i < stack.size(); ++i) {
      records.push_back(MakeFrameRecord(stack[i], i));
    }
    return records;
  }

  auto pretty_manager = thread->session()->system().pretty_stack_manager();
  std::vector<PrettyStackManager::FrameEntry> entries;
  if (pretty_manager) {
    entries = pretty_manager->ProcessStack(stack);
  }
  if (entries.empty()) {
    for (size_t i = 0; i < stack.size(); ++i) {
      records.push_back(MakeFrameRecord(stack[i], i));
    }
    return records;
  }

  for (const auto& entry : entries) {
    bool contains_matched = std::ranges::any_of(entry.frames, [&matched_frames](const Frame* f) {
      return std::ranges::find(matched_frames, f) != matched_frames.end();
    });
    for (size_t i = 0; i < entry.frames.size(); ++i) {
      TracepointFrameRecord rec = MakeFrameRecord(entry.frames[i], entry.begin_index + i);
      if (entry.match && !contains_matched) {
        rec.elided = (i > 0);
        rec.elide_description = entry.match.description;
      }
      records.push_back(std::move(rec));
    }
  }
  return records;
}

ErrOr<TracepointMatchResult> MatchTracepointFrames(Stack* stack, const TracepointSpec& spec,
                                                   FrameMatchMode mode) {
  if (spec.max_matching_frames.has_value() && *spec.max_matching_frames == 0) {
    return Err("Tracepoint max_matching_frames must be positive.");
  }

  if (mode == FrameMatchMode::kAllMatchingFrames && spec.watch.has_value()) {
    return Err("Watchpoint tracepoints do not support all-matching-frames mode.");
  }

  if (!stack || stack->empty())
    return TracepointMatchResult();

  if (spec.watch.has_value()) {
    return TracepointMatchResult{.frames = {(*stack)[0]}};
  }

  if (spec.address.has_value()) {
    return MatchAddressFrames(stack, *spec.address, spec.max_matching_frames, mode);
  }

  if (spec.file_line.has_value() && spec.file_line->is_valid()) {
    return MatchFileLineFrames(stack, *spec.file_line, spec.max_matching_frames, mode);
  }

  if (spec.function.has_value() && !spec.function->empty()) {
    return MatchFunctionFrames(stack, *spec.function, spec.max_matching_frames, mode);
  }

  TracepointMatchResult result;
  if (mode == FrameMatchMode::kLiveStopTopFrame) {
    result.frames.push_back((*stack)[0]);
    return result;
  }

  for (size_t i = 0; i < stack->size(); ++i) {
    if (spec.max_matching_frames.has_value() && result.frames.size() >= *spec.max_matching_frames) {
      result.truncated = true;
      break;
    }
    result.frames.push_back((*stack)[i]);
  }
  return result;
}

void CaptureTracepointFrame(fxl::WeakPtr<Thread> thread, fxl::WeakPtr<Frame> frame,
                            const TracepointSpec& spec,
                            fit::callback<void(ErrOr<TracepointFrameCapture>)> cb) {
  if (!frame) {
    cb(Err("Frame destroyed during tracepoint capture."));
    return;
  }

  if (spec.condition.has_value() && !spec.condition->empty()) {
    auto eval_context = frame->GetEvalContext();
    if (!eval_context) {
      cb(Err("No eval context available for condition evaluation."));
      return;
    }

    EvalExpression(*spec.condition, eval_context, true,
                   [thread, frame, spec, cb = std::move(cb)](ErrOrValue val_res) mutable {
                     if (!frame) {
                       cb(Err("Frame destroyed during condition evaluation."));
                       return;
                     }

                     if (val_res.has_error()) {
                       TracepointFrameCapture capture;
                       capture.matched = false;
                       capture.errors.push_back(val_res.err().msg());
                       cb(capture);
                       return;
                     }

                     auto bool_res =
                         CastNumericExprValueToBool(frame->GetEvalContext(), val_res.value());
                     if (bool_res.has_error()) {
                       TracepointFrameCapture capture;
                       capture.matched = false;
                       capture.errors.push_back(bool_res.err().msg());
                       cb(capture);
                       return;
                     }

                     if (!bool_res.value()) {
                       TracepointFrameCapture capture;
                       capture.matched = false;
                       cb(capture);
                       return;
                     }

                     DoCaptureFrameState(thread, frame, spec, std::move(cb));
                   });
    return;
  }

  DoCaptureFrameState(thread, frame, spec, std::move(cb));
}

}  // namespace zxdb
