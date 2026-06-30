// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sched.h>
#include <stdlib.h>
#include <sys/mount.h>
#include <sys/stat.h>

#include <gtest/gtest.h>

#include "fault_test.h"
#include "fault_test_suite.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

namespace {

int CreateTempFile() {
  char tmpl[] = "/tmp/tmpfile.XXXXXX";
  return mkstemp(tmpl);
}

INSTANTIATE_TEST_SUITE_P(TmpfsFaultTest, FaultFileTest, ::testing::Values(CreateTempFile));

TEST(TmpfsTest, DefaultStickyBit) {
  if (!test_helper::HasSysAdmin()) {
    GTEST_SKIP() << "need CAP_SYS_ADMIN to mount tmpfs";
  }

  test_helper::ScopedTempDir temp_dir;

  auto mount_result = test_helper::ScopedMount::Mount("none", temp_dir.path(), "tmpfs", 0, nullptr);
  ASSERT_TRUE(mount_result.is_ok())
      << "mount(tmpfs) failed: " << strerror(mount_result.error_value());

  struct stat stat_buf;
  ASSERT_EQ(stat(temp_dir.path().c_str(), &stat_buf), 0);
  EXPECT_EQ(stat_buf.st_mode & 07777, 01777u) << "Sticky bit should be set on tmpfs root";
}

}  // namespace
