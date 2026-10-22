// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/utils/pmr_stack_arena.h"

#include <cstdint>
#include <cstring>
#include <set>
#include <unordered_map>
#include <unordered_set>
#include <vector>

#include <gtest/gtest.h>

#include "src/ui/scenic/lib/utils/pmr_stats_resource.h"

namespace utils {
namespace test {
namespace {

#if !__has_feature(address_sanitizer) && !__has_feature(hwaddress_sanitizer) && \
    !__has_feature(memory_sanitizer)
constexpr size_t kPatternTestArenaSize = 2048;
constexpr uint8_t kSentinelByte = 0x55;

// Runs in the exact same stack frame shape on consecutive calls:
// - When `seed_sentinel` is true, fills the stack arena's buffer with `kSentinelByte`.
// - When `seed_sentinel` is false, counts how many bytes in a freshly constructed `PmrStackArena`
//   still hold `kSentinelByte` (rather than being overwritten with `0xAA` by
//   `-ftrivial-auto-var-init=pattern`).
__attribute__((noinline)) size_t RunArenaStackFrame(bool seed_sentinel) {
  PmrStackArena<kPatternTestArenaSize> arena;
  auto* bytes = static_cast<uint8_t*>(arena.resource()->allocate(kPatternTestArenaSize, 1));
  asm volatile("" : "+r"(bytes) : : "memory");
  if (seed_sentinel) {
    std::memset(bytes, kSentinelByte, kPatternTestArenaSize);
    asm volatile("" : : "r"(bytes) : "memory");
    return 0;
  }
  size_t sentinel_count = 0;
  for (size_t i = 0; i < kPatternTestArenaSize; ++i) {
    if (bytes[i] == kSentinelByte) {
      ++sentinel_count;
    }
  }
  return sentinel_count;
}
#endif

}  // namespace

TEST(PmrStackArenaTest, SizeAndAlignment) {
  PmrStackArena<256> default_upstream_arena;
  EXPECT_EQ(default_upstream_arena.resource()->upstream_resource(),
            std::pmr::get_default_resource());

  // Allocate the full `Size` at each arena's `Alignment` so any misalignment of `buffer_` wastes
  // capacity and spills to `upstream`.
  PmrStatsResource upstream;
  PmrStackArena<256> default_aligned_arena(&upstream);
  EXPECT_GE(alignof(decltype(default_aligned_arena)), alignof(std::max_align_t));

  void* p1 = default_aligned_arena.resource()->allocate(256, alignof(std::max_align_t));
  EXPECT_EQ(reinterpret_cast<uintptr_t>(p1) % alignof(std::max_align_t), 0u);
  EXPECT_EQ(upstream.total_allocation_count(), 0u);

  constexpr size_t kCustomAlignment = 64;
  PmrStackArena<512, kCustomAlignment> custom_aligned_arena(&upstream);
  EXPECT_GE(alignof(decltype(custom_aligned_arena)), kCustomAlignment);

  void* p2 = custom_aligned_arena.resource()->allocate(512, kCustomAlignment);
  EXPECT_EQ(reinterpret_cast<uintptr_t>(p2) % kCustomAlignment, 0u);
  EXPECT_EQ(upstream.total_allocation_count(), 0u);
}

TEST(PmrStackArenaTest, AllocatesFromStackBufferWithoutUpstream) {
  PmrStatsResource upstream;
  {
    PmrStackArena<1024> arena(&upstream);
    EXPECT_EQ(arena.resource()->upstream_resource(), &upstream);

    std::pmr::vector<uint64_t> vec(arena.resource());
    vec.reserve(64);
    for (uint64_t i = 0; i < 64; ++i) {
      vec.push_back(i);
    }

    EXPECT_EQ(upstream.total_allocation_count(), 0u);
    EXPECT_EQ(upstream.total_allocated_bytes(), 0u);
  }
  EXPECT_EQ(upstream.total_allocation_count(), 0u);
  EXPECT_EQ(upstream.total_deallocation_count(), 0u);
}

TEST(PmrStackArenaTest, FallsBackToUpstreamWhenExhausted) {
  PmrStatsResource upstream;
  {
    PmrStackArena<256> arena(&upstream);

    // Fits in the 256-byte stack buffer.
    void* p1 = arena.resource()->allocate(128, 8);
    ASSERT_NE(p1, nullptr);
    EXPECT_EQ(upstream.total_allocation_count(), 0u);

    // Exceeds remaining stack buffer capacity; must allocate from `upstream`.
    void* p2 = arena.resource()->allocate(512, 8);
    ASSERT_NE(p2, nullptr);
    EXPECT_EQ(upstream.total_allocation_count(), 1u);
    EXPECT_GE(upstream.total_allocated_bytes(), 512u);
    EXPECT_EQ(upstream.total_deallocation_count(), 0u);
  }
  // Destruction of `arena` releases upstream chunks.
  EXPECT_EQ(upstream.total_deallocation_count(), 1u);
  EXPECT_EQ(upstream.outstanding_allocations(), 0u);
  EXPECT_EQ(upstream.outstanding_bytes(), 0u);
}

TEST(PmrStackArenaTest, ReleaseReusesStackBuffer) {
  PmrStatsResource upstream;
  PmrStackArena<256> arena(&upstream);

  void* p1 = arena.resource()->allocate(256, 8);
  ASSERT_NE(p1, nullptr);
  EXPECT_EQ(upstream.total_allocation_count(), 0u);

  arena.resource()->release();

  // After `release()`, the internal stack buffer can be allocated from again without hitting
  // `upstream`.
  void* p2 = arena.resource()->allocate(256, 8);
  ASSERT_NE(p2, nullptr);
  EXPECT_EQ(p1, p2);
  EXPECT_EQ(upstream.total_allocation_count(), 0u);
}

TEST(PmrStackArenaTest, PmrContainers) {
  PmrStatsResource upstream;
  PmrStackArena<4096> arena(&upstream);

  std::pmr::vector<int> vec(arena.resource());
  vec.assign({1, 2, 3, 4});

  std::pmr::unordered_set<int> uset(vec.begin(), vec.end(), /*bucket_count=*/vec.size(),
                                    arena.resource());
  EXPECT_EQ(uset.size(), 4u);

  std::pmr::unordered_map<int, int> umap(arena.resource());
  umap.emplace(1, 10);
  EXPECT_EQ(umap.at(1), 10);

  std::pmr::set<int> oset(arena.resource());
  oset.insert(42);
  EXPECT_TRUE(oset.contains(42));

  EXPECT_EQ(upstream.total_allocation_count(), 0u);
}

#if !__has_feature(address_sanitizer) && !__has_feature(hwaddress_sanitizer) && \
    !__has_feature(memory_sanitizer)
TEST(PmrStackArenaTest, SkipsAutoVarPatternInit) {
  RunArenaStackFrame(/*seed_sentinel=*/true);
  const size_t preserved_sentinel_bytes = RunArenaStackFrame(/*seed_sentinel=*/false);
  EXPECT_EQ(preserved_sentinel_bytes, kPatternTestArenaSize);
}
#endif

}  // namespace test
}  // namespace utils
