// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "sampler.h"

#include <lib/async/cpp/task.h>
#include <lib/async/dispatcher.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>
#include <lib/zx/process.h>
#include <lib/zx/result.h>
#include <lib/zx/suspend_token.h>
#include <lib/zx/thread.h>
#include <lib/zx/time.h>
#include <zircon/errors.h>
#include <zircon/status.h>
#include <zircon/types.h>

#include <cstddef>
#include <cstdint>
#include <map>
#include <memory>
#include <span>
#include <unordered_map>
#include <utility>
#include <vector>

#include <src/lib/unwinder/fp_unwinder.h>
#include <src/lib/unwinder/registers.h>
#include <src/lib/unwinder/unwind.h>

#include "job_watcher.h"
#include "process_watcher.h"
#include "symbolization_context.h"
#include "targets.h"

namespace {
std::pair<zx::ticks, std::vector<uint64_t>> SampleThread(const zx::unowned_process& process,
                                                         const zx::unowned_thread& thread,
                                                         unwinder::FramePointerUnwinder& unwinder) {
  TRACE_DURATION("cpu_profiler", __PRETTY_FUNCTION__);
  zx_info_thread_t thread_info;
  zx_status_t status =
      thread->get_info(ZX_INFO_THREAD, &thread_info, sizeof(thread_info), nullptr, nullptr);
  if (status != ZX_OK) {
    FX_PLOGS(ERROR, status) << "unable to get thread info for thread " << thread->get()
                            << ", skipping";
    return {zx::ticks(), std::vector<uint64_t>()};  // Skip this thread.
  }

  // Skip threads that are not actively running or blocked
  if (thread_info.state != ZX_THREAD_STATE_RUNNING) {
    return {zx::ticks(), std::vector<uint64_t>()};
  }

  zx::ticks before = zx::ticks::now();
  zx::suspend_token suspend_token;
  status = thread->suspend(&suspend_token);
  if (status != ZX_OK) {
    FX_PLOGS(WARNING, status) << "Failed to suspend thread: " << thread->get();
    return {zx::ticks(), std::vector<uint64_t>()};  // Skip this thread.
  }

  // Asking to wait for suspended means only waiting for the thread to suspend. If the thread
  // terminates instead this will wait forever (or until the timeout). Thus we need to explicitly
  // wait for ZX_THREAD_TERMINATED too.
  zx_signals_t signals = ZX_THREAD_SUSPENDED | ZX_THREAD_TERMINATED;
  zx_signals_t observed = 0;
  zx::time deadline = zx::deadline_after(zx::msec(100));
  status = thread->wait_one(signals, deadline, &observed);

  if (status != ZX_OK) {
    FX_PLOGS(WARNING, status) << "failure waiting for thread to suspend, skipping thread: "
                              << thread->get();
    return {zx::ticks(), std::vector<uint64_t>()};  // Skip this thread.
  }

  if (observed & ZX_THREAD_TERMINATED) {
    FX_LOGS(INFO) << "Skipping terminated thread...";
    return {zx::ticks(), std::vector<uint64_t>()};  // Skip this thread.
  }
  unwinder::FuchsiaMemory memory(process->get());

  // Setup registers.
  zx_thread_state_general_regs_t regs;
  if (thread->read_state(ZX_THREAD_STATE_GENERAL_REGS, &regs, sizeof(regs)) != ZX_OK) {
    FX_LOGS(INFO) << "Can't read state, Skipping thread...";
    return {zx::ticks(), std::vector<uint64_t>()};
  }
  auto registers = unwinder::FromFuchsiaRegisters(regs);
  const auto& frames = unwinder.Unwind(&memory, registers);

  std::vector<uint64_t> pcs;
  pcs.reserve(frames.size());
  std::ranges::transform(frames, std::back_inserter(pcs), [](const unwinder::Frame& frame) {
    uint64_t pc;
    // This will always succeed - the unwinder will not return frames that do not contain PC.
    std::ignore = frame.regs.GetPC(pc);
    return pc;
  });

  zx::ticks duration = zx::ticks::now() - before;
  return {duration, pcs};
}
}  // namespace

zx::result<> profiler::Sampler::AddTarget(JobTarget&& target) {
  zx::result<> res = WatchTarget(target);
  if (res.is_error()) {
    return res;
  }

  return targets_.AddJob(std::move(target));
}

