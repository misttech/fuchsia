// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/zx/iob.h>
#include <lib/zx/job.h>
#include <lib/zx/process.h>
#include <lib/zx/thread.h>
#include <lib/zx/time.h>
#include <lib/zx/vmar.h>
#include <lib/zx/vmo.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/iob.h>
#include <zircon/syscalls/object.h>

#include <format>
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
  EXPECT_TRUE(out.str().contains("\"committed_scaled_bytes\": "));
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

TEST(PsUtilsMemgraph, OnProcessReportsIobHandle) {
  const size_t page_size = zx_system_get_page_size();
  zx_iob_region_t region = {
      .type = ZX_IOB_REGION_TYPE_PRIVATE,
      .access = ZX_IOB_ACCESS_EP0_CAN_MAP_READ | ZX_IOB_ACCESS_EP0_CAN_MAP_WRITE |
                ZX_IOB_ACCESS_EP1_CAN_MAP_READ,
      .size = page_size,
      .discipline = {.type = ZX_IOB_DISCIPLINE_TYPE_NONE},
      .private_region = {.options = 0},
  };
  zx::iob ep0;
  zx::iob ep1;
  ASSERT_OK(zx::iob::create(0u, &region, 1u, &ep0, &ep1));
  ep0.reset();

  zx_iob_region_info_t region_info = {};
  ASSERT_OK(ep1.get_info(ZX_INFO_IOB_REGIONS, &region_info, sizeof(region_info), nullptr, nullptr));

  zx_info_handle_basic_t basic_info = {};
  ASSERT_OK(zx::process::self()->get_info(ZX_INFO_HANDLE_BASIC, &basic_info, sizeof(basic_info),
                                          nullptr, nullptr));

  std::ostringstream unmapped_out;
  JsonTaskEnumerator unmapped_enumerator(/*self_koid=*/0, /*show_threads=*/false,
                                         /*show_vmos=*/true, /*show_handle_stats=*/false,
                                         unmapped_out);
  EXPECT_OK(unmapped_enumerator.OnProcess(/*depth=*/1, zx::process::self()->get(), basic_info.koid,
                                          basic_info.related_koid));
  EXPECT_TRUE(unmapped_enumerator.partial_failures().empty());
  EXPECT_TRUE(unmapped_out.str().contains(
      std::format("\"vmo_koid\": {}, \"via\": [\"IOB_HANDLE\"], \"handle_rights\": [\"READ\"",
                  region_info.koid)));
  EXPECT_FALSE(unmapped_out.str().contains(
      std::format("\"vmo_koid\": {}, \"via\": [\"MAPPING\"]}}", region_info.koid)));

  zx_vaddr_t mapped_address = 0;
  ASSERT_OK(
      zx::vmar::root_self()->map_iob(ZX_VM_PERM_READ, 0u, ep1, 0u, 0u, page_size, &mapped_address));

  std::ostringstream mapped_out;
  JsonTaskEnumerator mapped_enumerator(/*self_koid=*/0, /*show_threads=*/false,
                                       /*show_vmos=*/true, /*show_handle_stats=*/false, mapped_out);
  EXPECT_OK(mapped_enumerator.OnProcess(/*depth=*/1, zx::process::self()->get(), basic_info.koid,
                                        basic_info.related_koid));
  ASSERT_OK(zx::vmar::root_self()->unmap(mapped_address, page_size));

  EXPECT_TRUE(mapped_enumerator.partial_failures().empty());
  EXPECT_TRUE(mapped_out.str().contains(
      std::format("\"vmo_koid\": {}, \"via\": [\"IOB_HANDLE\"], \"handle_rights\": [\"READ\"",
                  region_info.koid)));
  EXPECT_TRUE(mapped_out.str().contains(
      std::format("\"vmo_koid\": {}, \"via\": [\"MAPPING\"]}}", region_info.koid)));
}

