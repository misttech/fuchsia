// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "stack_sampler.h"

#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>
#include <lib/zx/suspend_token.h>
#include <zircon/syscalls/debug.h>

#include <algorithm>

#include <src/lib/unwinder/fuchsia.h>
#include <src/lib/unwinder/registers.h>

#include "lib/zx/result.h"

namespace profiler {

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
  zx::result res =
      targets_.AddThread(job_path, pid, ThreadTarget{.handle = std::move(t), .tid = tid});
  if (res.is_error()) {
    FX_PLOGS(ERROR, res.status_value()) << "Failed to add thread to session: " << tid;
  }
}

void StackSampler::CollectSamples(async_dispatcher_t* dispatcher, async::TaskBase* task,
                                  zx_status_t status) {
  TRACE_DURATION("cpu_profiler", __PRETTY_FUNCTION__);
  if (status != ZX_OK) {
    return;
  }

  zx::result res = targets_.ForEachProcess([this](std::span<const zx_koid_t>,
                                                  const ProcessTarget& target) {
    for (const auto& [tid, thread] : target.threads) {
      TRACE_DURATION("cpu_profiler", "StackSampler::CollectSamples/ForEachThread");

      // Suspend thread
      zx::suspend_token suspend_token;
      zx_status_t status = thread.handle.suspend(&suspend_token);
      if (status != ZX_OK) {
        continue;
      }

      zx_signals_t signals = ZX_THREAD_SUSPENDED | ZX_THREAD_TERMINATED;
      zx_signals_t observed = 0;
      status = thread.handle.wait_one(signals, zx::deadline_after(zx::msec(100)), &observed);
      if (status != ZX_OK || (observed & ZX_THREAD_TERMINATED)) {
        continue;
      }

      // Get registers
      zx_thread_state_general_regs_t regs;
      if (thread.handle.read_state(ZX_THREAD_STATE_GENERAL_REGS, &regs, sizeof(regs)) != ZX_OK) {
        continue;
      }

      auto registers = unwinder::FromFuchsiaRegisters(regs);
      uint64_t sp = 0;
      if (registers.GetSP(sp).has_err()) {
        continue;
      }

      // Get mappings
      size_t actual = 0;
      size_t avail = 0;

      // Check mappings size
      status = target.handle.get_info(ZX_INFO_PROCESS_MAPS, nullptr, 0, &actual, &avail);
      if (status != ZX_OK) {
        continue;
      }

      // NOTE/RISK: Even though the target thread is suspended, other threads in the process
      // might still be running. They can add/remove mappings (e.g., mmap, dlopen)
      // between the size check above and the data fetch below.
      //
      // If the map list grows during this window, 'actual' will be less than the new 'avail',
      // and the kernel will truncate the list. If our stack mapping happens to be at the
      // end of that list, we might miss it.
      // TODO(https://fxbug.dev/467127240): Fix TOCTOU race condition in process mapping retrieval

      // Fetch mappings data
      std::vector<zx_info_maps_t> maps(avail);
      status = target.handle.get_info(ZX_INFO_PROCESS_MAPS, maps.data(),
                                      maps.size() * sizeof(zx_info_maps_t), &actual, &avail);
      if (status != ZX_OK) {
        continue;
      }

      zx_info_maps_t* stack_map = nullptr;
      // TODO(https://fxbug.dev/467162834): Apply Cache for stack mappings
      for (auto& map : maps) {
        if (map.type == ZX_INFO_MAPS_TYPE_MAPPING && sp >= map.base && sp < map.base + map.size) {
          stack_map = &map;
          break;
        }
      }

      if (stack_map) {
        // TODO(https://fxbug.dev/466469604): Update size to a real value.
        size_t wanted_size = 4096;
        // Stack pointer go decrementing/grow down.
        size_t used_stack_size = (stack_map->base + stack_map->size) - sp;
        size_t copy_len = std::min(wanted_size, used_stack_size);
        std::vector<uint8_t> stack_copy(copy_len);
        size_t actual_read = 0;
        status = target.handle.read_memory(sp, stack_copy.data(), copy_len, &actual_read);
        if (status == ZX_OK) {
          stack_copy.resize(actual_read);
          samples_[target.pid].push_back({.pid = target.pid,
                                          .tid = tid,
                                          .stack = {},
                                          .timestamp = zx::ticks::now(),
                                          .stack_memory = std::move(stack_copy)});
        }
      }
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