zx::result<> profiler::Sampler::WatchTarget(const JobTarget& target) {
  TRACE_DURATION("cpu_profiler", __PRETTY_FUNCTION__);
  std::vector<zx_koid_t> job_path = target.ancestry;
  job_path.push_back(target.job_id);
  auto job_watcher = std::make_unique<JobWatcher>(
      target.job.borrow(), [job_path = std::move(job_path), this](zx_koid_t pid, zx::process p) {
        // We've intercepted this process before its threads have started, so we don't recursively
        // add them here. We let the watcher handle the thread start exceptions as soon as we
        // acknowledge this process start exception.
        char name[ZX_MAX_NAME_LEN];
        if (zx_status_t status = p.get_property(ZX_PROP_NAME, name, sizeof(name));
            status != ZX_OK) {
          name[0] = '\0';
        }
        ProcessTarget process_target{std::move(p), pid, std::string(name),
                                     std::unordered_map<zx_koid_t, ThreadTarget>()};
        // Furthermore, we need to watch each started process for threads it creates
        auto process_watcher = std::make_unique<ProcessWatcher>(
            process_target.handle.borrow(),
            [job_path, this](zx_koid_t pid, zx_koid_t tid, zx::thread t) {
              AddThread(job_path, pid, tid, std::move(t));
            },
            [job_path, this](zx_koid_t pid, zx_koid_t tid) { RemoveThread(job_path, pid, tid); }

        );
        auto [it, emplaced] = process_watchers_.emplace(pid, std::move(process_watcher));
        if (!emplaced) {
          // We're already tracking this process. This happens when several watched jobs are
          // ancestors of the process (each of them is notified), or when the process was found by
          // AddProcessesStartedSinceConfigure.
          return;
        }
        if (zx::result watch_result = it->second->Watch(dispatcher_); watch_result.is_error()) {
          if (watch_result.error_value() == ZX_ERR_BAD_STATE) {
            FX_LOGS(DEBUG) << "Process terminated before being watched.";
          } else {
            FX_PLOGS(ERROR, watch_result.status_value()) << "Failed to watch process: " << pid;
            job_watchers_.clear();
            process_watchers_.clear();
            return;
          }
        }

        if (zx::result res = targets_.AddProcess(job_path, std::move(process_target));
            res.is_error()) {
          FX_PLOGS(ERROR, res.status_value()) << "Failed to add process to session: " << pid;
        }
      });

  auto [it, emplaced] = job_watchers_.emplace(target.job_id, std::move(job_watcher));
  if (emplaced) {
    if (zx::result res = it->second->Watch(dispatcher_); res.is_error()) {
      if (res.error_value() == ZX_ERR_BAD_STATE) {
        FX_LOGS(DEBUG) << "Job terminated before being watched.";
      } else {
        FX_PLOGS(ERROR, res.status_value()) << "Failed to watch job : " << target.job_id;
        job_watchers_.clear();
        return res;
      }
    }
  }
  return zx::ok();
}

