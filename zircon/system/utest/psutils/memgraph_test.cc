// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/zx/job.h>
#include <lib/zx/process.h>
#include <lib/zx/thread.h>
#include <lib/zx/time.h>
#include <lib/zx/vmar.h>
#include <lib/zx/vmo.h>
#include <stdio.h>
#include <string.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/object.h>

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

  char root_buffer[1024] = {};
  FILE* root_file = fmemopen(root_buffer, sizeof(root_buffer), "w");
  ASSERT_NOT_NULL(root_file);
  JsonTaskEnumerator root_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                     /*show_vmos=*/false, /*show_handle_stats=*/false, root_file);
  EXPECT_OK(root_enumerator.OnJob(/*depth=*/0, job.get(), basic_info.koid, /*parent_koid=*/0));
  fclose(root_file);
  EXPECT_OK(root_enumerator.partial_failure());
  EXPECT_NOT_NULL(strstr(root_buffer, "\"type\": \"j\""));
  EXPECT_NOT_NULL(strstr(root_buffer, "\"parent\": \"kernel/vmo\""));
  EXPECT_NOT_NULL(strstr(root_buffer, "\"name\": \"memgraph-test-job\""));

  char child_buffer[1024] = {};
  FILE* child_file = fmemopen(child_buffer, sizeof(child_buffer), "w");
  ASSERT_NOT_NULL(child_file);
  JsonTaskEnumerator child_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                      /*show_vmos=*/false, /*show_handle_stats=*/false, child_file);
  EXPECT_OK(
      child_enumerator.OnJob(/*depth=*/1, job.get(), basic_info.koid, basic_info.related_koid));
  fclose(child_file);
  EXPECT_OK(child_enumerator.partial_failure());
  EXPECT_NOT_NULL(strstr(child_buffer, "\"parent\": \"j/"));
  EXPECT_NOT_NULL(strstr(child_buffer, "\"name\": \"memgraph-test-job\""));

  char invalid_buffer[1024] = {};
  FILE* invalid_file = fmemopen(invalid_buffer, sizeof(invalid_buffer), "w");
  ASSERT_NOT_NULL(invalid_file);
  JsonTaskEnumerator invalid_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                        /*show_vmos=*/false, /*show_handle_stats=*/false,
                                        invalid_file);
  EXPECT_OK(invalid_enumerator.OnJob(/*depth=*/1, ZX_HANDLE_INVALID, /*koid=*/1234,
                                     /*parent_koid=*/5678));
  fclose(invalid_file);
  EXPECT_OK(invalid_enumerator.partial_failure());
  EXPECT_NOT_NULL(strstr(invalid_buffer, "\"name\": \"<UNKNOWN>\""));
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

  std::string buffer(65536, '\0');
  FILE* file = fmemopen(buffer.data(), buffer.size(), "w");
  ASSERT_NOT_NULL(file);
  JsonTaskEnumerator json_task_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                          /*show_vmos=*/true, /*show_handle_stats=*/true, file);
  EXPECT_OK(json_task_enumerator.OnProcess(/*depth=*/1, zx::process::self()->get(), basic_info.koid,
                                           basic_info.related_koid));
  fclose(file);

  ASSERT_OK(zx::vmar::root_self()->unmap(mapped_address, page_size));

  EXPECT_OK(json_task_enumerator.partial_failure());
  EXPECT_NOT_NULL(strstr(buffer.c_str(), "\"type\": \"p\""));
  EXPECT_NOT_NULL(strstr(buffer.c_str(), "\"private_bytes\": "));
  EXPECT_NOT_NULL(strstr(buffer.c_str(), "\"vmos\": ["));
  EXPECT_NOT_NULL(strstr(buffer.c_str(), "\"name\": \"memgraph-test-vmo\""));
  EXPECT_NOT_NULL(strstr(buffer.c_str(), "\"vmo_refs\": ["));
  EXPECT_NOT_NULL(strstr(buffer.c_str(), "\"HANDLE\""));
  EXPECT_NOT_NULL(strstr(buffer.c_str(), "\"MAPPING\""));
  EXPECT_NOT_NULL(strstr(buffer.c_str(), "\"handle_rights\": [\"READ\", \"WRITE\""));
  EXPECT_NOT_NULL(strstr(buffer.c_str(), "\"handle_stats\": {"));
  EXPECT_NOT_NULL(strstr(buffer.c_str(), "\"vmo\": "));

  // When self_koid matches the target process koid, VMO enumeration is skipped.
  char self_buffer[1024] = {};
  FILE* self_file = fmemopen(self_buffer, sizeof(self_buffer), "w");
  ASSERT_NOT_NULL(self_file);
  JsonTaskEnumerator self_enumerator(/*self_koid=*/basic_info.koid, /*show_threads=*/true,
                                     /*show_vmos=*/true, /*show_handle_stats=*/false, self_file);
  EXPECT_OK(self_enumerator.OnProcess(/*depth=*/1, zx::process::self()->get(), basic_info.koid,
                                      basic_info.related_koid));
  fclose(self_file);
  EXPECT_OK(self_enumerator.partial_failure());
  EXPECT_NULL(strstr(self_buffer, "\"vmos\": ["));
  EXPECT_NULL(strstr(self_buffer, "\"handle_stats\": {"));
}

