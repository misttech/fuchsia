// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/console/commands/verb_handle.h"

#include <algorithm>
#include <cstdint>
#include <tuple>

#include "src/developer/debug/zxdb/client/process.h"
#include "src/developer/debug/zxdb/client/target.h"
#include "src/developer/debug/zxdb/console/command.h"
#include "src/developer/debug/zxdb/console/command_utils.h"
#include "src/developer/debug/zxdb/console/format_handle.h"
#include "src/developer/debug/zxdb/console/verbs.h"
#include "src/developer/debug/zxdb/format/output_buffer.h"

namespace zxdb {

namespace {

constexpr int kKoidSwitch = 1;
constexpr int kHexSwitch = 2;

enum class LookupType : uint8_t {
  kHandle,  // Search for the object with the given handle value.
  kKoid,    // Search for the object with the given koid.
};

const char kHandleShortHelp[] = "handle[s]: Print handle list or details.";
const char kHandleUsage[] = "handle[s] [-k] [-x] [ <expression> ]";
const char kHandleHelp[] = R"(
  With no arguments, prints all handles for the process.

  If an expression or number is given, more detailed information for the given
  handle value (the default) or koid (with the "-k" option) will be printed.

  👉 See "help expressions" for how to write expressions.

  In addition to open handles, this command will print VMO ("Virtual Memory
  Object") information for mapped VMOs, even if there is no open handle to it.
  These will be shown with "<none>" for the handle value. To view detailed
  information about these objects, reference them by koid using the "-k" switch.

Options

  -k
    Look up the object by koid instead of handle value. This will only match
    objects visible to the process, not arbitrary objects in the system.

  -x
     Print numbers as hexadecimal. Otherwise defaults to decimal.

Examples

  handle
  process 1 handles
      Print all handles for the current/given process.

  handle -x h
  handle -x some_object->handle
      Prints the information for the given handle.

  handle -k 7256
      Prints the information for the object with koid 7256.
)";

void OnEvalComplete(const fxl::RefPtr<CommandContext>& cmd_context,
                    const fxl::WeakPtr<Process>& weak_process, LookupType lookup, ErrOrValue value,
                    bool hex) {
  if (!weak_process) {
    cmd_context->ReportError(Err("Process exited while requesting handles."));
    return;
  }
  if (value.has_error()) {
    cmd_context->ReportError(value.err());
    return;
  }

  uint64_t lookup_value = 0;
  if (Err err = value.value().PromoteTo64(&lookup_value); err.has_error()) {
    cmd_context->ReportError(err);
    return;
  }

  weak_process->LoadInfoHandleTable([cmd_context, lookup, lookup_value,
                                     hex](ErrOr<std::vector<debug_ipc::InfoHandle>> handles) {
    if (handles.has_error()) {
      cmd_context->ReportError(handles.err());
      return;
    }

    for (const auto& handle : handles.value()) {
      const uint64_t candidate = lookup == LookupType::kHandle ? handle.handle_value : handle.koid;
      if (candidate == lookup_value) {
        cmd_context->Output(FormatHandle(handle, hex));
        return;
      }
    }

    const char* kind = lookup == LookupType::kHandle ? "handle with value" : "object with koid";
    cmd_context->Output(std::string("No ") + kind + " " + std::to_string(lookup_value) +
                        " in the process.");
  });
}

void RunVerbHandle(const Command& cmd, const fxl::RefPtr<CommandContext>& cmd_context) {
  if (Err err = AssertRunningTarget(cmd_context->GetConsoleContext(), "handle", cmd.target());
      err.has_error()) {
    cmd_context->ReportError(err);
    return;
  }

  LookupType lookup = cmd.HasSwitch(kKoidSwitch) ? LookupType::kKoid : LookupType::kHandle;
  bool hex = cmd.HasSwitch(kHexSwitch);

  if (cmd.args().empty()) {
    cmd.target()->GetProcess()->LoadInfoHandleTable(
        [cmd_context, hex](ErrOr<std::vector<debug_ipc::InfoHandle>> handles) {
          if (handles.has_error()) {
            cmd_context->ReportError(handles.err());
            return;
          }

          // Sort by handle value, then koid (mapped VMOs can have no handle value).
          auto handles_sorted = handles.take_value();
          std::ranges::sort(
              handles_sorted, [](const debug_ipc::InfoHandle& a, const debug_ipc::InfoHandle& b) {
                return std::tie(a.handle_value, a.koid) < std::tie(b.handle_value, b.koid);
              });
          cmd_context->Output(FormatHandles(handles_sorted, hex));
        });
  } else {
    // Evaluate the expression, then print just that handle.
    fxl::RefPtr<EvalContext> eval_context = GetEvalContextForCommand(cmd);
    Err err = EvalCommandExpression(
        cmd, "handle", eval_context, /*follow_references=*/false, /*verbose_errors=*/false,
        [cmd_context, weak_process = cmd.target()->GetProcess()->GetWeakPtr(), lookup,
         hex](ErrOrValue value) {
          OnEvalComplete(cmd_context, weak_process, lookup, std::move(value), hex);
        });
    if (err.has_error()) {
      cmd_context->ReportError(err);
    }
  }
}

}  // namespace

VerbRecord GetHandleVerbRecord() {
  VerbRecord handle(&RunVerbHandle, {"handle", "handles"}, kHandleShortHelp, kHandleUsage,
                    kHandleHelp, CommandGroup::kQuery);
  handle.param_type = VerbRecord::kOneParam;
  handle.switches.emplace_back(kKoidSwitch, false, "", 'k');
  handle.switches.emplace_back(kHexSwitch, false, "", 'x');
  return handle;
}

}  // namespace zxdb
