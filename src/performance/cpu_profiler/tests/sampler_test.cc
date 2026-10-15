// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/performance/cpu_profiler/sampler.h"

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/zx/job.h>
#include <lib/zx/process.h>
#include <lib/zx/thread.h>
#include <zircon/types.h>

#include <barrier>
#include <span>
#include <thread>
#include <unordered_set>
#include <vector>

#include <gtest/gtest.h>

#include "src/lib/fxl/memory/ref_ptr.h"
#include "src/performance/cpu_profiler/targets.h"

namespace {

zx_koid_t GetKoid(const zx::object_base& object) {
  zx_info_handle_basic_t info;
  EXPECT_EQ(ZX_OK, object.get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr));
  return info.koid;
}

// Exposes the protected parts of Sampler that we want to test.
class TestSampler : public profiler::Sampler {
 public:
  using Sampler::AddProcessesStartedSinceConfigure;
  using Sampler::AddThread;
  using Sampler::AddThreadsStartedSinceConfigure;
  using Sampler::CacheModules;
  using Sampler::Sampler;
  using Sampler::ScheduleCacheModules;
  profiler::TargetTree& targets() { return targets_; }
  const auto& contexts() const { return contexts_; }
  const auto& process_watchers() const { return process_watchers_; }
};

// Jobs and processes started in a target job after the target tree was populated must be added to
// it.
TEST(SamplerTest, AddsProcessesStartedSinceConfigure) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  // Tests aren't allowed to create processes, so model a target tree that was populated before this
  // process started: a target for our own job that doesn't know about any of its processes or
  // subjobs.
  zx::job job;
  ASSERT_EQ(ZX_OK, zx::job::default_job()->duplicate(ZX_RIGHT_SAME_RIGHTS, &job));
  const zx_koid_t job_koid = GetKoid(job);
  profiler::TargetTree tree;
  ASSERT_TRUE(
      tree.AddJob(profiler::JobTarget{std::move(job), job_koid, std::span<const zx_koid_t>{}})
          .is_ok());
  auto sampler = fxl::MakeRefCounted<TestSampler>(
      loop.dispatcher(), std::move(tree), std::vector<fuchsia_cpu_profiler::SamplingConfig>{});

  zx::job child_job, grandchild_job;
  ASSERT_EQ(ZX_OK, zx::job::create(*zx::job::default_job(), 0, &child_job));
  ASSERT_EQ(ZX_OK, zx::job::create(child_job, 0, &grandchild_job));
  const zx_koid_t child_job_koid = GetKoid(child_job);
  const zx_koid_t grandchild_job_koid = GetKoid(grandchild_job);

  const zx_koid_t pid = GetKoid(*zx::process::self());
  const std::vector<zx_koid_t> job_path{job_koid};
  EXPECT_TRUE(sampler->targets().GetProcess(job_path, pid).is_error());

  sampler->AddProcessesStartedSinceConfigure();
  zx::result<profiler::ProcessTarget*> target = sampler->targets().GetProcess(job_path, pid);
  ASSERT_TRUE(target.is_ok());
  // The process's threads come along with it.
  EXPECT_TRUE((*target)->threads.contains(GetKoid(*zx::thread::self())));

  // Nested subjobs created since configuration are also added with their ancestry.
  bool found_child = false;
  bool found_grandchild = false;
  ASSERT_TRUE(sampler->targets()
                  .ForEachJob([&](const profiler::JobTarget& j) {
                    if (j.job_id == child_job_koid) {
                      EXPECT_EQ(j.ancestry, (std::vector<zx_koid_t>{job_koid}));
                      found_child = true;
                    } else if (j.job_id == grandchild_job_koid) {
                      EXPECT_EQ(j.ancestry, (std::vector<zx_koid_t>{job_koid, child_job_koid}));
                      found_grandchild = true;
                    }
                    return zx::ok();
                  })
                  .is_ok());
  EXPECT_TRUE(found_child);
  EXPECT_TRUE(found_grandchild);

  // Doing it again is harmless.
  sampler->AddProcessesStartedSinceConfigure();
  EXPECT_TRUE(sampler->targets().GetProcess(job_path, pid).is_ok());
}

