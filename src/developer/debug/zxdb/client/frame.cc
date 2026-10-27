// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/client/frame.h"

#include "src/developer/debug/zxdb/symbols/code_block.h"
#include "src/developer/debug/zxdb/symbols/function.h"
#include "src/developer/debug/zxdb/symbols/location.h"
#include "src/developer/debug/zxdb/symbols/variable.h"
#include "src/developer/debug/zxdb/symbols/visit_scopes.h"

namespace zxdb {

Frame::Frame(Session* session) : ClientObject(session), weak_factory_(this) {}

Frame::~Frame() = default;

fxl::WeakPtr<Frame> Frame::GetWeakPtr() { return weak_factory_.GetWeakPtr(); }

ErrOr<std::map<std::string, fxl::RefPtr<Variable>>> Frame::GetLocalVariables() const {
  const Location& location = GetLocation();
  if (!location.symbol())
    return Err("There is no symbol information for the frame.");
  const Function* function = location.symbol().Get()->As<Function>();
  if (!function)
    return Err("Symbols are corrupt.");

  // Walk upward from the innermost lexical block for the current IP to collect local variables.
  // Using the map allows collecting only the innermost version of a given name, and sorts them as
  // we go.
  //
  // Need owning variable references to copy data out.
  //
  // Note that this does NOT skip "artificial" variables. In the standard these are marked as
  // compiler-generated and we should probably just skip them. The exception is for "this"
  // variables which we do want to show.
  //
  // Be aware that as of this writing there is Clang bug https://bugs.llvm.org/show_bug.cgi?id=49565
  // which marks the artificial flag on structured bindings incorrectly:
  //
  //   auto [a, b] = GetSomePair();
  //
  // It generates an unnamed std::pair variable without the DW_AT_artificial tag, and "a" and "b"
  // variables WITH the artificial tag. This is backwards from what one would expect and how GCC
  // encodes this (the internal generated variable should be marked artificial, and the ones the
  // user named should not be).
  //
  // Our behavior of showing artificial variables but hiding unnamed ones worked around this bug.
  // It's not clear what other cases in C++ there might be for artificial variables.
  std::map<std::string, fxl::RefPtr<Variable>> vars;
  VisitLocalBlocks(function->GetMostSpecificChild(location.symbol_context(), location.address()),
                   [&vars](const CodeBlock* block) {
                     for (const auto& lazy_var : block->variables()) {
                       const Variable* var = lazy_var.Get()->As<Variable>();
                       if (!var)
                         continue;  // Symbols are corrupt.

                       const std::string& name = var->GetAssignedName();
                       if (name.empty())
                         continue;

                       if (!vars.contains(name))
                         vars[name] = RefPtrTo(var);  // New one.
                     }
                     return VisitResult::kContinue;
                   });

  // Add function parameters. Don't overwrite existing names in case of duplicates to duplicate the
  // shadowing rules of the language.
  for (const auto& param : function->parameters()) {
    const Variable* var = param.Get()->As<Variable>();
    if (!var)
      continue;  // Symbols are corrupt.

    const std::string& name = var->GetAssignedName();
    if (!name.empty() && !vars.contains(name))
      vars[name] = RefPtrTo(var);  // New one.
  }

  return vars;
}

}  // namespace zxdb
