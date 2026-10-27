// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_BRINGUP_BIN_CRITICAL_SERVICES_CRASHSVC_CRASHSVC_H_
#define SRC_BRINGUP_BIN_CRITICAL_SERVICES_CRASHSVC_CRASHSVC_H_

#include <fidl/fuchsia.io/cpp/wire.h>
#include <lib/async/cpp/wait.h>
#include <lib/fit/function.h>
#include <lib/zx/job.h>
#include <lib/zx/process.h>
#include <zircon/syscalls/exception.h>

#include <string_view>

// What crashsvc does with an exception, as decided by |exception_hook| below.
enum class ExceptionDisposition {
  // Print the crash and hand it to the exception handler.
  kPrintAndHandoff,
  // Hand the crash to the exception handler without printing it. Printing reads the crashed
  // process's memory, which blocks for as long as a stuck pager does.
  kHandoffNoPrint,
  // Keep the exception, and with it the crashed thread, without printing or handing it off, because
  // the hook is rebooting the system.
  kHoldUntilReboot,
};

// Initialize the crash service, this supersedes the standalone service with
// the same name that lived in zircon/system/core/crashsvc/crashsvc.cpp
// (/boot/bin/crashsvc) and ad-hoc microservice in devmgr that delegated to
// svchost. See https://fxbug.dev/42108157 for details.
//
// The job of this service is to handle exceptions that reached |root_job| and
// delegate the crash analysis to one of two services:
//
// - built-in : using system/ulib/inspector
// - remotely hosted: via FIDL interface call (fuchsia_exception_Handler).
//
// Which one depends if |exception_handler_svc| is a valid channel handle, which
// svchost sets depending on "use_system".
//
// |backtrace_request_hook|, if set, is called with the requesting process and thread name of each
// backtrace request. If it returns true, crashsvc just resumes the thread instead of printing
// backtraces.
//
// |exception_hook|, if set, is called with the process and report of each other exception, and
// decides what crashsvc does with it.
zx::result<std::unique_ptr<async::Wait>> start_crashsvc(
    async_dispatcher_t* dispatcher, zx::channel exception_channel,
    fidl::ClientEnd<fuchsia_io::Directory> exception_handler_svc,
    fit::function<bool(const zx::process& process, std::string_view thread_name)>
        backtrace_request_hook = nullptr,
    fit::function<ExceptionDisposition(const zx::process& process,
                                       const zx_exception_report_t& report)>
        exception_hook = nullptr);

#endif  // SRC_BRINGUP_BIN_CRITICAL_SERVICES_CRASHSVC_CRASHSVC_H_
