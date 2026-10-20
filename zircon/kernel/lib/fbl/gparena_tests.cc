// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/page/size.h>
#include <lib/unittest/unittest.h>
#include <platform.h>

#include <arch/regs.h>
#include <fbl/alloc_checker.h>
#include <fbl/gparena.h>
#include <ktl/atomic.h>

#include <ktl/enforce.h>

using fbl::GPArena;

static bool can_declare_small_objectsize() {
  BEGIN_TEST;

  // This is just exercising the static_assert in GPArena that ensures we can have fairly small
  // objectsize even in the presence of preservation.
  GPArena<0, 8> smallest;
  GPArena<4, 16> smallest_with_preserve;

  END_TEST;
}

static bool basic_lifo() {
  BEGIN_TEST;

  GPArena<0, 8> arena;
  ASSERT_EQ(arena.Init("test", 4), ZX_OK);

  void* first = arena.Alloc();
  ASSERT_NONNULL(first);

  void* second = arena.Alloc();
  ASSERT_NONNULL(second);

  // Alloc should always return the last Free.
  arena.Free(second);
  EXPECT_EQ(second, arena.Alloc());

  // If we Free multiple we should get them back in last-in-first-out order.
  arena.Free(second);
  arena.Free(first);
  EXPECT_EQ(first, arena.Alloc());
  EXPECT_EQ(second, arena.Alloc());

  // Cleanup.
  arena.Free(second);
  arena.Free(first);

  END_TEST;
}

static bool out_of_memory() {
  BEGIN_TEST;

  // Use large objects so we can store all the allocations in a stack array.
  GPArena<0, 512> arena;
  constexpr int count = kPageSize / 512;
  ASSERT_EQ(arena.Init("test", count), ZX_OK);
  void* allocs[count];

  // Allocate all objects from the arena.
  for (int i = 0; i < count; i++) {
    allocs[i] = arena.Alloc();
    ASSERT_NONNULL(allocs[i]);
  }

  // Unless we calculated wrong, allocations should now fail.
  EXPECT_NULL(arena.Alloc());
  EXPECT_NULL(arena.Alloc());

  // Should be able to put objects back and then successfully re-allocate them.
  arena.Free(allocs[count - 1]);
  arena.Free(allocs[count - 2]);
  EXPECT_EQ(allocs[count - 2], arena.Alloc());
  EXPECT_EQ(allocs[count - 1], arena.Alloc());

  // Once we re-allocate the ones we put back, future allocations should be back to failing.
  EXPECT_NULL(arena.Alloc());
  EXPECT_NULL(arena.Alloc());

  // Cleanup.
  for (int i = 0; i < count; i++) {
    arena.Free(allocs[i]);
  }

  END_TEST;
}

static bool does_preserve() {
  BEGIN_TEST;

  constexpr int preserve = 8;
  constexpr char magic[preserve + 1] = "preserve";

  GPArena<preserve, 16> arena;
  constexpr int count = 4;
  ASSERT_EQ(arena.Init("test", count), ZX_OK);
  void* allocs[count];

  // Allocate all our objects, and initialize them with the magic data.
  for (int i = 0; i < count; i++) {
    allocs[i] = arena.Alloc();
    ASSERT_NONNULL(allocs[i]);
    memcpy(allocs[i], magic, preserve);
  }

  // Return the objects back to the allocator.
  for (int i = 0; i < count; i++) {
    arena.Free(allocs[i]);
  }

  // Whilst unallocated the preserve region should be unchanged.
  for (int i = 0; i < count; i++) {
    EXPECT_EQ(memcmp(allocs[i], magic, preserve), 0);
  }

  // Reallocate the object and validate that allocation didn't destroy the preserve region.
  for (int i = 0; i < count; i++) {
    allocs[i] = arena.Alloc();
    ASSERT_NONNULL(allocs[i]);
  }
  for (int i = 0; i < count; i++) {
    EXPECT_EQ(memcmp(allocs[i], magic, preserve), 0);
  }

  // Cleanup.
  for (int i = 0; i < count; i++) {
    arena.Free(allocs[i]);
  }

  END_TEST;
}