zx::result<> profiler::Sampler::WatchTargets() {
  TRACE_DURATION("cpu_profiler", __PRETTY_FUNCTION__);
  // When we start, we want to recursively attach to each process and job we've found. In addition,
  // we want to set up a notification for new jobs and processes so that if any watched task creates
  // a sub task, we also attach to it and watch it.
  //
  // A job or process may have exited between us configuring our target tree and actually starting,
  // so if we find we're unable to attach to a task, we simply move on without it.
  if (auto res = targets_.ForEachJob([this](const JobTarget& target) {
        if (zx::result res = WatchTarget(target); res.is_error()) {
          // If a job exited out from underneath us, simply move on without it.
          FX_PLOGS(WARNING, res.status_value())
              << "Failed to watch job [" << target.job_id << "] and its children";
        }
        return zx::ok();
      });
      res.is_error()) {
    return res;
  }
  AddProcessesStartedSinceConfigure();

  return targets_.ForEachProcess([this](std::span<const zx_koid_t> job_path,
                                        const ProcessTarget& p) -> zx::result<> {
    TRACE_DURATION("cpu_profiler", "Sampler::WatchTargets/ForEachProcess");
    std::vector<zx_koid_t> saved_path{job_path.begin(), job_path.end()};

    if (!p.modules.empty()) {
      contexts_.try_emplace(p.pid, p.modules);
    } else {
      ScheduleCacheModules(saved_path, p.pid);
    }

    if (!p.full_process) {
      return zx::ok();
    }

    auto process_watcher = std::make_unique<ProcessWatcher>(
        p.handle.borrow(),
        [saved_path, this](zx_koid_t pid, zx_koid_t tid, zx::thread t) {
          AddThread(saved_path, pid, tid, std::move(t));
        },
        [saved_path, this](zx_koid_t pid, zx_koid_t tid) { RemoveThread(saved_path, pid, tid); });

    auto [it, emplaced] = process_watchers_.emplace(p.pid, std::move(process_watcher));
    if (emplaced) {
      if (zx::result watch_result = it->second->Watch(dispatcher_); watch_result.is_error()) {
        // A watch may fail for a number of reason, possibly the process exited out from under
        // us. Simply move on without it and attempt to watch the remaining processes.
        FX_PLOGS(WARNING, watch_result.status_value()) << "Failed to watch process: " << p.pid;
      }
    }
    AddThreadsStartedSinceConfigure(job_path, p);
    return zx::ok();
  });
}

zx::result<> profiler::Sampler::Start(size_t buffer_size_mb /* unused, we buffer in memory */) {
  TRACE_DURATION("cpu_profiler", __PRETTY_FUNCTION__);
  if (zx::result<> res = WatchTargets(); res.is_error()) {
    return res;
  }

  inspecting_durations_.reserve(1000);
  sample_task_.Post(dispatcher_);
  return zx::ok();
}

zx::result<> profiler::Sampler::Stop() {
  TRACE_DURATION("cpu_profiler", __PRETTY_FUNCTION__);
  sample_task_.Cancel();
  FX_LOGS(INFO) << "Stopped! Collected " << inspecting_durations_.size() << " samples";
  return zx::ok();
}

void profiler::Sampler::CollectSamples(async_dispatcher_t* dispatcher, async::TaskBase* task,
                                       zx_status_t status) {
  TRACE_DURATION("cpu_profiler", __PRETTY_FUNCTION__);
  if (status != ZX_OK) {
    return;
  }

  zx::result res =
      targets_.ForEachProcess([this](std::span<const zx_koid_t>, const ProcessTarget& target) {
        for (const auto& [_, thread] : target.threads) {
          TRACE_DURATION("cpu_profiler", "Sampler::CollectSamples/ForEachProcess");
          unwinder::ElfModuleCache modules{target.unwinder_data->modules};
          unwinder::FramePointerUnwinder fp_unwinder{modules};
          auto [time_sampling, pcs] =
              SampleThread(target.handle.borrow(), thread.handle.borrow(), fp_unwinder);
          if (time_sampling != zx::ticks()) {
            if (sample_cb_) {
              sample_cb_({target.pid, thread.tid, pcs, time_sampling, {}});
            }
            inspecting_durations_.push_back(time_sampling);
          }
        }
        return zx::ok();
      });
  if (res.is_error()) {
    FX_PLOGS(ERROR, res.status_value()) << "Sampling Failed";
    return;
  }

  zx::duration period = zx::msec(10);
  if (!sample_specs_.empty() && sample_specs_[0].period().has_value()) {
    period = zx::nsec(sample_specs_[0].period().value());
  }
  sample_task_.PostDelayed(dispatcher_, period);
}

profiler::Sampler::~Sampler() {
  process_watchers_.clear();
  module_cache_loop_.Shutdown();
}