// Threads started in a target process after its target was created must be added to it.
TEST(SamplerTest, AddsThreadsStartedSinceConfigure) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  zx::process self;
  ASSERT_EQ(ZX_OK, zx::process::self()->duplicate(ZX_RIGHT_SAME_RIGHTS, &self));
  const zx_koid_t pid = GetKoid(self);

  zx::result<profiler::ProcessTarget> process_target =
      profiler::MakeProcessTargetWithoutModules(std::move(self));
  ASSERT_TRUE(process_target.is_ok());
  profiler::TargetTree tree;
  ASSERT_TRUE(tree.AddProcess(std::move(*process_target)).is_ok());
  auto sampler = fxl::MakeRefCounted<TestSampler>(
      loop.dispatcher(), std::move(tree), std::vector<fuchsia_cpu_profiler::SamplingConfig>{});

  zx_koid_t tid = ZX_KOID_INVALID;
  std::barrier sync_point(2);
  std::thread late_thread([&tid, &sync_point] {
    tid = GetKoid(*zx::thread::self());
    sync_point.arrive_and_wait();
    sync_point.arrive_and_wait();
  });
  sync_point.arrive_and_wait();

  const std::vector<zx_koid_t> job_path;
  zx::result<profiler::ProcessTarget*> target = sampler->targets().GetProcess(job_path, pid);
  ASSERT_TRUE(target.is_ok());
  EXPECT_FALSE((*target)->threads.contains(tid));

  sampler->AddThreadsStartedSinceConfigure(job_path, **target);
  EXPECT_TRUE((*target)->threads.contains(tid));

  sync_point.arrive_and_wait();
  late_thread.join();
}

// Sampler::Start must watch jobs, pick up subjobs, processes, and threads started since
// configuration, and schedule module caching.
TEST(SamplerTest, StartPicksUpTasksStartedSinceConfigure) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  zx::job job;
  ASSERT_EQ(ZX_OK, zx::job::default_job()->duplicate(ZX_RIGHT_SAME_RIGHTS, &job));
  const zx_koid_t job_koid = GetKoid(job);
  profiler::TargetTree tree;
  ASSERT_TRUE(
      tree.AddJob(profiler::JobTarget{std::move(job), job_koid, std::span<const zx_koid_t>{}})
          .is_ok());
  auto sampler = fxl::MakeRefCounted<TestSampler>(
      loop.dispatcher(), std::move(tree), std::vector<fuchsia_cpu_profiler::SamplingConfig>{});

  zx::job child_job;
  ASSERT_EQ(ZX_OK, zx::job::create(*zx::job::default_job(), 0, &child_job));
  const zx_koid_t child_job_koid = GetKoid(child_job);

  zx_koid_t tid = ZX_KOID_INVALID;
  std::barrier sync_point(2);
  std::thread late_thread([&tid, &sync_point] {
    tid = GetKoid(*zx::thread::self());
    sync_point.arrive_and_wait();
    sync_point.arrive_and_wait();
  });
  sync_point.arrive_and_wait();

  ASSERT_TRUE(sampler->Start(0).is_ok());
  loop.RunUntilIdle();
  ASSERT_TRUE(sampler->Stop().is_ok());

  const zx_koid_t pid = GetKoid(*zx::process::self());
  const std::vector<zx_koid_t> job_path{job_koid};
  zx::result<profiler::ProcessTarget*> target = sampler->targets().GetProcess(job_path, pid);
  ASSERT_TRUE(target.is_ok());
  EXPECT_TRUE((*target)->threads.contains(tid));
  EXPECT_TRUE(sampler->contexts().contains(pid));
  EXPECT_FALSE(sampler->contexts().at(pid).empty());

  bool found_child = false;
  ASSERT_TRUE(sampler->targets()
                  .ForEachJob([&](const profiler::JobTarget& j) {
                    if (j.job_id == child_job_koid) {
                      found_child = true;
                    }
                    return zx::ok();
                  })
                  .is_ok());
  EXPECT_TRUE(found_child);

  sync_point.arrive_and_wait();
  late_thread.join();
}