// Small helper to do offset arithmetic of an arena Base, keeping it as a void*
template <size_t P, size_t O>
static inline void* base_offset(const GPArena<P, O>& arena, size_t offset) {
  return reinterpret_cast<void*>(reinterpret_cast<uintptr_t>(arena.Base()) + offset);
}

static bool committed_monotonic() {
  BEGIN_TEST;
  GPArena<0, 8> arena;
  ASSERT_EQ(arena.Init("test", 4), ZX_OK);

  // Initially Alloc has not been called, and so Committed can never be true.
  EXPECT_FALSE(arena.Committed(base_offset(arena, 0)));
  EXPECT_FALSE(arena.Committed(base_offset(arena, 8)));
  EXPECT_FALSE(arena.Committed(base_offset(arena, 16)));

  // Perform an allocation check Committed is true for that value, but no other.
  EXPECT_EQ(arena.Alloc(), base_offset(arena, 0));
  EXPECT_TRUE(arena.Committed(base_offset(arena, 0)));
  EXPECT_FALSE(arena.Committed(base_offset(arena, 8)));
  EXPECT_FALSE(arena.Committed(base_offset(arena, 16)));

  // Perform another allocation, Committed should be true for it and previous allocation.
  EXPECT_EQ(arena.Alloc(), base_offset(arena, 8));
  EXPECT_TRUE(arena.Committed(base_offset(arena, 0)));
  EXPECT_TRUE(arena.Committed(base_offset(arena, 8)));
  EXPECT_FALSE(arena.Committed(base_offset(arena, 16)));

  // Returning the allocated objects should have no impact on what Committed returns.
  arena.Free(base_offset(arena, 8));
  EXPECT_TRUE(arena.Committed(base_offset(arena, 0)));
  EXPECT_TRUE(arena.Committed(base_offset(arena, 8)));
  EXPECT_FALSE(arena.Committed(base_offset(arena, 16)));

  arena.Free(base_offset(arena, 0));
  EXPECT_TRUE(arena.Committed(base_offset(arena, 0)));
  EXPECT_TRUE(arena.Committed(base_offset(arena, 8)));
  EXPECT_FALSE(arena.Committed(base_offset(arena, 16)));

  END_TEST;
}

// Allocates every object in an arena, then frees them all and allocates them all again. This checks
// that the free list can refer to every object in the arena, including for object sizes that are
// not a power of two.
template <size_t P, size_t O>
static bool alloc_all() {
  BEGIN_TEST;

  // Use enough objects that the arena has to grow several times, and that fill a whole number of
  // pages, so that the arena holds exactly this many objects.
  constexpr size_t kCount = 16 * kPageSize / O;
  static_assert(RoundUpPageSize(kCount * O) / O == kCount);

  GPArena<P, O> arena;
  ASSERT_EQ(arena.Init("test", kCount), ZX_OK);

  fbl::AllocChecker ac;
  ktl::unique_ptr<void*[]> allocs = ktl::make_unique<void*[]>(&ac, kCount);
  ASSERT_TRUE(ac.check());

  // Objects are initially allocated in address order.
  for (size_t i = 0; i < kCount; i++) {
    allocs[i] = arena.Alloc();
    ASSERT_NONNULL(allocs[i]);
    EXPECT_EQ(base_offset(arena, i * O), allocs[i]);
  }
  EXPECT_NULL(arena.Alloc());

  // Free the objects in an order other than address order: first the objects with an even index,
  // then those with an odd index.
  constexpr size_t kEvens = (kCount + 1) / 2;
  auto free_order = [](size_t n) { return n < kEvens ? 2 * n : 2 * (n - kEvens) + 1; };
  for (size_t n = 0; n < kCount; n++) {
    arena.Free(allocs[free_order(n)]);
  }

  // As the free list is LIFO, the objects should be reallocated in the reverse of that order.
  for (size_t n = kCount; n-- > 0;) {
    EXPECT_EQ(allocs[free_order(n)], arena.Alloc());
  }
  EXPECT_NULL(arena.Alloc());

  // Cleanup.
  for (size_t i = 0; i < kCount; i++) {
    arena.Free(allocs[i]);
  }

  END_TEST;
}

