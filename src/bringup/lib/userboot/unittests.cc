// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/elfldltl/testing/test-pipe-reader.h>
#include <lib/userboot/testing/launcher.h>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

namespace {

class UserbootTests : public ::testing::Test {
 protected:
  static void SetUpTestSuite() {
    auto launcher = userboot::testing::Launcher::Create();
    ASSERT_TRUE(launcher.is_ok());
    launcher_ = *std::move(launcher);
  }

  static void TearDownTestSuite() { launcher_ = {}; }

  static auto& launcher() { return launcher_; }

  void SetUp() override {
    ASSERT_FALSE(job_);
    ASSERT_NO_FATAL_FAILURE(job_.Init());
    ASSERT_FALSE(log_fd_);
    ASSERT_NO_FATAL_FAILURE(log_.Init(log_fd_));
  }

  fbl::unique_fd TakeLogFd() {
    EXPECT_TRUE(log_fd_);
    return std::exchange(log_fd_, {});
  }

  std::string FinishLog() { return std::move(log_).Finish(); }

  zx::result<zx::process> Launch(zx::vmo vmo, std::vector<zx::handle> handles) {
    return launcher().Launch(job_.Get(), std::move(vmo), TakeLogFd(), std::move(handles));
  }

  zx::result<zx::process> Launch(const char* file, std::vector<zx::handle> handles) {
    auto vmo = userboot::testing::GetExecutable(file);
    if (vmo.is_error()) {
      return vmo.take_error();
    }
    return launcher().Launch(job_.Get(), *std::move(vmo), TakeLogFd(), std::move(handles));
  }

 private:
  inline static userboot::testing::Launcher launcher_;

  userboot::testing::TestJob job_;
  elfldltl::testing::TestPipeReader log_;
  fbl::unique_fd log_fd_;
};

TEST_F(UserbootTests, BasicLaunch) {
  auto process = Launch("/pkg/test/userboot-lib-static-pie-test", {});
  ASSERT_TRUE(process.is_ok());

  auto result = userboot::testing::WaitForTermination(process->borrow());
  ASSERT_TRUE(result.is_ok());
  EXPECT_EQ(*result, 0);

  std::string log = FinishLog();
  EXPECT_THAT(log, ::testing::HasSubstr("Hello from userland!")) << log;
}

}  // namespace
