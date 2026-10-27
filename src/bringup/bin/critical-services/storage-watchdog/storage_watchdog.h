// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_BRINGUP_BIN_CRITICAL_SERVICES_STORAGE_WATCHDOG_STORAGE_WATCHDOG_H_
#define SRC_BRINGUP_BIN_CRITICAL_SERVICES_STORAGE_WATCHDOG_STORAGE_WATCHDOG_H_

#include <lib/zx/process.h>

#include <string_view>

namespace storage_watchdog {

// A debug watchdog for storage stalls.
//
// When one of fxfs's I/O requests has been blocked for 5 minutes, fxfs's I/O watchdog makes a
// backtrace request from a thread named after it ("fxfs-watchdog:<op>-300s"). crashsvc, which
// handles every backtrace request in the system, passes the requesting process and thread name to
// OnBacktraceRequest(). If the watchdog is armed, DumpWedgeDiagnosticsAndReboot() then writes
// backtraces to the debuglog and reboots in a way that keeps the debuglog in the crashlog.
//
// fxfs's I/O watchdog runs on fxfs's executor, which stops when fxfs's own threads block on page
// faults that need the stuck storage. Then the first sign is the kernel giving up on one of those
// faults (kernel.userpager.overtime_timeout_seconds, 5 minutes on Iris), which crashsvc sees as a
// fatal page fault with ZX_ERR_TIMED_OUT and passes to OnPagerTimeout().
//
// critical-services only arms it if fuchsia.diagnostics.StorageStallWatchdogEnabled is set, which
// assembly does for non-user builds on boards that provide fuchsia::storage_stall_watchdog.

// The name prefix of the threads whose backtrace requests mean that storage is stuck.
inline constexpr std::string_view kTriggerThreadPrefix = "fxfs-watchdog:";

// Arms the watchdog. Gets the root job now, while the system is healthy.
//
// Test only: with the boot argument storage-watchdog.simulate-stall-secs=<n> (e.g. on the assembly
// kernel command line), a thread named like fxfs's makes a backtrace request after n seconds.
// That reboots every boot, so remove it once done.
void Arm();

// Called for every backtrace request with the requesting process and thread name. If the watchdog
// is armed, the thread name starts with kTriggerThreadPrefix, and the process is fxfs (or
// critical-services itself during SimulateStall), starts the dump and reboot on another thread,
// once, and returns true; the caller should just resume the requesting thread.
bool OnBacktraceRequest(const zx::process& process, std::string_view thread_name);

// Called for every fatal page fault that failed with ZX_ERR_TIMED_OUT, i.e. the kernel stopped
// waiting for a pager. If the watchdog is armed and the process is fxfs, starts the dump and reboot
// on another thread, once, and returns true; the caller should keep the exception until the reboot.
bool OnPagerTimeout(const zx::process& process);

}  // namespace storage_watchdog

#endif  // SRC_BRINGUP_BIN_CRITICAL_SERVICES_STORAGE_WATCHDOG_STORAGE_WATCHDOG_H_
