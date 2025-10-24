// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <stdlib.h>
#include <unistd.h>

#include <string>

#include <fbl/unique_fd.h>
#include <gtest/gtest.h>

#include "src/starnix/tests/selinux/userspace/util.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

namespace {

TEST(AppendTest, DontRequireGetattrForAppend) {
  fbl::unique_fd fd;
  {
    auto fscreate =
        ScopedTaskAttrResetter::SetTaskAttr("fscreate", "test_u:object_r:append_only_file_t:s0");
    fd = fbl::unique_fd(open("/tmp", O_APPEND | O_WRONLY | O_TMPFILE, 0644));
    ASSERT_TRUE(fd.is_valid()) << errno;
  }

  auto enforce = ScopedEnforcement::SetEnforcing();
  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:append_test_t:s0", [&] {
    char message[] = "abcdef";
    EXPECT_THAT(write(fd.get(), message, sizeof(message)),
                SyscallSucceedsWithValue(sizeof(message)));
  }));
}

TEST(AppendTest, DontRequireGetattrForSeek) {
  fbl::unique_fd fd;
  {
    auto fscreate =
        ScopedTaskAttrResetter::SetTaskAttr("fscreate", "test_u:object_r:write_only_file_t:s0");
    fd = fbl::unique_fd(open("/tmp", O_WRONLY | O_TMPFILE, 0644));
    ASSERT_TRUE(fd.is_valid()) << errno;
  }

  auto enforce = ScopedEnforcement::SetEnforcing();

  ASSERT_TRUE(RunSubprocessAs("test_u:test_r:append_test_t:s0", [&] {
    EXPECT_THAT(lseek(fd.get(), 0, SEEK_END), SyscallSucceeds());
  }));
}

}  // namespace

extern std::string DoPrePolicyLoadWork() { return "append.pp"; }