TEST(PsUtilsMemgraph, OnProcessReportsScaledBytesForCowClone) {
  static constexpr char kParentName[] = "memgraph-cow-parent";
  static constexpr char kCloneName[] = "memgraph-cow-clone";
  const size_t page_size = zx_system_get_page_size();

  zx::vmo parent;
  ASSERT_OK(zx::vmo::create(page_size, 0u, &parent));
  ASSERT_OK(parent.set_property(ZX_PROP_NAME, kParentName, sizeof(kParentName) - 1));

  const uint8_t pattern = 0x5a;
  ASSERT_OK(parent.write(&pattern, 0u, sizeof(pattern)));

  zx::vmo clone;
  ASSERT_OK(parent.create_child(ZX_VMO_CHILD_SNAPSHOT, 0u, page_size, &clone));
  ASSERT_OK(clone.set_property(ZX_PROP_NAME, kCloneName, sizeof(kCloneName) - 1));

  zx_info_handle_basic_t basic_info = {};
  ASSERT_OK(zx::process::self()->get_info(ZX_INFO_HANDLE_BASIC, &basic_info, sizeof(basic_info),
                                          nullptr, nullptr));

  // Retry if background kernel activity (such as DebugCompressor) changes the VMO's committed
  // byte counts while OnProcess is snapshotting process VMOs.
  bool verified = false;
  for (int attempt = 0; attempt < 10; ++attempt) {
    // Ensure the shared page is uncompressed and committed before snapshotting.
    uint8_t byte = 0;
    ASSERT_OK(parent.read(&byte, 0u, sizeof(byte)));
    EXPECT_EQ(byte, pattern);

    zx_info_vmo_t before = {};
    ASSERT_OK(clone.get_info(ZX_INFO_VMO, &before, sizeof(before), nullptr, nullptr));

    std::ostringstream out;
    JsonTaskEnumerator enumerator(/*self_koid=*/0, /*show_threads=*/false,
                                  /*show_vmos=*/true, /*show_handle_stats=*/false, out);
    EXPECT_OK(enumerator.OnProcess(/*depth=*/1, zx::process::self()->get(), basic_info.koid,
                                   basic_info.related_koid));
    EXPECT_TRUE(enumerator.partial_failures().empty());

    zx_info_vmo_t after = {};
    ASSERT_OK(clone.get_info(ZX_INFO_VMO, &after, sizeof(after), nullptr, nullptr));
    if (before.committed_change_events != after.committed_change_events ||
        before.committed_bytes != page_size) {
      continue;
    }

    EXPECT_EQ(before.committed_scaled_bytes, page_size / 2);
    EXPECT_TRUE(out.str().contains(
        std::format("\"koid\": {}, \"name\": \"memgraph-cow-clone\", \"size_bytes\": {}, "
                    "\"parent_koid\": {}, \"num_children\": 0, \"num_mappings\": 0, "
                    "\"share_count\": {}, \"committed_bytes\": {}, \"committed_scaled_bytes\": {}",
                    before.koid, before.size_bytes, before.parent_koid, before.share_count,
                    page_size, page_size / 2)));
    verified = true;
    break;
  }
  EXPECT_TRUE(verified);
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

TEST(PsUtilsMemgraph, EscapesSpecialCharactersInTaskAndVmoNames) {
  static constexpr char kRawName[] = "a\"b\\c\bd\fe\nf\rg\th\x01\x1f";
  static constexpr char kExpectedNameField[] =
      "\"name\": \"a\\\"b\\\\c\\bd\\fe\\nf\\rg\\th\\u0001\\u001f\"";
  static_assert(sizeof(kRawName) <= ZX_MAX_NAME_LEN);

  zx::job job;
  ASSERT_OK(zx::job::create(*zx::job::default_job(), 0u, &job));
  ASSERT_OK(job.set_property(ZX_PROP_NAME, kRawName, sizeof(kRawName) - 1));
  zx_info_handle_basic_t job_info = {};
  ASSERT_OK(job.get_info(ZX_INFO_HANDLE_BASIC, &job_info, sizeof(job_info), nullptr, nullptr));

  std::ostringstream job_output;
  JsonTaskEnumerator job_enumerator(/*self_koid=*/0, /*show_threads=*/false,
                                    /*show_vmos=*/false, /*show_handle_stats=*/false, job_output);
  EXPECT_OK(job_enumerator.OnJob(/*depth=*/1, job.get(), job_info.koid, job_info.related_koid));
  EXPECT_TRUE(job_enumerator.partial_failures().empty());
  EXPECT_TRUE(job_output.str().contains(kExpectedNameField));

  zx::process process;
  zx::vmar vmar;
  ASSERT_OK(zx::process::create(job, kRawName, sizeof(kRawName) - 1, 0u, &process, &vmar));
  zx_info_handle_basic_t process_info = {};
  ASSERT_OK(process.get_info(ZX_INFO_HANDLE_BASIC, &process_info, sizeof(process_info), nullptr,
                             nullptr));

  std::ostringstream process_output;
  JsonTaskEnumerator process_enumerator(/*self_koid=*/0, /*show_threads=*/false,
                                        /*show_vmos=*/false, /*show_handle_stats=*/false,
                                        process_output);
  EXPECT_OK(process_enumerator.OnProcess(/*depth=*/1, process.get(), process_info.koid,
                                         process_info.related_koid));
  EXPECT_TRUE(process_enumerator.partial_failures().empty());
  EXPECT_TRUE(process_output.str().contains(kExpectedNameField));

  zx::thread thread;
  ASSERT_OK(zx::thread::create(*zx::process::self(), kRawName, sizeof(kRawName) - 1, 0u, &thread));
  zx_info_handle_basic_t thread_info = {};
  ASSERT_OK(
      thread.get_info(ZX_INFO_HANDLE_BASIC, &thread_info, sizeof(thread_info), nullptr, nullptr));

  std::ostringstream thread_output;
  JsonTaskEnumerator thread_enumerator(/*self_koid=*/0, /*show_threads=*/true,
                                       /*show_vmos=*/false, /*show_handle_stats=*/false,
                                       thread_output);
  EXPECT_OK(thread_enumerator.OnThread(/*depth=*/2, thread.get(), thread_info.koid,
                                       thread_info.related_koid));
  EXPECT_TRUE(thread_enumerator.partial_failures().empty());
  EXPECT_TRUE(thread_output.str().contains(kExpectedNameField));

  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0u, &vmo));
  ASSERT_OK(vmo.set_property(ZX_PROP_NAME, kRawName, sizeof(kRawName) - 1));
  zx_info_vmo_t vmo_info = {};
  ASSERT_OK(vmo.get_info(ZX_INFO_VMO, &vmo_info, sizeof(vmo_info), nullptr, nullptr));

  zx_info_handle_basic_t self_info = {};
  ASSERT_OK(zx::process::self()->get_info(ZX_INFO_HANDLE_BASIC, &self_info, sizeof(self_info),
                                          nullptr, nullptr));

  std::ostringstream vmo_output;
  JsonTaskEnumerator vmo_enumerator(/*self_koid=*/0, /*show_threads=*/false,
                                    /*show_vmos=*/true, /*show_handle_stats=*/false, vmo_output);
  EXPECT_OK(vmo_enumerator.OnProcess(/*depth=*/1, zx::process::self()->get(), self_info.koid,
                                     self_info.related_koid));
  EXPECT_TRUE(vmo_enumerator.partial_failures().empty());
  EXPECT_TRUE(vmo_output.str().contains(
      std::format("\"koid\": {}, {}", vmo_info.koid, kExpectedNameField)));
}

}  // namespace