void profiler::Sampler::GetContexts(
    fit::callback<void(zx::result<profiler::SymbolizationContext>)> callback) {
  TRACE_DURATION("cpu_profiler", __PRETTY_FUNCTION__);
  if (get_contexts_callback_) {
    callback(zx::error(ZX_ERR_BAD_STATE));
    return;
  }
  std::vector<std::pair<zx_koid_t, zx::process>> processes;
  zx::result<> res = targets_.ForEachProcess(
      [&processes](std::span<const zx_koid_t>, const ProcessTarget& target) -> zx::result<> {
        zx::process process;
        // It's possible that the process we were profiling no longer exists -- it exited before the
        // profile ended. If this happens, we don't want ForEachProcess to short circuit and stop,
        // so we return success even if we "failed".
        if (target.handle.duplicate(ZX_RIGHT_SAME_RIGHTS, &process) == ZX_OK) {
          processes.emplace_back(target.pid, std::move(process));
        }
        return zx::ok();
      });
  if (res.is_error()) {
    callback(res.take_error());
    return;
  }
  get_contexts_callback_ = std::move(callback);

  if (!module_cache_thread_started_) {
    ModuleScanResults modules;
    modules.reserve(processes.size());
    for (auto& [pid, process] : processes) {
      modules.emplace_back(pid, profiler::GetProcessModules(process, searcher_));
    }
    FinishGetContexts(std::move(modules));
    return;
  }

  // `module_cache_loop_` runs its tasks in order, so the scans posted by earlier
  // ScheduleCacheModules calls complete, and post their results back to `dispatcher_`, before this
  // one does.
  zx_status_t status = async::PostTask(
      module_cache_loop_.dispatcher(),
      [this, weak = GetWeakPtr(), processes = std::move(processes)]() mutable {
        TRACE_DURATION("cpu_profiler", "Sampler::GetContexts/GetProcessModules");
        ModuleScanResults modules;
        modules.reserve(processes.size());
        for (auto& [pid, process] : processes) {
          modules.emplace_back(pid, profiler::GetProcessModules(process, module_cache_searcher_));
        }
        async::PostTask(dispatcher_,
                        [weak = std::move(weak), modules = std::move(modules)]() mutable {
                          if (weak) {
                            weak->FinishGetContexts(std::move(modules));
                          }
                        });
      });
  if (status != ZX_OK) {
    std::exchange(get_contexts_callback_, nullptr)(zx::error(status));
  }
}

void profiler::Sampler::FinishGetContexts(ModuleScanResults modules) {
  for (auto& [pid, process_modules] : modules) {
    if (process_modules.is_ok()) {
      for (auto& [build_id, mod] : *process_modules) {
        contexts_[pid].try_emplace(build_id, std::move(mod));
      }
    }
  }
  if (get_contexts_callback_) {
    std::exchange(get_contexts_callback_,
                  nullptr)(zx::ok(profiler::SymbolizationContext{contexts_}));
  }
}

void profiler::Sampler::AddThread(std::vector<zx_koid_t> job_path, zx_koid_t pid, zx_koid_t tid,
                                  zx::thread t) {
  TRACE_DURATION("cpu_profiler", __PRETTY_FUNCTION__);
  // Before we start sampling the thread, make sure we've recorded information about its process
  if (!contexts_.contains(pid)) {
    ScheduleCacheModules(job_path, pid);
  }
  std::string thread_name = profiler::GetThreadName(t);
  zx::result res = targets_.AddThread(
      job_path, pid,
      ThreadTarget{.handle = std::move(t), .tid = tid, .name = std::move(thread_name)});
  if (res.is_error() && res.error_value() != ZX_ERR_ALREADY_EXISTS) {
    FX_PLOGS(ERROR, res.status_value()) << "Failed to add thread to session: " << tid;
  }
}

void profiler::Sampler::RemoveThread(std::vector<zx_koid_t> job_path, zx_koid_t pid,
                                     zx_koid_t tid) {
  TRACE_DURATION("cpu_profiler", __PRETTY_FUNCTION__);
  zx::result res = targets_.RemoveThread(job_path, pid, tid);
  if (res.is_error()) {
    FX_PLOGS(ERROR, res.status_value()) << "Failed to remove exited thread: " << tid;
  }
}

void profiler::Sampler::CacheModules(const ProcessTarget& p) {
  TRACE_DURATION("cpu_profiler", __PRETTY_FUNCTION__);
  if (contexts_.contains(p.pid)) {
    return;
  }
  if (!p.modules.empty()) {
    contexts_.try_emplace(p.pid, p.modules);
    return;
  }
  zx::result modules = profiler::GetProcessModules(p.handle, searcher_);
  if (modules.is_error()) {
    // Eat the error and log -- We _probably_ shouldn't see this in normal operation, but there
    // may be some cases where we race with an exiting processes and it's not a huge deal if we're
    // not able to get the modules.
    FX_PLOGS(ERROR, modules.status_value()) << "Failed to get modules for process: " << p.pid;
    return;
  }
  if (!modules->empty()) {
    contexts_[p.pid] = std::move(*modules);
  }
}

