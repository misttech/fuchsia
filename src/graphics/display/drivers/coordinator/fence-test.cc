// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/coordinator/fence.h"

#include <lib/async-testing/test_loop.h>
#include <lib/async/default.h>
#include <lib/driver/testing/cpp/driver_runtime.h>
#include <lib/driver/testing/cpp/scoped_global_logger.h>
#include <lib/zx/event.h>
#include <lib/zx/result.h>

#include <vector>

#include <fbl/ref_ptr.h>
#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/lib/testing/predicates/status.h"

namespace display_coordinator {

class FakeFenceOwner : public FenceOwner {
 public:
  void OnFenceSignaled(FenceReference* fence) override { signaled_fences_.push_back(fence); }

  void OnRefForFenceDead(Fence* fence) override {
    // TODO(https://fxbug.dev/394422104): it is not ideal to require implementors of `FenceCallback`
    // to call `OnRefDead()` in order to maintain the fence's ref-count. This should be handled
    // between `Fence`/`FenceReference` without muddying the `FenceCallback` contract.
    fence->OnRefDead();
  }

  const std::vector<FenceReference*>& signaled_fences() { return signaled_fences_; }

 private:
  std::vector<FenceReference*> signaled_fences_;
};

class FenceTest : public testing::Test {
 public:
  void SetUp() override {
    static constexpr display::EventId kEventId(1);

    zx::event event;
    ASSERT_OK(zx::event::create(/*options=*/0, &event));

    fence_ = fbl::AdoptRef(
        new Fence(&fence_owner_, driver_dispatcher_->borrow(), kEventId, std::move(event)));
  }

  void TearDown() override { fence_->ClearRef(); }

  fbl::RefPtr<Fence> fence() { return fence_; }
  FakeFenceOwner& fence_owner() { return fence_owner_; }

 protected:
  fdf_testing::ScopedGlobalLogger logger_;
  fdf_testing::DriverRuntime driver_runtime_;

  fdf::UnownedSynchronizedDispatcher driver_dispatcher_ = driver_runtime_.GetForegroundDispatcher();

  fbl::RefPtr<Fence> fence_;
  FakeFenceOwner fence_owner_;
};

TEST_F(FenceTest, MultipleRefs_OnePurpose) {
  fence()->CreateRef();
  fbl::RefPtr<FenceReference> reference_one = fence()->GetReference();
  fbl::RefPtr<FenceReference> reference_two = fence()->GetReference();
}

TEST_F(FenceTest, MultipleRefs_MultiplePurposes) {
  fence()->CreateRef();
  fbl::RefPtr<FenceReference> reference_one = fence()->GetReference();
  fence()->CreateRef();
  fbl::RefPtr<FenceReference> reference_two = fence()->GetReference();
  fence()->CreateRef();
  fbl::RefPtr<FenceReference> reference_three = fence()->GetReference();
  ASSERT_OK(reference_two->StartReadyWait());
  ASSERT_OK(reference_one->StartReadyWait());

  reference_three->Signal();
  driver_runtime_.RunUntilIdle();

  reference_three->Signal();
  driver_runtime_.RunUntilIdle();

  EXPECT_THAT(fence_owner().signaled_fences(),
              testing::ElementsAre(reference_two.get(), reference_one.get()));
}

}  // namespace display_coordinator
