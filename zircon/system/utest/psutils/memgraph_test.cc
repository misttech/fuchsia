// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/zx/job.h>
#include <lib/zx/process.h>
#include <lib/zx/thread.h>
#include <lib/zx/time.h>
#include <lib/zx/vmar.h>
#include <lib/zx/vmo.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/object.h>

#include <sstream>
#include <string>

#include <zxtest/zxtest.h>

#include "src/sys/bin/psutils/memgraph_internal.h"

namespace {

TEST(PsUtilsMemgraph, OnJob) {
  static constexpr char kJobName[] = "memgraph-test-job";
  zx::job job;
  ASSERT_OK(zx::job::create(*zx::job::default_job(), 0u, &job));
  ASSERT_OK(job.set_property(ZX_PROP_NAME, kJobName, sizeof(kJobName) - 1));

  zx_info_handle_basic_t basic_info = {};
  ASSERT_OK(job.get_info(ZX_INFO_HANDLE_BASIC, &basic_info, sizeof(basic_info), nullptr, nullptr));

  std::ostringstream root_output;
  JsonTaskEnumerator root_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                     /*show_vmos=*/false, /*show_handle_stats=*/false, root_output);
  EXPECT_OK(root_enumerator.OnJob(/*depth=*/0, job.get(), basic_info.koid, /*parent_koid=*/0));
  EXPECT_TRUE(root_enumerator.partial_failures().empty());
  EXPECT_TRUE(root_output.str().contains("\"type\": \"j\""));
  EXPECT_TRUE(root_output.str().contains("\"parent\": \"kernel/vmo\""));
  EXPECT_TRUE(root_output.str().contains("\"name\": \"memgraph-test-job\""));

  std::ostringstream child_output;
  JsonTaskEnumerator child_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                      /*show_vmos=*/false, /*show_handle_stats=*/false,
                                      child_output);
  EXPECT_OK(
      child_enumerator.OnJob(/*depth=*/1, job.get(), basic_info.koid, basic_info.related_koid));
  EXPECT_TRUE(child_enumerator.partial_failures().empty());
  EXPECT_TRUE(child_output.str().contains("\"parent\": \"j/"));
  EXPECT_TRUE(child_output.str().contains("\"name\": \"memgraph-test-job\""));

  std::ostringstream invalid_output;
  JsonTaskEnumerator invalid_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                        /*show_vmos=*/false, /*show_handle_stats=*/false,
                                        invalid_output);
  EXPECT_OK(invalid_enumerator.OnJob(/*depth=*/1, ZX_HANDLE_INVALID, /*koid=*/1234,
                                     /*parent_koid=*/5678));
  EXPECT_TRUE(invalid_enumerator.partial_failures().empty());
  EXPECT_TRUE(invalid_output.str().contains("\"name\": \"<UNKNOWN>\""));
}