// CacheModules must reuse modules collected at configuration time and must not insert an empty
// module map into `contexts_` if a process's modules cannot be read yet, so a later thread start
// can retry.
TEST(SamplerTest, CacheModulesReusesPreloadedAndRetriesWhenEmpty) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);

  // 1. A ProcessTarget with preloaded `modules` populates `contexts_` without reading the process.
  constexpr zx_koid_t kPreloadedPid = 12345;
  profiler::ProcessTarget preloaded{zx::process{ZX_HANDLE_INVALID}, kPreloadedPid, "preloaded", {}};
  preloaded.modules[{std::byte{0xAB}}] =
      profiler::Module{.module_name = "preloaded.so", .vaddr = 0};

  // 2. A process handle with ZX_RIGHT_INSPECT but without ZX_RIGHT_READ makes GetProcessModules
  // return zx::ok with an empty map (modeling a newly created process before ELF modules are
  // mapped).
  zx::process unreadable_self;
  ASSERT_EQ(ZX_OK, zx::process::self()->duplicate(ZX_RIGHT_INSPECT | ZX_RIGHT_GET_PROPERTY,
                                                  &unreadable_self));
  const zx_koid_t self_pid = GetKoid(*zx::process::self());
  profiler::ProcessTarget empty_modules_target{std::move(unreadable_self), self_pid, "self", {}};

  profiler::TargetTree tree;
  ASSERT_TRUE(tree.AddProcess(std::move(preloaded)).is_ok());
  ASSERT_TRUE(tree.AddProcess(std::move(empty_modules_target)).is_ok());

  auto sampler = fxl::MakeRefCounted<TestSampler>(
      loop.dispatcher(), std::move(tree), std::vector<fuchsia_cpu_profiler::SamplingConfig>{});

  const std::vector<zx_koid_t> job_path;
  zx::result<profiler::ProcessTarget*> preloaded_ptr =
      sampler->targets().GetProcess(job_path, kPreloadedPid);
  ASSERT_TRUE(preloaded_ptr.is_ok());
  sampler->CacheModules(**preloaded_ptr);
  ASSERT_TRUE(sampler->contexts().contains(kPreloadedPid));
  EXPECT_EQ(sampler->contexts().at(kPreloadedPid).size(), 1u);

  // ScheduleCacheModules on the unreadable handle returns an empty map and must not insert into
  // `contexts_`.
  sampler->ScheduleCacheModules(job_path, self_pid);
  loop.RunUntilIdle();
  EXPECT_FALSE(sampler->contexts().contains(self_pid));

  // Once the process's memory is readable, a subsequent thread start schedules module caching again
  // and populates `contexts_`.
  zx::result<profiler::ProcessTarget*> self_ptr = sampler->targets().GetProcess(job_path, self_pid);
  ASSERT_TRUE(self_ptr.is_ok());
  ASSERT_EQ(ZX_OK, zx::process::self()->duplicate(ZX_RIGHT_SAME_RIGHTS, &(*self_ptr)->handle));

  zx::thread self_thread;
  ASSERT_EQ(ZX_OK, zx::thread::self()->duplicate(ZX_RIGHT_SAME_RIGHTS, &self_thread));
  sampler->AddThread(job_path, self_pid, GetKoid(self_thread), std::move(self_thread));
  loop.RunUntilIdle();
  ASSERT_TRUE(sampler->contexts().contains(self_pid));
  EXPECT_FALSE(sampler->contexts().at(self_pid).empty());
}

// GetChildrenTids must grow its buffer when a process has more than 64 threads.
TEST(SamplerTest, GetChildrenTidsOverInitialBufferSize) {
  constexpr size_t kNumExtraThreads = 70;
  std::barrier sync_point(kNumExtraThreads + 1);
  std::vector<std::thread> threads;
  std::vector<zx_koid_t> expected_tids(kNumExtraThreads);
  threads.reserve(kNumExtraThreads);
  for (size_t i = 0; i < kNumExtraThreads; ++i) {
    threads.emplace_back([&, i] {
      expected_tids[i] = GetKoid(*zx::thread::self());
      sync_point.arrive_and_wait();
      sync_point.arrive_and_wait();
    });
  }
  sync_point.arrive_and_wait();

  zx::result<std::vector<zx_koid_t>> tids = profiler::GetChildrenTids(*zx::process::self());
  sync_point.arrive_and_wait();
  for (std::thread& t : threads) {
    t.join();
  }

  ASSERT_TRUE(tids.is_ok());
  EXPECT_GT(tids->size(), kNumExtraThreads);
  std::unordered_set<zx_koid_t> tid_set(tids->begin(), tids->end());
  for (zx_koid_t expected : expected_tids) {
    EXPECT_TRUE(tid_set.contains(expected));
  }
}

// When a ProcessTarget only holds explicitly targeted threads (`full_process == false`),
// Sampler::Start must not watch the process or add its other threads.
TEST(SamplerTest, StartDoesNotAddOtherThreadsForThreadOnlyTarget) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  zx::process self;
  ASSERT_EQ(ZX_OK, zx::process::self()->duplicate(ZX_RIGHT_SAME_RIGHTS, &self));
  const zx_koid_t pid = GetKoid(self);

  zx::result<profiler::ProcessTarget> process_target =
      profiler::MakeProcessTargetWithoutModules(std::move(self), /*full_process=*/false);
  ASSERT_TRUE(process_target.is_ok());
  EXPECT_TRUE(process_target->threads.empty());

  profiler::TargetTree tree;
  ASSERT_TRUE(tree.AddProcess(std::move(*process_target)).is_ok());
  auto sampler = fxl::MakeRefCounted<TestSampler>(
      loop.dispatcher(), std::move(tree), std::vector<fuchsia_cpu_profiler::SamplingConfig>{});

  ASSERT_TRUE(sampler->Start(0).is_ok());
  ASSERT_TRUE(sampler->Stop().is_ok());

  EXPECT_TRUE(sampler->process_watchers().empty());
  zx::result<profiler::ProcessTarget*> target =
      sampler->targets().GetProcess(std::span<const zx_koid_t>{}, pid);
  ASSERT_TRUE(target.is_ok());
  EXPECT_TRUE((*target)->threads.empty());
}

}  // namespace