static bool init_max_count() {
  BEGIN_TEST;

  using Arena = GPArena<0, 8>;
  constexpr size_t kMaxCount = Arena::MaxCount();
  constexpr size_t kObjectsPerPage = kPageSize / 8;
  static_assert(kMaxCount % kObjectsPerPage != 0);

  // More objects than the free list can refer to.
  {
    Arena arena;
    EXPECT_EQ(arena.Init("test", kMaxCount + 1), ZX_ERR_OUT_OF_RANGE);
  }

  // Not more objects than the free list can refer to, but rounding up to a whole number of pages
  // would make room for more objects than that.
  {
    Arena arena;
    EXPECT_EQ(arena.Init("test", kMaxCount), ZX_ERR_OUT_OF_RANGE);
  }

  // The largest number of objects that both fills a whole number of pages and does not exceed
  // kMaxCount. Nothing is committed until objects are allocated, so this only reserves address
  // space.
  {
    Arena arena;
    ASSERT_EQ(arena.Init("test", kMaxCount / kObjectsPerPage * kObjectsPerPage), ZX_OK);
    void* object = arena.Alloc();
    EXPECT_EQ(arena.Base(), object);
    arena.Free(object);
  }

  END_TEST;
}

// A helper that continuously allocates and frees a single object.
template <size_t P, size_t O>
struct ArenaAllocFixture {
  GPArena<P, O> arena;
  ktl::atomic<bool> stop_requested{};

  // A thread_start_routine for use with Thread::Create.
  static int run(void* arg) { return static_cast<ArenaAllocFixture<P, O>*>(arg)->Run(); }
  int Run() {
    unsigned int allocations = 0;
    while (1) {
      void* v = arena.Alloc();
      // On any failure just return. That we terminated at all is the error signal to our parent.
      if (v == nullptr) {
        return -1;
      }
      arena.Free(v);
      // Check every so often if someone is trying to stop us. We don't check every iteration to
      // maximize the chance of actually doing Alloc and Free concurrently.
      allocations++;
      if (allocations % 100 == 0 && stop_requested.load(ktl::memory_order_relaxed)) {
        return 0;
      }
    }
  }
};

static bool parallel_alloc() {
  BEGIN_TEST;

  using Fixture = ArenaAllocFixture<0, 8>;
  Fixture fixture;
  ASSERT_EQ(fixture.arena.Init("test", 4), ZX_OK);

  // Spin up two instances of the allocation fixture that will run in parallel.
  auto t1 = Thread::Create("gparena worker1", Fixture::run, &fixture, DEFAULT_PRIORITY);
  auto t2 = Thread::Create("gparena worker2", Fixture::run, &fixture, DEFAULT_PRIORITY);
  t1->Resume();
  t2->Resume();

  // Attempt to join one of the threads, letting it run for a bit. If the join succeeds this means
  // the fixture terminated, which indicates it encountered an error.
  zx_status_t status = t1->Join(nullptr, current_mono_time() + ZX_MSEC(500));
  EXPECT_NE(status, ZX_OK);
  // Check that the other thread is still running as well.
  status = t2->Join(nullptr, current_mono_time());
  EXPECT_NE(status, ZX_OK);

  // Cleanup.
  fixture.stop_requested.store(true, ktl::memory_order_relaxed);
  status = t1->Join(nullptr, current_mono_time() + ZX_SEC(5));
  EXPECT_EQ(status, ZX_OK);
  status = t2->Join(nullptr, current_mono_time() + ZX_SEC(5));
  EXPECT_EQ(status, ZX_OK);

  END_TEST;
}

