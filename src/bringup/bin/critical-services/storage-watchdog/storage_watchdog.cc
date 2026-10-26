// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/bringup/bin/critical-services/storage-watchdog/storage_watchdog.h"

#include <fidl/fuchsia.boot/cpp/wire.h>
#include <fidl/fuchsia.kernel/cpp/wire.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/zx/job.h>
#include <lib/zx/process.h>
#include <lib/zx/thread.h>
#include <stdlib.h>
#include <zircon/syscalls/object.h>

#include <atomic>
#include <optional>
#include <print>
#include <string>
#include <thread>

#include "src/bringup/bin/critical-services/storage-watchdog/wedge_dump.h"
#include "src/lib/debug/backtrace-request.h"

// Zircon C headers define ZX_INFO_* with signed (1 << N).
// NOLINTBEGIN(bugprone-signed-bitwise)
namespace storage_watchdog {
namespace {

// Set once by Arm(), before |armed| is.
zx::job root_job;
std::atomic<bool> armed = false;
std::atomic<bool> triggered = false;

zx::job GetRootJob() {
  zx::result client = component::Connect<fuchsia_kernel::RootJob>();
  if (client.is_error()) {
    return {};
  }
  fidl::WireResult result = fidl::WireCall(*client)->Get();
  return result.ok() ? std::move(result.value().job) : zx::job();
}

std::optional<zx::duration> SimulateStallDelay() {
  zx::result client = component::Connect<fuchsia_boot::Arguments>();
  if (client.is_error()) {
    return std::nullopt;
  }
  fidl::WireResult result =
      fidl::WireCall(*client)->GetString("storage-watchdog.simulate-stall-secs");
  if (!result.ok() || result->value.is_null()) {
    return std::nullopt;
  }
  const std::string text(result->value.get());
  char* end = nullptr;
  const uint64_t secs = strtoull(text.c_str(), &end, 10);
  if (end == text.c_str() || *end != '\0') {
    return std::nullopt;
  }
  return zx::sec(static_cast<int64_t>(secs));
}

// Does what fxfs's I/O watchdog does when an I/O request has been blocked for 5 minutes.
void SimulateStall(zx::duration delay) {
  zx::nanosleep(zx::deadline_after(delay));
  constexpr char kName[] = "fxfs-watchdog:simulated-stall";
  zx::thread::self()->set_property(ZX_PROP_NAME, kName, sizeof(kName) - 1);
  std::println("storage-watchdog: simulating a storage stall");
  backtrace_request_all_threads();
}

zx_koid_t GetKoid(zx_handle_t handle) {
  zx_info_handle_basic_t info = {};
  zx_object_get_info(handle, ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr);
  return info.koid;
}

std::string GetName(zx_handle_t handle) {
  char name[ZX_MAX_NAME_LEN] = {};
  zx_object_get_property(handle, ZX_PROP_NAME, name, sizeof(name));
  return name;
}

// ZX_PROP_NAME is writable by any process with ZX_RIGHT_SET_PROPERTY on its own thread or process
// handle (including sandboxed processes), so verify that the caller is either critical-services
// itself (for SimulateStall) or a bootfs fxfs.cm holding a kernel pager object. fxfs maps some of
// the pager-backed VMOs it serves, so only its code has to come from bootfs.
bool IsTrustedCaller(const zx::process& process) {
  const zx_koid_t koid = GetKoid(process.get());
  if (koid != ZX_KOID_INVALID && koid == GetKoid(zx::process::self()->get())) {
    return true;
  }
  if (GetName(process.get()) != "fxfs.cm") {
    return false;
  }
  zx_info_process_handle_stats_t stats = {};
  if (process.get_info(ZX_INFO_PROCESS_HANDLE_STATS, &stats, sizeof(stats), nullptr, nullptr) !=
          ZX_OK ||
      stats.handle_count[ZX_OBJ_TYPE_PAGER] == 0) {
    return false;
  }
  return !HasPagerBackedCode(process);
}

}  // namespace

void Arm() {
  root_job = GetRootJob();
  if (!root_job.is_valid()) {
    std::println("storage-watchdog: no root job, not arming");
    return;
  }
  armed = true;
  std::println("storage-watchdog: armed");
  if (std::optional delay = SimulateStallDelay()) {
    std::thread(SimulateStall, *delay).detach();
  }
}

bool OnBacktraceRequest(const zx::process& process, std::string_view thread_name) {
  if (!armed || !thread_name.starts_with(kTriggerThreadPrefix) || !IsTrustedCaller(process)) {
    return false;
  }
  if (!triggered.exchange(true)) {
    std::string reason = "fxfs I/O watchdog fired (" + std::string(thread_name) + ")";
    std::thread([reason = std::move(reason)] {
      DumpWedgeDiagnosticsAndReboot(reason, std::move(root_job));
    }).detach();
  }
  return true;
}

}  // namespace storage_watchdog
// NOLINTEND(bugprone-signed-bitwise)
