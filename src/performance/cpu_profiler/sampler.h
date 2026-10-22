// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_PERFORMANCE_CPU_PROFILER_SAMPLER_H_
#define SRC_PERFORMANCE_CPU_PROFILER_SAMPLER_H_

#include <fidl/fuchsia.cpu.profiler/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/task.h>
#include <lib/async/dispatcher.h>
#include <lib/fit/function.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/process.h>
#include <lib/zx/result.h>
#include <lib/zx/task.h>
#include <lib/zx/thread.h>
#include <lib/zxdump/elf-search.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <functional>
#include <map>
#include <memory>
#include <optional>
#include <span>
#include <string_view>
#include <unordered_map>
#include <unordered_set>
#include <utility>
#include <vector>

#include <src/lib/unwinder/cfi_unwinder.h>
#include <src/lib/unwinder/fp_unwinder.h>
#include <src/lib/unwinder/fuchsia.h>
#include <src/lib/unwinder/unwind.h>

#include "job_watcher.h"
#include "process_watcher.h"
#include "src/lib/fxl/memory/ref_counted.h"
#include "src/lib/fxl/memory/weak_ptr.h"
#include "symbolization_context.h"
#include "targets.h"

namespace profiler {
struct Sample {
  zx_koid_t pid;
  zx_koid_t tid;
  std::span<const uint64_t> stack;
  zx::ticks timestamp;
  // TODO(https://fxbug.dev/466468564): Use std::variant<Backtrace, StackMemory> instead of separate
  // fields.
  std::span<const uint8_t> stack_memory;
};

using SampleCallback = std::function<void(const Sample&)>;

class Sampler : public fxl::RefCountedThreadSafe<Sampler> {
 public:
  Sampler(async_dispatcher_t* dispatcher, TargetTree targets,
          std::vector<fuchsia_cpu_profiler::SamplingConfig> sample_specs,
          SampleCallback sample_cb = nullptr)
      : dispatcher_(dispatcher),
        targets_(std::move(targets)),
        sample_specs_(std::move(sample_specs)),
        sample_cb_(std::move(sample_cb)),
        weak_factory_(this) {
    module_cache_thread_started_ = module_cache_loop_.StartThread("module-cache") == ZX_OK;
  }

  virtual zx::result<> Start(size_t buffer_size_mb);
  virtual zx::result<> Stop();

  // Asynchronously gathers the information needed to symbolize the samples.
  //
  // Process modules are read on a background thread so that `dispatcher_` keeps servicing the
  // process exception channels in the meantime: reading a process's memory may fault, and
  // resolving that fault may in turn require one of those exceptions to be handled.
  //
  // `callback` is invoked on `dispatcher_` with the result, after every module scan scheduled
  // before this call has completed. If the sampler is destroyed first, `callback` is dropped
  // without being invoked. Only one call may be outstanding at a time.
  void GetContexts(fit::callback<void(zx::result<profiler::SymbolizationContext>)> callback);
  fxl::WeakPtr<Sampler> GetWeakPtr() { return weak_factory_.GetWeakPtr(); }

  std::optional<std::string_view> GetProcessName(zx_koid_t pid) const {
    return targets_.GetProcessName(pid);
  }
  std::optional<std::string_view> GetThreadName(zx_koid_t tid) const {
    return targets_.GetThreadName(tid);
  }
  std::vector<zx::ticks> SamplingDurations() { return inspecting_durations_; }
  virtual zx::result<> AddTarget(JobTarget&& target);
  virtual ~Sampler();

 protected:
  zx::result<> WatchTargets();
  zx::result<> WatchTarget(const JobTarget& target);
  virtual void AddThread(std::vector<zx_koid_t> job_path, zx_koid_t pid, zx_koid_t tid,
                         zx::thread t);
  virtual void RemoveThread(std::vector<zx_koid_t> job_path, zx_koid_t pid, zx_koid_t tid);

  void CollectSamples(async_dispatcher_t* dispatcher, async::TaskBase* task, zx_status_t status);
  void CacheModules(const ProcessTarget& p);

  // Cache the modules of process `pid` on a dedicated background worker loop so that reading a
  // process's modules (e.g. for Starnix processes with large address spaces) never blocks
  // `dispatcher_`, which services process/thread exception channels.
  void ScheduleCacheModules(std::vector<zx_koid_t> job_path, zx_koid_t pid);

  // Runs `loop`, which must own `dispatcher_`, until every task already posted to the module cache
  // loop has run and the results it posted back to `dispatcher_` have been delivered.
  //
  // `loop` keeps running in the meantime rather than blocking on the module cache loop: a module
  // cache task (or the module cache thread starting) can wait on an exception that only
  // `dispatcher_` handles.
  void RunUntilModuleCachesDoneForTesting(async::Loop& loop);

  // The target tree is populated when the session is configured, which can take a long time for
  // large targets (e.g. the whole system), and processes and threads keep starting in the meantime.
  // These add the ones that started since then, so that they aren't silently left out of the
  // session.
  //
  // Call AddProcessesStartedSinceConfigure after watching the target jobs, and
  // AddThreadsStartedSinceConfigure after watching `process`, so that anything starting later is
  // reported by the watchers instead.
  void AddProcessesStartedSinceConfigure();
  void AddThreadsStartedSinceConfigure(std::span<const zx_koid_t> job_path,
                                       const ProcessTarget& process);

  async_dispatcher_t* dispatcher_;
  TargetTree targets_;
  std::vector<fuchsia_cpu_profiler::SamplingConfig> sample_specs_;
  std::vector<zx::ticks> inspecting_durations_;
  SampleCallback sample_cb_;

  // Watchers cannot be moved, so we need to box them
  std::unordered_map<zx_koid_t, std::unique_ptr<ProcessWatcher>> process_watchers_;
  std::unordered_map<zx_koid_t, std::unique_ptr<JobWatcher>> job_watchers_;
  std::map<zx_koid_t, std::map<std::vector<std::byte>, profiler::Module>> contexts_;
  // Processes with a ScheduleCacheModules task in flight.
  std::unordered_set<zx_koid_t> pending_module_caches_;

 private:
  using ModuleMap = std::map<std::vector<std::byte>, profiler::Module>;
  using ModuleScanResults = std::vector<std::pair<zx_koid_t, zx::result<ModuleMap>>>;

  // Runs on `dispatcher_` once a ScheduleCacheModules scan of process `pid` completes.
  void OnModulesCached(zx_koid_t pid, zx::result<ModuleMap> modules);
  // Runs on `dispatcher_` once the module scans started by GetContexts complete.
  void FinishGetContexts(ModuleScanResults modules);

  elf_search::Searcher searcher_;
  // Module scans run on this loop's thread, so that they never block `dispatcher_`. The tasks it
  // runs only touch `module_cache_searcher_` and post their results back to `dispatcher_`.
  async::Loop module_cache_loop_{&kAsyncLoopConfigNeverAttachToThread};
  bool module_cache_thread_started_ = false;
  elf_search::Searcher module_cache_searcher_;
  // The callback of the outstanding GetContexts call, if any.
  fit::callback<void(zx::result<profiler::SymbolizationContext>)> get_contexts_callback_;
  fxl::WeakPtrFactory<Sampler> weak_factory_;
  async::TaskMethod<profiler::Sampler, &profiler::Sampler::CollectSamples> sample_task_{this};
};
}  // namespace profiler
#endif  // SRC_PERFORMANCE_CPU_PROFILER_SAMPLER_H_
