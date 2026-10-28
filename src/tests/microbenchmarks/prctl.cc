// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/syslog/cpp/macros.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <unistd.h>

#include <thread>

#include <perftest/perftest.h>

#ifndef PR_SET_VMA
#define PR_SET_VMA 0x53564d41
#endif
#ifndef PR_SET_VMA_ANON_NAME
#define PR_SET_VMA_ANON_NAME 0
#endif

namespace {

bool SupportsSetVmaAnonName() {
  const size_t page_size = static_cast<size_t>(sysconf(_SC_PAGESIZE));
  void* addr = mmap(nullptr, page_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (addr == MAP_FAILED) {
    return false;
  }
  bool supported = prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME, addr, page_size, "probe") == 0;
  FX_CHECK(munmap(addr, page_size) == 0);
  return supported;
}

// Thread-group leader rename: also renames the process and reads argv[0].
bool PrctlSetNameLeader(perftest::RepeatState* state) {
  char orig_name[16] = {};
  FX_CHECK(prctl(PR_GET_NAME, orig_name) == 0);

  constexpr const char* kNames[2] = {"bench_leader_0", "bench_leader_1"};
  size_t i = 0;
  while (state->KeepRunning()) {
    FX_CHECK(prctl(PR_SET_NAME, kNames[i & 1]) == 0);
    ++i;
  }

  FX_CHECK(prctl(PR_SET_NAME, orig_name) == 0);
  return true;
}

// Renaming to the current name.
bool PrctlSetNameUnchanged(perftest::RepeatState* state) {
  char orig_name[16] = {};
  FX_CHECK(prctl(PR_GET_NAME, orig_name) == 0);
  FX_CHECK(prctl(PR_SET_NAME, "bench_unchanged") == 0);

  while (state->KeepRunning()) {
    FX_CHECK(prctl(PR_SET_NAME, "bench_unchanged") == 0);
  }

  FX_CHECK(prctl(PR_SET_NAME, orig_name) == 0);
  return true;
}

// Non-leader rename: only the thread is renamed.
bool PrctlSetNameWorker(perftest::RepeatState* state) {
  std::thread worker([state]() {
    constexpr const char* kNames[2] = {"bench_worker_0", "bench_worker_1"};
    size_t i = 0;
    while (state->KeepRunning()) {
      FX_CHECK(prctl(PR_SET_NAME, kNames[i & 1]) == 0);
      ++i;
    }
  });
  worker.join();
  return true;
}

bool PrctlSetVmaAnonName(perftest::RepeatState* state) {
  const size_t page_size = static_cast<size_t>(sysconf(_SC_PAGESIZE));
  void* addr = mmap(nullptr, page_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  FX_CHECK(addr != MAP_FAILED);

  constexpr const char* kNames[2] = {"bench_vma_0", "bench_vma_1"};
  size_t i = 0;
  while (state->KeepRunning()) {
    FX_CHECK(prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME, addr, page_size, kNames[i & 1]) == 0);
    ++i;
  }

  FX_CHECK(munmap(addr, page_size) == 0);
  return true;
}

bool PrctlSetVmaAnonNameUnchanged(perftest::RepeatState* state) {
  const size_t page_size = static_cast<size_t>(sysconf(_SC_PAGESIZE));
  void* addr = mmap(nullptr, page_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  FX_CHECK(addr != MAP_FAILED);
  FX_CHECK(prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME, addr, page_size, "bench_vma_unchanged") == 0);

  while (state->KeepRunning()) {
    FX_CHECK(prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME, addr, page_size, "bench_vma_unchanged") == 0);
  }

  FX_CHECK(munmap(addr, page_size) == 0);
  return true;
}

void RegisterTests() {
  perftest::RegisterTest("Prctl/SetName/Leader", PrctlSetNameLeader);
  perftest::RegisterTest("Prctl/SetName/Unchanged", PrctlSetNameUnchanged);
  perftest::RegisterTest("Prctl/SetName/Worker", PrctlSetNameWorker);
  // PR_SET_VMA_ANON_NAME needs CONFIG_ANON_VMA_NAME on Linux hosts.
  if (SupportsSetVmaAnonName()) {
    perftest::RegisterTest("Prctl/SetVmaAnonName", PrctlSetVmaAnonName);
    perftest::RegisterTest("Prctl/SetVmaAnonName/Unchanged", PrctlSetVmaAnonNameUnchanged);
  }
}
PERFTEST_CTOR(RegisterTests)

}  // namespace
