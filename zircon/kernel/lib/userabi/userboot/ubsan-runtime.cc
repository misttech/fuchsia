// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/ubsan-custom/handlers.h>

#include "util.h"

// This (via <lib/ubsan-custom/handlers.h>) defines the custom ubsan runtime
// for userboot.  The ubsan:: functions below handle the details specific to
// the environment: how to print and how to panic.

ubsan::Report::Report(const char* check, const ubsan::SourceLocation& loc,  //
                      void* caller, void* frame) {
  if (loc.column == 0) {
    printl(*Debuglog(),
           "userboot: %s:%u: *** "
           "UndefinedBehaviorSanitizer CHECK FAILED *** %s (PC=%p, FP=%p)\n",
           loc.filename, loc.line, check, caller, frame);
  } else {
    printl(*Debuglog(),
           "userboot: %s:%u:%u: *** "
           "UndefinedBehaviorSanitizer CHECK FAILED *** %s (PC=%p, FP=%p)\n",
           loc.filename, loc.line, loc.column, check, caller, frame);
  }
}

// **Note:** //tools/testing/tefmocheck/string_in_log_check.go matches this
// exact fragment in console logs to flag reports that should not be ignored.
// So however this message changes, make sure that this fragment remains
// identical to the precise string tefmocheck matches.
#define SUMMARY_TEXT "SUMMARY: UndefinedBehaviorSanitizer"

ubsan::Report::~Report() {
  fail(*Debuglog(), "userboot: *** " SUMMARY_TEXT " ERRORS! Emergency crash! ***");
}

void ubsan::VPrintf(const char* fmt, va_list args) { vprintl(*Debuglog(), fmt, args); }