TEST(PsUtilsMemgraph, OnProcessInvalidHandleRecordsPartialFailure) {
  char buffer[1024] = {};
  FILE* file = fmemopen(buffer, sizeof(buffer), "w");
  ASSERT_NOT_NULL(file);
  JsonTaskEnumerator json_task_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                          /*show_vmos=*/true, /*show_handle_stats=*/true, file);
  EXPECT_OK(json_task_enumerator.OnProcess(/*depth=*/1, ZX_HANDLE_INVALID, /*koid=*/1234,
                                           /*parent_koid=*/5678));
  fclose(file);

  EXPECT_EQ(json_task_enumerator.partial_failure(), ZX_ERR_BAD_HANDLE);
  EXPECT_NOT_NULL(strstr(buffer, "\"name\": \"<UNKNOWN>\""));
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

  // Confirm that the exited process is in ZX_ERR_BAD_STATE for task stats.
  zx_info_task_stats_t task_stats = {};
  ASSERT_EQ(process.get_info(ZX_INFO_TASK_STATS, &task_stats, sizeof(task_stats), nullptr, nullptr),
            ZX_ERR_BAD_STATE);

  char buffer[1024] = {};
  FILE* file = fmemopen(buffer, sizeof(buffer), "w");
  ASSERT_NOT_NULL(file);

  JsonTaskEnumerator json_task_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                          /*show_vmos=*/false,
                                          /*show_handle_stats=*/false, file);
  EXPECT_OK(json_task_enumerator.OnProcess(/*depth=*/1, process.get(), basic_info.koid,
                                           basic_info.related_koid));
  fclose(file);

  EXPECT_OK(json_task_enumerator.partial_failure());
  EXPECT_NOT_NULL(strstr(buffer, "\"private_bytes\": 0, \"shared_bytes\": 0, \"pss_bytes\": 0"));
}

