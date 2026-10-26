// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_BRINGUP_BIN_CRITICAL_SERVICES_STORAGE_WATCHDOG_WEDGE_DUMP_H_
#define SRC_BRINGUP_BIN_CRITICAL_SERVICES_STORAGE_WATCHDOG_WEDGE_DUMP_H_

#include <lib/zx/job.h>
#include <lib/zx/process.h>

#include <string_view>

namespace storage_watchdog {

// Whether |process| runs any code from a pager-backed VMO (a package blob rather than bootfs).
bool HasPagerBackedCode(const zx::process& process);

// Writes |reason| and backtraces of the storage stack (fxfs, busy driver host and Starnix threads,
// and every thread blocked on a pager) to the debuglog as symbolizer markup. Then dies as
// a process critical to |root_job|: the kernel reboots with USERSPACE ROOT JOB TERMINATION, which
// keeps the debuglog in the crashlog, and feedback files the reboot as
// fuchsia-reboot-storage-watchdog-terminated.
//
// Nothing here waits on storage without a timeout, and a separate thread reboots after 60 s
// however far the dump got.
[[noreturn]] void DumpWedgeDiagnosticsAndReboot(std::string_view reason, zx::job root_job);

}  // namespace storage_watchdog

#endif  // SRC_BRINGUP_BIN_CRITICAL_SERVICES_STORAGE_WATCHDOG_WEDGE_DUMP_H_
