// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/lib/escher/base/ownable.h"

#include <atomic>
#include <thread>
#include <vector>

#include <gtest/gtest.h>

#include "src/lib/fxl/memory/ref_ptr.h"
#include "src/ui/lib/escher/base/owner.h"
#include "src/ui/lib/escher/base/type_info.h"

namespace {

enum class OwnableTypes {
  kOwnableBaseClassForTest = 1,
  kOwnable1 = 1 << 1,
  kOwnable2 = 1 << 2,
  kSubOwnable1 = 1 << 3,
  kSubOwnable2 = 1 << 4,
};

typedef escher::TypeInfo<OwnableTypes> OwnableTypeInfo;

class OwnableBaseClassForTest : public escher::Ownable<OwnableBaseClassForTest, OwnableTypeInfo> {
 public:
  static const TypeInfo kTypeInfo;
  const TypeInfo& type_info() const override { return OwnableBaseClassForTest::kTypeInfo; }

  using escher::Ownable<OwnableBaseClassForTest, OwnableTypeInfo>::owner;
};

class Ownable1 : public OwnableBaseClassForTest {
 public:
  static const TypeInfo kTypeInfo;
  const TypeInfo& type_info() const override { return Ownable1::kTypeInfo; }

  explicit Ownable1(size_t& destroyed_count) : destroyed_count_(destroyed_count) {}

  ~Ownable1() override { ++destroyed_count_; }

 private:
  size_t& destroyed_count_;
};

class Ownable2 : public OwnableBaseClassForTest {
 public:
  static const TypeInfo kTypeInfo;
  const TypeInfo& type_info() const override { return Ownable2::kTypeInfo; }
};

class SubOwnable1 : public Ownable1 {
 public:
  static const TypeInfo kTypeInfo;
  const TypeInfo& type_info() const override { return SubOwnable1::kTypeInfo; }

  explicit SubOwnable1(size_t& destroyed_count) : Ownable1(destroyed_count) {}
};

class SubOwnable2 : public Ownable2 {
 public:
  static const TypeInfo kTypeInfo;
  const TypeInfo& type_info() const override { return SubOwnable2::kTypeInfo; }
};

const OwnableTypeInfo OwnableBaseClassForTest::kTypeInfo("OwnableBaseClassForTest",
                                                         OwnableTypes::kOwnableBaseClassForTest);
const OwnableTypeInfo Ownable1::kTypeInfo("Ownable1", OwnableTypes::kOwnableBaseClassForTest,
                                          OwnableTypes::kOwnable1);
const OwnableTypeInfo Ownable2::kTypeInfo("Ownable2", OwnableTypes::kOwnableBaseClassForTest,
                                          OwnableTypes::kOwnable2);
const OwnableTypeInfo SubOwnable1::kTypeInfo("SubOwnable1", OwnableTypes::kOwnableBaseClassForTest,
                                             OwnableTypes::kOwnable1, OwnableTypes::kSubOwnable1);
const OwnableTypeInfo SubOwnable2::kTypeInfo("SubOwnable2", OwnableTypes::kOwnableBaseClassForTest,
                                             OwnableTypes::kOwnable2, OwnableTypes::kSubOwnable2);

class TestOwner : public escher::Owner<OwnableBaseClassForTest, OwnableTypeInfo> {
 public:
  explicit TestOwner(size_t& destroyed_count) : destroyed_count_(destroyed_count) {}

  fxl::RefPtr<Ownable1> NewOwnable1() {
    auto result = escher::Make<Ownable1>(destroyed_count_);
    EXPECT_EQ(1U, result->ref_count());
    BecomeOwnerOf(result.get());
    return result;
  }

  void OnReceiveOwnable(std::unique_ptr<OwnableBaseClassForTest> unreffed) override {
    EXPECT_EQ(0U, unreffed->ref_count());
    unreffed_.push_back(fxl::RefPtr<OwnableBaseClassForTest>(unreffed.release()));
  }

  size_t GetUnreffedCount() const { return unreffed_.size(); }
  void ClearUnreffed() {
    for (auto& owned : unreffed_) {
      // So that they are destroyed.
      RelinquishOwnershipOf(owned.get());
    }
    unreffed_.clear();
  }