TEST(PsUtilsMemgraph, OnThread) {
  zx_info_handle_basic_t self_info = {};
  ASSERT_OK(zx::thread::self()->get_info(ZX_INFO_HANDLE_BASIC, &self_info, sizeof(self_info),
                                         nullptr, nullptr));

  char running_buffer[1024] = {};
  FILE* running_file = fmemopen(running_buffer, sizeof(running_buffer), "w");
  ASSERT_NOT_NULL(running_file);
  JsonTaskEnumerator running_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                        /*show_vmos=*/false, /*show_handle_stats=*/false,
                                        running_file);
  EXPECT_OK(running_enumerator.OnThread(/*depth=*/2, zx::thread::self()->get(), self_info.koid,
                                        self_info.related_koid));
  fclose(running_file);
  EXPECT_OK(running_enumerator.partial_failure());
  EXPECT_NOT_NULL(strstr(running_buffer, "\"type\": \"t\""));
  EXPECT_NOT_NULL(strstr(running_buffer, "\"state\": \"RUNNING\""));

  static constexpr char kThreadName[] = "memgraph-test-thread";
  zx::thread thread;
  ASSERT_OK(
      zx::thread::create(*zx::process::self(), kThreadName, sizeof(kThreadName) - 1, 0u, &thread));
  zx_info_handle_basic_t thread_info = {};
  ASSERT_OK(
      thread.get_info(ZX_INFO_HANDLE_BASIC, &thread_info, sizeof(thread_info), nullptr, nullptr));

  char new_buffer[1024] = {};
  FILE* new_file = fmemopen(new_buffer, sizeof(new_buffer), "w");
  ASSERT_NOT_NULL(new_file);
  JsonTaskEnumerator new_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                    /*show_vmos=*/false, /*show_handle_stats=*/false, new_file);
  EXPECT_OK(new_enumerator.OnThread(/*depth=*/2, thread.get(), thread_info.koid,
                                    thread_info.related_koid));
  fclose(new_file);
  EXPECT_OK(new_enumerator.partial_failure());
  EXPECT_NOT_NULL(strstr(new_buffer, "\"name\": \"memgraph-test-thread\""));
  EXPECT_NOT_NULL(strstr(new_buffer, "\"state\": \"NEW\""));

  char invalid_buffer[1024] = {};
  FILE* invalid_file = fmemopen(invalid_buffer, sizeof(invalid_buffer), "w");
  ASSERT_NOT_NULL(invalid_file);
  JsonTaskEnumerator invalid_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                        /*show_vmos=*/false, /*show_handle_stats=*/false,
                                        invalid_file);
  EXPECT_OK(invalid_enumerator.OnThread(/*depth=*/2, ZX_HANDLE_INVALID, /*koid=*/1234,
                                        /*parent_koid=*/5678));
  fclose(invalid_file);
  EXPECT_EQ(invalid_enumerator.partial_failure(), ZX_ERR_BAD_HANDLE);
  EXPECT_NOT_NULL(strstr(invalid_buffer, "\"name\": \"<UNKNOWN>\""));
}

TEST(PsUtilsMemgraph, WalkJobTree) {
  std::string with_threads_buffer(16384, '\0');
  FILE* with_threads_file = fmemopen(with_threads_buffer.data(), with_threads_buffer.size(), "w");
  ASSERT_NOT_NULL(with_threads_file);
  JsonTaskEnumerator with_threads_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                             /*show_vmos=*/false, /*show_handle_stats=*/false,
                                             with_threads_file);
  EXPECT_OK(with_threads_enumerator.WalkJobTree(zx::job::default_job()->get()));
  fclose(with_threads_file);
  EXPECT_OK(with_threads_enumerator.partial_failure());
  EXPECT_NOT_NULL(strstr(with_threads_buffer.c_str(), "\"type\": \"j\""));
  EXPECT_NOT_NULL(strstr(with_threads_buffer.c_str(), "\"type\": \"p\""));
  EXPECT_NOT_NULL(strstr(with_threads_buffer.c_str(), "\"type\": \"t\""));

  std::string without_threads_buffer(16384, '\0');
  FILE* without_threads_file =
      fmemopen(without_threads_buffer.data(), without_threads_buffer.size(), "w");
  ASSERT_NOT_NULL(without_threads_file);
  JsonTaskEnumerator without_threads_enumerator(/*self_koid=*/0, /*show_threads=*/false,
                                                /*show_vmos=*/false, /*show_handle_stats=*/false,
                                                without_threads_file);
  EXPECT_OK(without_threads_enumerator.WalkJobTree(zx::job::default_job()->get()));
  fclose(without_threads_file);
  EXPECT_OK(without_threads_enumerator.partial_failure());
  EXPECT_NOT_NULL(strstr(without_threads_buffer.c_str(), "\"type\": \"j\""));
  EXPECT_NOT_NULL(strstr(without_threads_buffer.c_str(), "\"type\": \"p\""));
  EXPECT_NULL(strstr(without_threads_buffer.c_str(), "\"type\": \"t\""));
}

}  // namespace
