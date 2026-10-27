// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/console/commands/verb_locals.h"

#include "src/developer/debug/zxdb/client/frame.h"
#include "src/developer/debug/zxdb/console/command.h"
#include "src/developer/debug/zxdb/console/command_utils.h"
#include "src/developer/debug/zxdb/console/console.h"
#include "src/developer/debug/zxdb/console/print_command_utils.h"
#include "src/developer/debug/zxdb/console/verbs.h"
#include "src/developer/debug/zxdb/format/output_buffer.h"
#include "src/developer/debug/zxdb/symbols/variable.h"

namespace zxdb {

namespace {

const char kLocalsShortHelp[] = "locals: Print local variables and function args.";
const char kLocalsUsage[] = "locals";
const char kLocalsHelp[] = R"(
  Prints all local variables and the current function's arguments. By default
  it will print the variables for the currently selected stack frame.

  You can override the stack frame with the "frame" noun to get the locals
  for any specific stack frame of thread.

Arguments

)" PRINT_COMMAND_SWITCH_HELP
                           R"(
Examples

  locals
      Prints locals and args for the current stack frame.

  f 4 locals
  frame 4 locals
  thread 2 frame 3 locals
      Prints locals for a specific stack frame.

  f 4 locals -t
      Prints locals with types.
)";
void RunVerbLocals(const Command& cmd, fxl::RefPtr<CommandContext> cmd_context) {
  if (Err err =
          AssertStoppedThreadWithFrameCommand(cmd_context->GetConsoleContext(), cmd, "locals");
      err.has_error())
    return cmd_context->ReportError(err);

  ErrOr<std::map<std::string, fxl::RefPtr<Variable>>> vars = cmd.frame()->GetLocalVariables();
  if (vars.has_error())
    return cmd_context->ReportError(vars.err());

  if (vars.value().empty()) {
    cmd_context->Output("No local variables in scope.");
    return;
  }

  ErrOr<FormatBufferOptions> options = GetPrintCommandFormatOptions(cmd);
  if (options.has_error())
    return cmd_context->ReportError(options.err());

  auto output = fxl::MakeRefCounted<AsyncOutputBuffer>();
  for (const auto& pair : vars.value()) {
    output->Append(
        FormatVariable(pair.second.get(), options.value(), cmd.frame()->GetEvalContext()));
    output->Append("\n");
  }
  output->Complete();
  cmd_context->Output(output);
}

}  // namespace

VerbRecord GetLocalsVerbRecord() {
  VerbRecord locals(&RunVerbLocals, {"locals"}, kLocalsShortHelp, kLocalsUsage, kLocalsHelp,
                    CommandGroup::kQuery);
  AppendPrintCommandSwitches(&locals);
  return locals;
}

}  // namespace zxdb