TEST(PsUtilsMemgraph, OnProcessLiveWithVmosAndHandleStats) {
  static constexpr char kVmoName[] = "memgraph-test-vmo";
  const size_t page_size = zx_system_get_page_size();
  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(page_size, 0u, &vmo));
  ASSERT_OK(vmo.set_property(ZX_PROP_NAME, kVmoName, sizeof(kVmoName) - 1));

  zx_vaddr_t mapped_address = 0;
  ASSERT_OK(zx::vmar::root_self()->map(ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, 0u, vmo, 0u, page_size,
                                       &mapped_address));

  zx_info_handle_basic_t basic_info = {};
  ASSERT_OK(zx::process::self()->get_info(ZX_INFO_HANDLE_BASIC, &basic_info, sizeof(basic_info),
                                          nullptr, nullptr));

  std::ostringstream out;
  JsonTaskEnumerator json_task_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                          /*show_vmos=*/true, /*show_handle_stats=*/true, out);
  EXPECT_OK(json_task_enumerator.OnProcess(/*depth=*/1, zx::process::self()->get(), basic_info.koid,
                                           basic_info.related_koid));

  ASSERT_OK(zx::vmar::root_self()->unmap(mapped_address, page_size));

  EXPECT_TRUE(json_task_enumerator.partial_failures().empty());
  EXPECT_TRUE(out.str().contains("\"type\": \"p\""));
  EXPECT_TRUE(out.str().contains("\"private_bytes\": "));
  EXPECT_TRUE(out.str().contains("\"vmos\": ["));
  EXPECT_TRUE(out.str().contains("\"name\": \"memgraph-test-vmo\""));
  EXPECT_TRUE(out.str().contains("\"vmo_refs\": ["));
  EXPECT_TRUE(out.str().contains("\"HANDLE\""));
  EXPECT_TRUE(out.str().contains("\"MAPPING\""));
  EXPECT_TRUE(out.str().contains("\"handle_rights\": [\"READ\", \"WRITE\""));
  EXPECT_TRUE(out.str().contains("\"handle_stats\": {"));
  EXPECT_TRUE(out.str().contains("\"vmo\": "));

  // When self_koid matches the target process koid, VMO enumeration is skipped.
  std::ostringstream self_output;
  JsonTaskEnumerator self_enumerator(/*self_koid=*/basic_info.koid, /*show_threads=*/true,
                                     /*show_vmos=*/true, /*show_handle_stats=*/false, self_output);
  EXPECT_OK(self_enumerator.OnProcess(/*depth=*/1, zx::process::self()->get(), basic_info.koid,
                                      basic_info.related_koid));
  EXPECT_TRUE(self_enumerator.partial_failures().empty());
  EXPECT_FALSE(self_output.str().contains("\"vmos\": ["));
  EXPECT_FALSE(self_output.str().contains("\"handle_stats\": {"));
}

TEST(PsUtilsMemgraph, OnProcessInvalidHandleRecordsPartialFailure) {
  std::ostringstream out;
  JsonTaskEnumerator json_task_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                          /*show_vmos=*/true, /*show_handle_stats=*/true, out);
  EXPECT_OK(json_task_enumerator.OnProcess(/*depth=*/1, ZX_HANDLE_INVALID, /*koid=*/1234,
                                           /*parent_koid=*/5678));

  ASSERT_EQ(json_task_enumerator.partial_failures().size(), 3u);
  EXPECT_EQ(json_task_enumerator.partial_failures()[0], ZX_ERR_BAD_HANDLE);
  EXPECT_EQ(json_task_enumerator.partial_failures()[1], ZX_ERR_BAD_HANDLE);
  EXPECT_EQ(json_task_enumerator.partial_failures()[2], ZX_ERR_BAD_HANDLE);
  EXPECT_TRUE(out.str().contains("\"name\": \"<UNKNOWN>\""));
}

TEST(PsUtilsMemgraph, ExitedProcessDoesNotCausePartialFailure) {
  static constexpr char kProcessName[] = "memgraph-exited-process";
  zx::process process;
  zx::vmar vmar;
  ASSERT_OK(zx::process::create(*zx::job::default_job(), kProcessName, sizeof(kProcessName) - 1, 0u,
                                &process, &vmar));

  zx_info_handle_basic_t basic_info = {};
  ASSERT_OK(
      process.get_info(ZX_INFO_HANDLE_BASIC, &basic_info, sizeof(basic_info), nullptr, nullptr));

  ASSERT_OK(process.kill());
  ASSERT_OK(process.wait_one(ZX_PROCESS_TERMINATED, zx::time::infinite(), nullptr));

  // Confirm that the exited process is in ZX_ERR_BAD_STATE for task stats and VMOs.
  zx_info_task_stats_t task_stats = {};
  ASSERT_EQ(process.get_info(ZX_INFO_TASK_STATS, &task_stats, sizeof(task_stats), nullptr, nullptr),
            ZX_ERR_BAD_STATE);
  zx_info_vmo_t vmo_info = {};
  size_t actual = 0;
  size_t available = 0;
  ASSERT_EQ(
      process.get_info(ZX_INFO_PROCESS_VMOS, &vmo_info, sizeof(vmo_info), &actual, &available),
      ZX_ERR_BAD_STATE);

  std::ostringstream out;
  JsonTaskEnumerator json_task_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                          /*show_vmos=*/true, /*show_handle_stats=*/true, out);
  EXPECT_OK(json_task_enumerator.OnProcess(/*depth=*/1, process.get(), basic_info.koid,
                                           basic_info.related_koid));

  EXPECT_TRUE(json_task_enumerator.partial_failures().empty());
  EXPECT_TRUE(out.str().contains("\"private_bytes\": 0, \"shared_bytes\": 0, \"pss_bytes\": 0"));
}