// A helper that continuously allocates a few objects at a time, tags each object with a value
// unique to that allocation, and checks that the tags are unchanged before freeing the objects. If
// the arena ever hands out an object that is already allocated, such as due to an ABA race in the
// free list, a tag will be overwritten.
template <size_t P, size_t O>
struct ArenaExclusiveFixture {
  static_assert(O % sizeof(uint64_t) == 0, "Objects must be able to hold an aligned tag");

  // Holding more than one object at a time allows other threads to rearrange the free list while a
  // thread is popping from it, which is the race that the free list's generation count prevents.
  static constexpr size_t kObjectsPerThread = 3;

  GPArena<P, O> arena;
  ktl::atomic<bool> stop_requested{};
  ktl::atomic<uint64_t> thread_count{};

  // A thread_start_routine for use with Thread::Create.
  static int run(void* arg) { return static_cast<ArenaExclusiveFixture<P, O>*>(arg)->Run(); }
  int Run() {
    // Start each thread's tags in a different range, so that tags are unique across threads.
    uint64_t next_tag = (thread_count.fetch_add(1, ktl::memory_order_relaxed) + 1) << 48u;
    for (unsigned int iterations = 1;; iterations++) {
      void* objects[kObjectsPerThread] = {};
      uint64_t tags[kObjectsPerThread] = {};
      int result = 0;
      for (size_t i = 0; i < kObjectsPerThread; i++) {
        objects[i] = arena.Alloc();
        if (objects[i] == nullptr) {
          result = -1;
          break;
        }
        tags[i] = next_tag++;
        Tag(objects[i]).store(tags[i], ktl::memory_order_relaxed);
      }
      // Free every object we allocated, even if there was an error, so the arena can be destroyed.
      for (size_t i = 0; i < kObjectsPerThread && objects[i] != nullptr; i++) {
        if (Tag(objects[i]).load(ktl::memory_order_relaxed) != tags[i]) {
          result = -2;
        }
        arena.Free(objects[i]);
      }
      if (result != 0) {
        return result;
      }
      // Check every so often if someone is trying to stop us.
      if (iterations % 100 == 0 && stop_requested.load(ktl::memory_order_relaxed)) {
        return 0;
      }
    }
  }

  // Tags are accessed atomically, as multiple threads may access the same object if the arena is
  // broken.
  static ktl::atomic_ref<uint64_t> Tag(void* object) {
    return ktl::atomic_ref<uint64_t>(*static_cast<uint64_t*>(object));
  }
};

static bool parallel_alloc_exclusive() {
  BEGIN_TEST;

  using Fixture = ArenaExclusiveFixture<0, 8>;
  Fixture fixture;
  // Use more threads than most test environments have CPUs, so that threads are sometimes
  // preempted part way through updating the free list, as well as racing with each other.
  constexpr size_t kNumThreads = 8;
  ASSERT_EQ(fixture.arena.Init("test", kNumThreads * Fixture::kObjectsPerThread), ZX_OK);

  Thread* threads[kNumThreads];
  for (Thread*& thread : threads) {
    thread = Thread::Create("gparena worker", Fixture::run, &fixture, DEFAULT_PRIORITY);
    thread->Resume();
  }

  // Let the threads run for a bit, then stop them and check that none of them found an error.
  Thread::Current::SleepRelative(ZX_MSEC(500));
  fixture.stop_requested.store(true, ktl::memory_order_relaxed);
  for (Thread* thread : threads) {
    int retcode = -1;
    EXPECT_EQ(thread->Join(&retcode, current_mono_time() + ZX_SEC(5)), ZX_OK);
    EXPECT_EQ(retcode, 0);
  }

  END_TEST;
}

