// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/syslog/cpp/macros.h>
#include <sys/mman.h>
#include <unistd.h>

#include <string>

#include <perftest/perftest.h>

namespace {

// Creates a contiguous region of `num_mappings` distinct page-sized VMAs by
// alternating PROT_READ | PROT_WRITE and PROT_READ permissions on odd pages so
// adjacent mappings are not merged by RangeMap.
uint8_t* CreateSplitMappings(int num_mappings, int map_flags, size_t page_size) {
  const size_t total_size = static_cast<size_t>(num_mappings) * page_size;
  void* addr = mmap(nullptr, total_size, PROT_READ | PROT_WRITE, map_flags | MAP_ANONYMOUS, -1, 0);
  FX_CHECK(addr != MAP_FAILED);
  uint8_t* base = static_cast<uint8_t*>(addr);
  for (int i = 1; i < num_mappings; i += 2) {
    FX_CHECK(mprotect(base + static_cast<size_t>(i) * page_size, page_size, PROT_READ) == 0);
  }
  return base;
}

// Measure the time taken by madvise(MADV_DONTNEED) across `num_mappings` VMAs.
bool MadviseDontNeedTest(perftest::RepeatState* state, int num_mappings) {
  const size_t page_size = static_cast<size_t>(sysconf(_SC_PAGESIZE));
  const size_t total_size = static_cast<size_t>(num_mappings) * page_size;
  uint8_t* base = CreateSplitMappings(num_mappings, MAP_SHARED, page_size);

  while (state->KeepRunning()) {
    FX_CHECK(madvise(base, total_size, MADV_DONTNEED) == 0);
  }

  FX_CHECK(munmap(base, total_size) == 0);
  return true;
}

// Measure the time taken by msync(MS_ASYNC) across `num_mappings` VMAs.
bool MsyncAsyncTest(perftest::RepeatState* state, int num_mappings) {
  const size_t page_size = static_cast<size_t>(sysconf(_SC_PAGESIZE));
  const size_t total_size = static_cast<size_t>(num_mappings) * page_size;
  uint8_t* base = CreateSplitMappings(num_mappings, MAP_PRIVATE, page_size);

  while (state->KeepRunning()) {
    FX_CHECK(msync(base, total_size, MS_ASYNC) == 0);
  }

  FX_CHECK(munmap(base, total_size) == 0);
  return true;
}

void RegisterTests() {
  for (int num_mappings : {64, 512, 4096}) {
    std::string madvise_name = "MadviseDontNeed/" + std::to_string(num_mappings) + "mappings";
    perftest::RegisterTest(madvise_name.c_str(), MadviseDontNeedTest, num_mappings);

    std::string msync_name = "MsyncAsync/" + std::to_string(num_mappings) + "mappings";
    perftest::RegisterTest(msync_name.c_str(), MsyncAsyncTest, num_mappings);
  }
}
PERFTEST_CTOR(RegisterTests)

}  // namespace