void profiler::Sampler::OnModulesCached(zx_koid_t pid, zx::result<ModuleMap> modules) {
  pending_module_caches_.erase(pid);
  if (!contexts_.contains(pid) && modules.is_ok() && !modules->empty()) {
    contexts_[pid] = std::move(*modules);
  }
}

void profiler::Sampler::RunUntilModuleCachesDoneForTesting(async::Loop& loop) {
  FX_CHECK(loop.dispatcher() == dispatcher_);
  if (!module_cache_thread_started_) {
    loop.RunUntilIdle();
    return;
  }
  bool done = false;
  // `module_cache_loop_` runs its tasks in order, so this one runs after every task posted to it
  // so far, and posts to `dispatcher_` after their results were posted.
  zx_status_t status =
      async::PostTask(module_cache_loop_.dispatcher(), [dispatcher = dispatcher_, &done]() {
        async::PostTask(dispatcher, [&done]() { done = true; });
      });
  FX_CHECK(status == ZX_OK) << zx_status_get_string(status);
  while (!done) {
    status = loop.Run(zx::time::infinite(), /*once=*/true);
    FX_CHECK(status == ZX_OK) << zx_status_get_string(status);
  }
}

void profiler::Sampler::ScheduleCacheModules(std::vector<zx_koid_t> job_path, zx_koid_t pid) {
  if (contexts_.contains(pid) || !pending_module_caches_.insert(pid).second) {
    return;
  }
  zx::result<ProcessTarget*> target = targets_.GetProcess(job_path, pid);
  if (target.is_error()) {
    // The process may have exited before we got to it.
    FX_PLOGS(DEBUG, target.status_value()) << "Failed to search up process: " << pid;
    pending_module_caches_.erase(pid);
    return;
  }
  if (!(*target)->modules.empty()) {
    contexts_.try_emplace(pid, (*target)->modules);
    pending_module_caches_.erase(pid);
    return;
  }
  zx::process process_dup;
  if (zx_status_t status = (*target)->handle.duplicate(ZX_RIGHT_SAME_RIGHTS, &process_dup);
      status != ZX_OK) {
    FX_PLOGS(DEBUG, status) << "Failed to duplicate process handle: " << pid;
    pending_module_caches_.erase(pid);
    return;
  }
  if (!module_cache_thread_started_) {
    pending_module_caches_.erase(pid);
    CacheModules(**target);
    return;
  }
  zx_status_t status = async::PostTask(
      module_cache_loop_.dispatcher(),
      [this, weak = GetWeakPtr(), pid, process = std::move(process_dup)]() mutable {
        TRACE_DURATION("cpu_profiler", "Sampler::ScheduleCacheModules/GetProcessModules");
        zx::result modules = profiler::GetProcessModules(process, module_cache_searcher_);
        if (modules.is_error()) {
          FX_PLOGS(ERROR, modules.status_value()) << "Failed to get modules for process: " << pid;
        }
        async::PostTask(dispatcher_,
                        [weak = std::move(weak), pid, modules = std::move(modules)]() mutable {
                          if (weak) {
                            weak->OnModulesCached(pid, std::move(modules));
                          }
                        });
      });
  if (status != ZX_OK) {
    pending_module_caches_.erase(pid);
  }
}