 private:
  size_t& destroyed_count_;
  std::vector<fxl::RefPtr<OwnableBaseClassForTest>> unreffed_;
};

TEST(Ownable, ReceiveOwnables) {
  size_t destroyed_count = 0;
  auto owner = std::make_unique<TestOwner>(destroyed_count);
  EXPECT_EQ(0U, owner->ownable_count());

  auto ownable1 = owner->NewOwnable1();
  auto ownable2 = owner->NewOwnable1();
  EXPECT_EQ(ownable1->owner(), owner.get());
  EXPECT_EQ(ownable2->owner(), owner.get());
  EXPECT_EQ(2U, owner->ownable_count());
  EXPECT_EQ(0U, owner->GetUnreffedCount());
  EXPECT_EQ(0U, destroyed_count);
  EXPECT_NE(ownable1.get(), ownable2.get());

  ownable1 = ownable2;
  EXPECT_EQ(ownable1.get(), ownable2.get());
  EXPECT_EQ(2U, ownable1->ref_count());
  EXPECT_EQ(2U, owner->ownable_count());
  EXPECT_EQ(1U, owner->GetUnreffedCount());
  EXPECT_EQ(0U, destroyed_count);

  owner->ClearUnreffed();
  EXPECT_EQ(1U, owner->ownable_count());
  EXPECT_EQ(0U, owner->GetUnreffedCount());
  EXPECT_EQ(1U, destroyed_count);

  ownable2 = nullptr;
  EXPECT_EQ(1U, owner->ownable_count());
  EXPECT_EQ(0U, owner->GetUnreffedCount());
  EXPECT_EQ(1U, destroyed_count);

  ownable1 = nullptr;
  EXPECT_EQ(1U, owner->ownable_count());
  EXPECT_EQ(1U, owner->GetUnreffedCount());
  EXPECT_EQ(1U, destroyed_count);

  owner->ClearUnreffed();
  EXPECT_EQ(0U, owner->ownable_count());
  EXPECT_EQ(0U, owner->GetUnreffedCount());
  EXPECT_EQ(2U, destroyed_count);
}

class ThreadSafeImmediateOwner : public escher::Owner<OwnableBaseClassForTest, OwnableTypeInfo> {
 public:
  class AtomicOwnable : public OwnableBaseClassForTest {
   public:
    static const TypeInfo kTypeInfo;
    const TypeInfo& type_info() const override { return kTypeInfo; }

    explicit AtomicOwnable(std::atomic<size_t>& destroyed_count)
        : destroyed_count_(destroyed_count) {}
    ~AtomicOwnable() override { destroyed_count_.fetch_add(1, std::memory_order_relaxed); }

   private:
    std::atomic<size_t>& destroyed_count_;
  };

  explicit ThreadSafeImmediateOwner(std::atomic<size_t>& destroyed_count)
      : destroyed_count_(destroyed_count) {}

  fxl::RefPtr<AtomicOwnable> NewOwnable() {
    auto result = escher::Make<AtomicOwnable>(destroyed_count_);
    BecomeOwnerOf(result.get());
    return result;
  }

  void Relinquish(AtomicOwnable* ownable) { RelinquishOwnershipOf(ownable); }

  void OnReceiveOwnable(std::unique_ptr<OwnableBaseClassForTest> unreffed) override {
    // Let `unreffed` go out of scope immediately so `~Ownable()` decrements `ownable_count_`
    // on the releasing thread.
  }

 private:
  std::atomic<size_t>& destroyed_count_;
};

const OwnableTypeInfo ThreadSafeImmediateOwner::AtomicOwnable::kTypeInfo(
    "AtomicOwnable", OwnableTypes::kOwnableBaseClassForTest);

TEST(Ownable, ConcurrentCreateDestroyAndRelinquishUpdatesOwnableCountAtomically) {
  std::atomic<size_t> destroyed_count{0};
  auto owner = std::make_unique<ThreadSafeImmediateOwner>(destroyed_count);

  constexpr size_t kNumThreads = 4;
  constexpr size_t kIterationsPerThread = 250;

  // Pre-create a batch on the main thread to be released across worker threads.
  std::vector<std::vector<fxl::RefPtr<ThreadSafeImmediateOwner::AtomicOwnable>>> precreated(
      kNumThreads);
  for (size_t t = 0; t < kNumThreads; ++t) {
    precreated[t].reserve(kIterationsPerThread);
    for (size_t i = 0; i < kIterationsPerThread; ++i) {
      precreated[t].push_back(owner->NewOwnable());
    }
  }
  EXPECT_EQ(kNumThreads * kIterationsPerThread, owner->ownable_count());

  std::atomic<size_t> ready_workers{0};
  std::atomic<bool> start{false};
  std::vector<std::thread> workers;
  workers.reserve(kNumThreads);
  for (size_t t = 0; t < kNumThreads; ++t) {
    workers.emplace_back([&, t]() {
      ready_workers.fetch_add(1, std::memory_order_release);
      while (!start.load(std::memory_order_acquire)) {
        std::this_thread::yield();
      }
      for (size_t i = 0; i < kIterationsPerThread; ++i) {
        // Release pre-created ownable, triggering `OnReceiveOwnable()` and `~Ownable()`.
        precreated[t][i] = nullptr;

        // Create and destroy an ownable on this thread.
        auto transient = owner->NewOwnable();
        transient = nullptr;

        // Create an ownable and explicitly relinquish ownership before dropping it.
        auto relinquished = owner->NewOwnable();
        owner->Relinquish(relinquished.get());
        relinquished = nullptr;
      }
    });
  }

  while (ready_workers.load(std::memory_order_acquire) < kNumThreads) {
    std::this_thread::yield();
  }
  start.store(true, std::memory_order_release);
  for (auto& worker : workers) {
    worker.join();
  }

  EXPECT_EQ(0U, owner->ownable_count());
  EXPECT_EQ(3 * kNumThreads * kIterationsPerThread,
            destroyed_count.load(std::memory_order_relaxed));
}

}  // namespace