static bool parallel_grow_memory() {
  BEGIN_TEST;

  constexpr int count = kPageSize * 32 / 8;

  fbl::AllocChecker ac;
  ktl::unique_ptr<void*[]> allocs = ktl::make_unique<void*[]>(&ac, count);
  ASSERT_TRUE(ac.check());

  using Fixture = ArenaAllocFixture<0, 8>;
  Fixture fixture;
  ASSERT_EQ(fixture.arena.Init("test", count), ZX_OK);

  // Spin up a worker that will perform allocations in parallel whilst we are causing the arena
  // to need to be grown.
  auto t = Thread::Create("gparena worker", Fixture::run, &fixture, DEFAULT_PRIORITY);
  t->Resume();

  // let the worker run for a bit to make sure its started.
  zx_status_t status = t->Join(nullptr, current_mono_time() + ZX_MSEC(10));
  EXPECT_NE(status, ZX_OK);

  // Allocate all the rest of the objects causing arena to have to grow.
  for (int i = 0; i < count - 1; i++) {
    allocs[i] = fixture.arena.Alloc();
    EXPECT_NONNULL(allocs[i]);
  }

  // Worker should still be running fine.
  status = t->Join(nullptr, current_mono_time() + ZX_MSEC(10));
  EXPECT_NE(status, ZX_OK);

  // Cleanup.
  fixture.stop_requested.store(true, ktl::memory_order_relaxed);
  status = t->Join(nullptr, current_mono_time() + ZX_SEC(5));
  EXPECT_EQ(status, ZX_OK);

  for (int i = 0; i < count - 1; i++) {
    fixture.arena.Free(allocs[i]);
  }

  END_TEST;
}

static bool test_confine() {
  BEGIN_TEST;
  constexpr int kMaxItems = 32;
  constexpr int kSize = 64;

  GPArena<0, kSize> arena;
  void* allocs[kMaxItems];

  ASSERT_EQ(arena.Init("test", kMaxItems), ZX_OK);

  for (int i = 0; i < kMaxItems; i++) {
    allocs[i] = arena.Alloc();
  }

  const uintptr_t base = reinterpret_cast<uintptr_t>(arena.Base());
  for (int i = 0; i < 2 * kMaxItems; i++) {
    uintptr_t expected_object = base + i * kSize;
    if (i < kMaxItems) {
      EXPECT_EQ(expected_object, arena.Confine(expected_object));
    } else {
      EXPECT_EQ(base, arena.Confine(expected_object));
    }
  }

  EXPECT_EQ(base, arena.Confine(-1ull));
  EXPECT_EQ(base, arena.Confine(base - 1));
  EXPECT_EQ(base, arena.Confine(base - kSize - 1));

  for (int i = 0; i < kMaxItems; i++) {
    arena.Free(allocs[i]);
  }

  END_TEST;
}

#define GPARENA_UNITTEST(fname) UNITTEST(#fname, fname)

UNITTEST_START_TESTCASE(gparena_tests)
GPARENA_UNITTEST(can_declare_small_objectsize)
GPARENA_UNITTEST(basic_lifo)
GPARENA_UNITTEST(out_of_memory)
GPARENA_UNITTEST(does_preserve)
GPARENA_UNITTEST(committed_monotonic)
UNITTEST("alloc_all<0, 8>", (alloc_all<0, 8>))
UNITTEST("alloc_all<24, 40>", (alloc_all<24, 40>))
GPARENA_UNITTEST(init_max_count)
GPARENA_UNITTEST(parallel_alloc)
GPARENA_UNITTEST(parallel_alloc_exclusive)
GPARENA_UNITTEST(parallel_grow_memory)
GPARENA_UNITTEST(test_confine)
UNITTEST_END_TESTCASE(gparena_tests, "gparena_tests", "GPArena test")