void profiler::Sampler::AddProcessesStartedSinceConfigure() {
  TRACE_DURATION("cpu_profiler", __PRETTY_FUNCTION__);
  std::vector<std::pair<std::vector<zx_koid_t>, zx::unowned_job>> jobs;
  std::unordered_set<zx_koid_t> known_jobs;
  (void)targets_.ForEachJob([&jobs, &known_jobs](const JobTarget& job) {
    std::vector<zx_koid_t> job_path = job.ancestry;
    job_path.push_back(job.job_id);
    jobs.emplace_back(std::move(job_path), job.job.borrow());
    known_jobs.insert(job.job_id);
    return zx::ok();
  });

  // `jobs` grows as new child jobs are discovered, visiting the whole subjob tree breadth-first.
  for (size_t i = 0; i < jobs.size(); ++i) {
    std::vector<zx_koid_t> job_path = jobs[i].first;
    zx::unowned_job job{jobs[i].second};

    std::vector<zx_koid_t> child_jids(64);
    size_t actual = 0;
    size_t avail = 0;
    zx_status_t status;
    for (;;) {
      status = job->get_info(ZX_INFO_JOB_CHILDREN, child_jids.data(),
                             child_jids.size() * sizeof(zx_koid_t), &actual, &avail);
      if (status != ZX_OK || actual == avail) {
        break;
      }
      child_jids.resize(avail + 16);
    }
    if (status != ZX_OK) {
      // The job may have exited.
      continue;
    }
    child_jids.resize(actual);

    for (zx_koid_t child_jid : child_jids) {
      if (known_jobs.contains(child_jid)) {
        continue;
      }
      zx::job child_job;
      if (job->get_child(child_jid, ZX_DEFAULT_JOB_RIGHTS, &child_job) != ZX_OK) {
        // The child job may have exited.
        continue;
      }
      zx::unowned_job unowned_child_job = child_job.borrow();
      JobTarget child_target{std::move(child_job), child_jid, job_path};
      if (zx::result watch_res = WatchTarget(child_target); watch_res.is_error()) {
        FX_PLOGS(WARNING, watch_res.status_value())
            << "Failed to watch job [" << child_jid << "] and its children";
      }
      FX_LOGS(DEBUG) << "Adding job started during configuration: " << child_jid;
      if (zx::result res = targets_.AddJob(job_path, std::move(child_target)); res.is_error()) {
        FX_PLOGS(WARNING, res.status_value()) << "Failed to add job to session: " << child_jid;
        continue;
      }
      known_jobs.insert(child_jid);
      std::vector<zx_koid_t> child_job_path = job_path;
      child_job_path.push_back(child_jid);
      jobs.emplace_back(std::move(child_job_path), std::move(unowned_child_job));
    }

    std::vector<zx_koid_t> pids(64);
    actual = 0;
    avail = 0;
    for (;;) {
      status = job->get_info(ZX_INFO_JOB_PROCESSES, pids.data(), pids.size() * sizeof(zx_koid_t),
                             &actual, &avail);
      if (status != ZX_OK || actual == avail) {
        break;
      }
      pids.resize(avail + 16);
    }
    if (status != ZX_OK) {
      // The job may have exited.
      continue;
    }
    pids.resize(actual);

    for (zx_koid_t pid : pids) {
      if (targets_.GetProcess(job_path, pid).is_ok()) {
        continue;
      }
      zx::process process;
      if (job->get_child(pid, ZX_DEFAULT_PROCESS_RIGHTS, &process) != ZX_OK) {
        // The process may have exited.
        continue;
      }
      zx::result<ProcessTarget> process_target =
          MakeProcessTargetWithoutModules(std::move(process));
      if (process_target.is_error()) {
        continue;
      }
      FX_LOGS(DEBUG) << "Adding process started during configuration: " << pid;
      if (zx::result res = targets_.AddProcess(job_path, std::move(*process_target));
          res.is_error()) {
        FX_PLOGS(WARNING, res.status_value()) << "Failed to add process to session: " << pid;
      }
    }
  }
}

void profiler::Sampler::AddThreadsStartedSinceConfigure(std::span<const zx_koid_t> job_path,
                                                        const ProcessTarget& process) {
  TRACE_DURATION("cpu_profiler", __PRETTY_FUNCTION__);
  if (!process.full_process) {
    return;
  }
  zx::result<std::vector<zx_koid_t>> tids = GetChildrenTids(process.handle);
  if (tids.is_error()) {
    return;
  }
  for (zx_koid_t tid : *tids) {
    if (process.threads.contains(tid)) {
      continue;
    }
    zx::thread thread;
    if (process.handle.get_child(tid, ZX_DEFAULT_THREAD_RIGHTS, &thread) != ZX_OK) {
      // The thread may have exited.
      continue;
    }
    AddThread(std::vector<zx_koid_t>{job_path.begin(), job_path.end()}, process.pid, tid,
              std::move(thread));
  }
}
