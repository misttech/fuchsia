// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "stack_sampler.h"

#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>
#include <lib/zx/clock.h>
#include <lib/zx/result.h>
#include <lib/zx/suspend_token.h>
#include <zircon/errors.h>
#include <zircon/syscalls-next.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/debug.h>
#include <zircon/syscalls/object.h>

#include <charconv>
#include <cstdint>
#include <cstring>
#include <vector>

#include <src/lib/unwinder/fuchsia.h>
#include <src/lib/unwinder/registers.h>
#include <src/lib/unwinder/unwind.h>

namespace profiler {
constexpr size_t kMaxUnwindDepth = 50;
constexpr size_t kStackCaptureSize = 4096ul * 4;  // 16 kib

zx::result<> StackSampler::Start(size_t buffer_size_mb) {
  TRACE_DURATION("cpu_profiler", __PRETTY_FUNCTION__);

  // TODO(https://fxbug.dev/468419097): Deduplicate watcher code.
  zx::result<> res = targets_.ForEachProcess([this](std::span<const zx_koid_t> job_path,
                                                    const ProcessTarget& p) -> zx::result<> {
    TRACE_DURATION("cpu_profiler", "StackSampler::Start/ForEachProcess");
    std::vector<zx_koid_t> saved_path{job_path.begin(), job_path.end()};

    auto process_watcher = std::make_unique<ProcessWatcher>(
        p.handle.borrow(),
        [saved_path, this](zx_koid_t pid, zx_koid_t tid, zx::thread t) {
          AddThread(saved_path, pid, tid, std::move(t));
        },
        [saved_path, this](zx_koid_t pid, zx_koid_t tid) { RemoveThread(saved_path, pid, tid); });

    auto [it, emplaced] = process_watchers_.emplace(p.pid, std::move(process_watcher));
    if (emplaced) {
      if (zx::result watch_result = it->second->Watch(dispatcher_); watch_result.is_error()) {
        FX_PLOGS(WARNING, watch_result.status_value()) << "Failed to watch process: " << p.pid;
      }
    }
    return zx::ok();
  });

  if (res.is_error()) {
    return res;
  }

  if (auto res = targets_.ForEachJob([this](const JobTarget& target) {
        if (zx::result res = WatchTarget(target); res.is_error()) {
          FX_PLOGS(WARNING, res.status_value())
              << "Failed to watch job [" << target.job_id << "] and its children";
        }
        return zx::ok();
      });
      res.is_error()) {
    return res;
  }

  return zx::make_result(sample_task_.Post(dispatcher_));
}

zx::result<> StackSampler::Stop() {
  TRACE_DURATION("cpu_profiler", __PRETTY_FUNCTION__);
  return zx::make_result(sample_task_.Cancel());
}

void StackSampler::AddThread(std::vector<zx_koid_t> job_path, zx_koid_t pid, zx_koid_t tid,
                             zx::thread t) {
  TRACE_DURATION("cpu_profiler", __PRETTY_FUNCTION__);
  std::string thread_name = profiler::GetThreadName(t);
  zx::result res = targets_.AddThread(
      job_path, pid,
      ThreadTarget{.handle = std::move(t), .tid = tid, .name = std::move(thread_name)});
  if (res.is_error()) {
    FX_PLOGS(ERROR, res.status_value()) << "Failed to add thread to session: " << tid;
  }
}

void StackSampler::PopulateRestrictedStateAddrs(const ProcessTarget& target) {
  constexpr char kRestrictedStateVmoPrefix[] = "restricted_state_vmo:";
  constexpr size_t kPrefixLen = sizeof(kRestrictedStateVmoPrefix) - 1;

  for (const zx_info_maps& map : target.cached_mappings) {
    if (strncmp(map.name, kRestrictedStateVmoPrefix, kPrefixLen) == 0) {
      const char* tid_str = map.name + kPrefixLen;
      zx_koid_t tid;
      auto result = std::from_chars(tid_str, map.name + sizeof(map.name), tid);
      if (result.ec != std::errc()) {
        continue;
      }
      auto it = target.threads.find(tid);
      if (it != target.threads.end()) {
        it->second.restricted_state_addr = map.base;
      }
    }
  }
}

zx::result<> StackSampler::RefreshMappings(const ProcessTarget& target) {
  size_t actual = 0;
  size_t avail = 0;
  zx_status_t status = target.handle.get_info(ZX_INFO_PROCESS_MAPS, nullptr, 0, &actual, &avail);
  if (status != ZX_OK) {
    FX_PLOGS(ERROR, status) << "Process has exited. Failed to get mappings for process: "
                            << target.pid;
    target.cached_mappings.clear();
    return zx::error(status);
  }
  target.cached_mappings.resize(avail);
  status = target.handle.get_info(ZX_INFO_PROCESS_MAPS, target.cached_mappings.data(),
                                  target.cached_mappings.size() * sizeof(zx_info_maps_t), &actual,
                                  &avail);
  if (status != ZX_OK) {
    FX_PLOGS(ERROR, status) << "Process has exited. Failed to get mappings for process: "
                            << target.pid;
    target.cached_mappings.clear();
    return zx::error(status);
  }
  if (actual < avail) {
    target.cached_mappings.resize(actual);
  }
  PopulateRestrictedStateAddrs(target);
  return zx::ok();
}

void StackSampler::GetRestrictedSP(const zx_restricted_state_t& restricted_state, uint64_t& sp) {
#if defined(__aarch64__)
  sp = restricted_state.sp;
#elif defined(__x86_64__)
  sp = restricted_state.rsp;
#elif defined(__riscv)
  sp = restricted_state.sp;
#else
#error "unsupported architecture"
#endif
}

void StackSampler::CollectSamples(async_dispatcher_t* dispatcher, async::TaskBase* task,
                                  zx_status_t status) {
  TRACE_DURATION("cpu_profiler", __PRETTY_FUNCTION__);
  if (status != ZX_OK) {
    return;
  }

  zx::result res = targets_.ForEachProcess([this](std::span<const zx_koid_t>,
                                                  const ProcessTarget& target) -> zx::result<> {
    TRACE_DURATION("cpu_profiler", "StackSampler::CollectSamples/ForEachProcess");
    // Get mappings once per process
    if (zx::result<> res = RefreshMappings(target); res.is_error()) {
      FX_PLOGS(WARNING, res.status_value()) << "Failed to get mappings for process: " << target.pid;
      return zx::ok();
    }

    unwinder::Unwinder unwinder(target.unwinder_data->modules);
    std::vector<Sample>& process_samples = samples_[target.pid];
    for (const auto& [tid, thread] : target.threads) {
      zx_info_thread_t thread_info;
      zx_status_t status = thread.handle.get_info(ZX_INFO_THREAD, &thread_info, sizeof(thread_info),
                                                  nullptr, nullptr);
      if (status != ZX_OK) {
        FX_PLOGS(ERROR, status) << "unable to get thread info for thread " << thread.handle.get()
                                << ", skipping";
        continue;  // Skip this thread.
      }
      // Skip threads that are not actively running or blocked
      if (thread_info.state != ZX_THREAD_STATE_RUNNING) {
        continue;
      }

      // Suspend thread
      zx::suspend_token suspend_token;
      status = thread.handle.suspend(&suspend_token);
      if (status != ZX_OK) {
        FX_PLOGS(ERROR, status) << "Failed to suspend thread: " << tid;
        continue;
      }

      zx_signals_t signals = ZX_THREAD_SUSPENDED | ZX_THREAD_TERMINATED;
      zx_signals_t observed = 0;
      status = thread.handle.wait_one(signals, zx::deadline_after(zx::msec(100)), &observed);
      if (status != ZX_OK || (observed & ZX_THREAD_TERMINATED)) {
        continue;
      }
      zx::ticks tick_suspended = zx::ticks::now();

      // Get registers
      zx_thread_state_general_regs_t regs;
      if (thread.handle.read_state(ZX_THREAD_STATE_GENERAL_REGS, &regs, sizeof(regs)) != ZX_OK) {
        continue;
      }

      unwinder::Registers registers = unwinder::FromFuchsiaRegisters(regs);

      uint64_t sp = 0;
      if (registers.GetSP(sp).has_err()) {
        continue;
      }

      BufferedStackMemory stack_memory(target.handle.borrow(), {}, target.cached_mappings);
      stack_memory.CaptureStack(sp, kStackCaptureSize);

      if (thread.restricted_state_addr.has_value()) {
        uint64_t restricted_base = thread.restricted_state_addr.value();

        // Capture the memory containing the restricted state struct
        if (stack_memory.CaptureStack(restricted_base, sizeof(zx_restricted_state_t)) == ZX_OK) {
          zx_restricted_state_t restricted_state;
          // Read from local buffer
          if (!stack_memory.ReadBytes(restricted_base, sizeof(restricted_state), &restricted_state)
                   .has_err()) {
            GetRestrictedSP(restricted_state, sp);
            stack_memory.CaptureStack(sp, kStackCaptureSize);
          } else {
            thread.restricted_state_addr.reset();
          }
        } else {
          thread.restricted_state_addr.reset();
        }
      }
      suspend_token.reset();
      zx::ticks tick_resume = zx::ticks::now();
      std::vector<uint64_t> stack;

      {
        TRACE_DURATION("cpu_profiler", "StackSampler::CollectSamples/Unwind");

        std::vector<unwinder::Frame> frames =
            unwinder.Unwind(&stack_memory, registers, kMaxUnwindDepth);
        for (const unwinder::Frame& frame : frames) {
          uint64_t pc;
          if (!frame.regs.GetPC(pc).has_err() && pc != 0) {
            stack.push_back(pc);
          }
        }
      }

      if (!stack.empty()) {
        process_samples.push_back({.pid = target.pid,
                                   .tid = tid,
                                   .stack = std::move(stack),
                                   .timestamp = zx::ticks::now(),
                                   .stack_memory = {}});
      }

      inspecting_durations_.push_back(tick_resume - tick_suspended);
    }
    return zx::ok();
  });

  if (res.is_error()) {
    FX_PLOGS(ERROR, res.status_value()) << "Stack Sampling Failed";
    return;
  }

  zx::duration period = zx::msec(10);
  if (!sample_specs_.empty() && sample_specs_[0].period().has_value()) {
    period = zx::nsec(sample_specs_[0].period().value());
  }
  sample_task_.PostDelayed(dispatcher_, period);
}
}  // namespace profiler
