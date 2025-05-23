// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_PERFORMANCE_EXPERIMENTAL_PROFILER_SAMPLER_H_
#define SRC_PERFORMANCE_EXPERIMENTAL_PROFILER_SAMPLER_H_

#include <fidl/fuchsia.cpu.profiler/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/async/dispatcher.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/process.h>
#include <lib/zx/result.h>
#include <lib/zx/task.h>
#include <lib/zx/thread.h>
#include <lib/zxdump/elf-search.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <memory>
#include <unordered_map>
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
  std::vector<uint64_t> stack;
};

class Sampler : public fxl::RefCountedThreadSafe<Sampler> {
 public:
  Sampler(async_dispatcher_t* dispatcher, TargetTree targets,
          std::vector<fuchsia_cpu_profiler::SamplingConfig> sample_specs)
      : dispatcher_(dispatcher),
        targets_(std::move(targets)),
        sample_specs_(std::move(sample_specs)),
        weak_factory_(this) {}

  virtual zx::result<> Start(size_t buffer_size_mb);
  virtual zx::result<> Stop();

  // Return the information needed to symbolize the samples
  zx::result<profiler::SymbolizationContext> GetContexts();
  fxl::WeakPtr<Sampler> GetWeakPtr() { return weak_factory_.GetWeakPtr(); }

  std::unordered_map<zx_koid_t, std::vector<Sample>> GetSamples() { return samples_; }
  std::vector<zx::ticks> SamplingDurations() { return inspecting_durations_; }
  virtual zx::result<> AddTarget(JobTarget&& target);
  virtual ~Sampler() = default;

 protected:
  zx::result<> WatchTarget(const JobTarget& target);
  virtual void AddThread(std::vector<zx_koid_t> job_path, zx_koid_t pid, zx_koid_t tid,
                         zx::thread t);
  virtual void RemoveThread(std::vector<zx_koid_t> job_path, zx_koid_t pid, zx_koid_t tid);

  void CollectSamples(async_dispatcher_t* dispatcher, async::TaskBase* task, zx_status_t status);
  void CacheModules(const ProcessTarget& p);

  async_dispatcher_t* dispatcher_;
  TargetTree targets_;
  std::vector<fuchsia_cpu_profiler::SamplingConfig> sample_specs_;
  std::vector<zx::ticks> inspecting_durations_;
  std::unordered_map<zx_koid_t, std::vector<Sample>> samples_;

  // Watchers cannot be moved, so we need to box them
  std::unordered_map<zx_koid_t, std::unique_ptr<ProcessWatcher>> process_watchers_;
  std::unordered_map<zx_koid_t, std::unique_ptr<JobWatcher>> job_watchers_;
  std::map<zx_koid_t, std::map<std::vector<std::byte>, profiler::Module>> contexts_;

 private:
  elf_search::Searcher searcher_;
  fxl::WeakPtrFactory<Sampler> weak_factory_;
  async::TaskMethod<profiler::Sampler, &profiler::Sampler::CollectSamples> sample_task_{this};
};
}  // namespace profiler
#endif  // SRC_PERFORMANCE_EXPERIMENTAL_PROFILER_SAMPLER_H_