TEST(PsUtilsMemgraph, OnThread) {
  zx_info_handle_basic_t self_info = {};
  ASSERT_OK(zx::thread::self()->get_info(ZX_INFO_HANDLE_BASIC, &self_info, sizeof(self_info),
                                         nullptr, nullptr));

  std::ostringstream running_output;
  JsonTaskEnumerator running_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                        /*show_vmos=*/false, /*show_handle_stats=*/false,
                                        running_output);
  EXPECT_OK(running_enumerator.OnThread(/*depth=*/2, zx::thread::self()->get(), self_info.koid,
                                        self_info.related_koid));
  EXPECT_TRUE(running_enumerator.partial_failures().empty());
  EXPECT_TRUE(running_output.str().contains("\"type\": \"t\""));
  EXPECT_TRUE(running_output.str().contains("\"state\": \"RUNNING\""));

  static constexpr char kThreadName[] = "memgraph-test-thread";
  zx::thread thread;
  ASSERT_OK(
      zx::thread::create(*zx::process::self(), kThreadName, sizeof(kThreadName) - 1, 0u, &thread));
  zx_info_handle_basic_t thread_info = {};
  ASSERT_OK(
      thread.get_info(ZX_INFO_HANDLE_BASIC, &thread_info, sizeof(thread_info), nullptr, nullptr));

  std::ostringstream new_output;
  JsonTaskEnumerator new_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                    /*show_vmos=*/false, /*show_handle_stats=*/false, new_output);
  EXPECT_OK(new_enumerator.OnThread(/*depth=*/2, thread.get(), thread_info.koid,
                                    thread_info.related_koid));
  EXPECT_TRUE(new_enumerator.partial_failures().empty());
  EXPECT_TRUE(new_output.str().contains("\"name\": \"memgraph-test-thread\""));
  EXPECT_TRUE(new_output.str().contains("\"state\": \"NEW\""));

  std::ostringstream invalid_output;
  JsonTaskEnumerator invalid_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                        /*show_vmos=*/false, /*show_handle_stats=*/false,
                                        invalid_output);
  EXPECT_OK(invalid_enumerator.OnThread(/*depth=*/2, ZX_HANDLE_INVALID, /*koid=*/1234,
                                        /*parent_koid=*/5678));
  ASSERT_EQ(invalid_enumerator.partial_failures().size(), 1u);
  EXPECT_EQ(invalid_enumerator.partial_failures()[0], ZX_ERR_BAD_HANDLE);
  EXPECT_TRUE(invalid_output.str().contains("\"name\": \"<UNKNOWN>\""));
}

TEST(PsUtilsMemgraph, WalkJobTree) {
  std::ostringstream with_threads_output;
  JsonTaskEnumerator with_threads_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                             /*show_vmos=*/false, /*show_handle_stats=*/false,
                                             with_threads_output);
  EXPECT_OK(with_threads_enumerator.WalkJobTree(zx::job::default_job()->get()));
  EXPECT_TRUE(with_threads_enumerator.partial_failures().empty());
  EXPECT_TRUE(with_threads_output.str().contains("\"type\": \"j\""));
  EXPECT_TRUE(with_threads_output.str().contains("\"type\": \"p\""));
  EXPECT_TRUE(with_threads_output.str().contains("\"type\": \"t\""));

  std::ostringstream without_threads_output;
  JsonTaskEnumerator without_threads_enumerator(/*self_koid=*/0, /*show_threads=*/false,
                                                /*show_vmos=*/false, /*show_handle_stats=*/false,
                                                without_threads_output);
  EXPECT_OK(without_threads_enumerator.WalkJobTree(zx::job::default_job()->get()));
  EXPECT_TRUE(without_threads_enumerator.partial_failures().empty());
  EXPECT_TRUE(without_threads_output.str().contains("\"type\": \"j\""));
  EXPECT_TRUE(without_threads_output.str().contains("\"type\": \"p\""));
  EXPECT_FALSE(without_threads_output.str().contains("\"type\": \"t\""));
}

}  // namespace
