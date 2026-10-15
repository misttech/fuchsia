// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/unittest/unittest.h>

#include <kernel/scheduler.h>
#include <kernel/thread.h>

namespace {

struct TestContext {
  const Thread* exclude_thread;
};

}  // namespace

// Helper class to access private Scheduler State and Scheduler members.
class SchedulerRunQueueTestHelper {
 public:
  static void SetTimes(Thread* t, SchedTime start, SchedTime finish) TA_NO_THREAD_SAFETY_ANALYSIS {
    t->scheduler_state().start_time_ = start;
    t->scheduler_state().finish_time_ = finish;
  }

  static bool RunTest() TA_NO_THREAD_SAFETY_ANALYSIS {
    BEGIN_TEST;

    // Create a RunQueue instance.
    Scheduler::RunQueue rq;
    EXPECT_TRUE(rust_kernel_run_queue_is_empty(&rq), "Queue should be empty initially");
    EXPECT_EQ(0u, rust_kernel_run_queue_size(&rq), "Queue size should be 0 initially");
    EXPECT_NULL(rust_kernel_run_queue_front(&rq), "Queue front should be null initially");
    EXPECT_NULL(rust_kernel_run_queue_root(&rq), "Queue root should be null initially");

    // Create three threads. We don't start them.
    Thread* t1 =
        Thread::Create("test_t1", [](void*) -> int { return 0; }, nullptr, DEFAULT_PRIORITY);
    Thread* t2 =
        Thread::Create("test_t2", [](void*) -> int { return 0; }, nullptr, DEFAULT_PRIORITY);
    Thread* t3 =
        Thread::Create("test_t3", [](void*) -> int { return 0; }, nullptr, DEFAULT_PRIORITY);

    ASSERT_NONNULL(t1);
    ASSERT_NONNULL(t2);
    ASSERT_NONNULL(t3);

    // Initialize scheduler state.
    Scheduler::InitializeThread(t1, SchedulerState::BaseProfile{DEFAULT_PRIORITY});
    Scheduler::InitializeThread(t2, SchedulerState::BaseProfile{DEFAULT_PRIORITY});
    Scheduler::InitializeThread(t3, SchedulerState::BaseProfile{DEFAULT_PRIORITY});

    // Set custom start/finish times to test order and eligibility.
    // t1: start = 10, finish = 20
    // t2: start = 20, finish = 30
    // t3: start = 15, finish = 25
    SetTimes(t1, SchedTime{10}, SchedTime{20});
    SetTimes(t2, SchedTime{20}, SchedTime{30});
    SetTimes(t3, SchedTime{15}, SchedTime{25});

    // Insert them into the run queue.
    rust_kernel_run_queue_insert(&rq, t1);
    rust_kernel_run_queue_insert(&rq, t2);
    rust_kernel_run_queue_insert(&rq, t3);

    EXPECT_FALSE(rust_kernel_run_queue_is_empty(&rq), "Queue should not be empty");
    EXPECT_EQ(3u, rust_kernel_run_queue_size(&rq), "Queue size should be 3");
    EXPECT_EQ(t1, rust_kernel_run_queue_front(&rq), "Queue front should be t1");
    EXPECT_NONNULL(rust_kernel_run_queue_root(&rq), "Queue root should not be null");

    // Predicate that always returns true.
    auto true_pred = [](const Thread*, const void*) { return true; };

    // Test case 1: eligible_time = 9.
    // No thread is eligible because front (t1) start_time is 10.
    {
      Thread* result = rust_kernel_run_queue_find_earliest_eligible_thread(&rq, SchedTime{9},
                                                                           true_pred, nullptr);
      EXPECT_NULL(result, "No thread should be eligible at time 9");
    }

    // Test case 2: eligible_time = 12.
    // Only t1 is eligible.
    {
      Thread* result = rust_kernel_run_queue_find_earliest_eligible_thread(&rq, SchedTime{12},
                                                                           true_pred, nullptr);
      EXPECT_EQ(t1, result, "t1 should be the only eligible thread at time 12");
    }

    // Test case 3: eligible_time = 16.
    // t1 (finish 20) and t3 (finish 25) are eligible.
    // Should return t1 because it has the earliest finish time.
    {
      Thread* result = rust_kernel_run_queue_find_earliest_eligible_thread(&rq, SchedTime{16},
                                                                           true_pred, nullptr);
      EXPECT_EQ(t1, result, "t1 should be preferred over t3 at time 16 due to earlier finish time");
    }

    // Test case 4: eligible_time = 16 with predicate excluding t1.
    // C++ FindEarliestEligibleThread does not backtrack, so if it decides to
    // search a subtree (based on min_finish_time) and that subtree's best node
    // is rejected by the predicate, it may fail (return nullptr) even if there
    // are other eligible and acceptable nodes. Rust should match this behavior.
    {
      auto exclude_pred = [](const Thread* t, const void* ctx) {
        auto context = static_cast<const TestContext*>(ctx);
        return t != context->exclude_thread;
      };
      TestContext ctx{t1};
      Thread* rust_result = rust_kernel_run_queue_find_earliest_eligible_thread(&rq, SchedTime{16},
                                                                                exclude_pred, &ctx);
      EXPECT_NULL(rust_result, "Should return nullptr because C++ algorithm does not backtrack");
    }

    // Test case 5: Erase t1 and try again at time 16.
    // Should return t3 even without exclusion predicate.
    {
      Thread* erased = rust_kernel_run_queue_erase(&rq, t1);
      EXPECT_EQ(t1, erased, "Erase should return t1");

      Thread* result = rust_kernel_run_queue_find_earliest_eligible_thread(&rq, SchedTime{16},
                                                                           true_pred, nullptr);
      EXPECT_EQ(t3, result, "t3 should be chosen after t1 is erased");
    }

    // Test case 6: Iteration.
    // Should visit t3, t2 in order (t1 was erased).
    {
      RunQueueIterator iter = rust_kernel_run_queue_iterator_create(&rq);
      EXPECT_TRUE(rust_kernel_run_queue_iterator_is_valid(&iter),
                  "Iterator should be valid initially");
      EXPECT_EQ(t3, rust_kernel_run_queue_iterator_get(&iter), "First element should be t3");

      rust_kernel_run_queue_iterator_next(&iter);
      EXPECT_TRUE(rust_kernel_run_queue_iterator_is_valid(&iter),
                  "Iterator should be valid after first next");
      EXPECT_EQ(t2, rust_kernel_run_queue_iterator_get(&iter), "Second element should be t2");

      rust_kernel_run_queue_iterator_next(&iter);
      EXPECT_FALSE(rust_kernel_run_queue_iterator_is_valid(&iter),
                   "Iterator should be invalid after second next");
      EXPECT_NULL(rust_kernel_run_queue_iterator_get(&iter), "Element should be null after end");
    }

    // Clean up threads. We must erase them from the queue first.
    rust_kernel_run_queue_erase(&rq, t2);
    rust_kernel_run_queue_erase(&rq, t3);
    t1->Forget();
    t2->Forget();
    t3->Forget();

    END_TEST;
  }
};

namespace {

bool test_run_queue_basic() { return SchedulerRunQueueTestHelper::RunTest(); }

}  // namespace

UNITTEST_START_TESTCASE(scheduler_run_queue_tests)
UNITTEST("run_queue_basic", test_run_queue_basic)
UNITTEST_END_TESTCASE(scheduler_run_queue_tests, "scheduler_run_queue", "Scheduler Run Queue tests")
