// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include <fbl/unique_fd.h>

#include "src/storage/fs_test/fs_test_fixture.h"

namespace fs_test {
namespace {

using LseekTest = FilesystemTest;

TEST_P(LseekTest, Position) {
  const std::string filename = GetPath("lseek_position");
  fbl::unique_fd fd(open(filename.c_str(), O_CREAT | O_RDWR, S_IRUSR | S_IWUSR));
  ASSERT_TRUE(fd);

  // File offset initialized to zero.
  ASSERT_EQ(lseek(fd.get(), 0, SEEK_CUR), 0);
  ASSERT_EQ(lseek(fd.get(), 0, SEEK_SET), 0);

  const char* const str = "hello";
  const size_t len = strlen(str);
  ASSERT_EQ(write(fd.get(), str, len), static_cast<ssize_t>(len));

  // After writing, the offset has been updated.
  ASSERT_EQ(lseek(fd.get(), 0, SEEK_CUR), static_cast<off_t>(len));
  ASSERT_EQ(lseek(fd.get(), 0, SEEK_END), static_cast<off_t>(len));

  // Reset the offset to the start of the file.
  ASSERT_EQ(lseek(fd.get(), -len, SEEK_END), 0);

  // Read the entire file.
  auto buf = std::make_unique<char[]>(len + 1);
  ASSERT_EQ(read(fd.get(), buf.get(), len), static_cast<ssize_t>(len));
  ASSERT_EQ(memcmp(buf.get(), str, len), 0);

  // Seek and read part of the file.
  ASSERT_EQ(lseek(fd.get(), 1, SEEK_SET), 1);
  ASSERT_EQ(read(fd.get(), buf.get(), len - 1), static_cast<ssize_t>(len - 1));
  ASSERT_EQ(memcmp(buf.get(), &str[1], len - 1), 0);

  ASSERT_EQ(unlink(filename.c_str()), 0);
}

TEST_P(LseekTest, OutOfBounds) {
  const std::string filename = GetPath("lseek_out_of_bounds");
  fbl::unique_fd fd(open(filename.c_str(), O_CREAT | O_RDWR, S_IRUSR | S_IWUSR));
  ASSERT_TRUE(fd);

  const char* const str = "hello";
  const size_t len = strlen(str);
  ASSERT_EQ(write(fd.get(), str, len), static_cast<ssize_t>(len));

  // After writing, the offset has been updated.
  ASSERT_EQ(lseek(fd.get(), 0, SEEK_CUR), static_cast<off_t>(len));

  // Seek beyond the end of the file.
  ASSERT_EQ(lseek(fd.get(), 1, SEEK_CUR), static_cast<off_t>(len + 1));
  ASSERT_EQ(lseek(fd.get(), 2, SEEK_END), static_cast<off_t>(len + 2));
  ASSERT_EQ(lseek(fd.get(), len + 3, SEEK_SET), static_cast<off_t>(len + 3));

  // Seek before the start of the file.
  ASSERT_EQ(lseek(fd.get(), 0, SEEK_SET), 0);

  // Negative seek is not allowed on Fuchsia.
  ASSERT_EQ(lseek(fd.get(), -2, SEEK_CUR), -1);
  ASSERT_EQ(lseek(fd.get(), -2, SEEK_SET), -1);
  ASSERT_EQ(lseek(fd.get(), -(len + 2), SEEK_END), -1);

  ASSERT_EQ(unlink(filename.c_str()), 0);
}

TEST_P(LseekTest, ZeroFill) {
  const std::string filename = GetPath("lseek_zero_fill");
  fbl::unique_fd fd(open(filename.c_str(), O_CREAT | O_RDWR, S_IRUSR | S_IWUSR));
  ASSERT_TRUE(fd);

  const char* const str = "hello";
  const size_t len = strlen(str);
  ASSERT_EQ(write(fd.get(), str, len), static_cast<ssize_t>(len));

  // After writing, the offset and length have been updated.
  ASSERT_EQ(lseek(fd.get(), 0, SEEK_CUR), static_cast<off_t>(len));
  struct stat st;
  ASSERT_EQ(fstat(fd.get(), &st), 0);
  ASSERT_EQ(st.st_size, static_cast<off_t>(len));

  // Seek beyond the end of the file.
  size_t zeros = 10;
  ASSERT_EQ(lseek(fd.get(), len + zeros, SEEK_SET), static_cast<off_t>(len + zeros));

  // This does not change the length of the file.
  ASSERT_EQ(fstat(fd.get(), &st), 0);
  ASSERT_EQ(st.st_size, static_cast<off_t>(len));

  // From the POSIX specification:
  //
  // "Before any action described below is taken, and if nbyte is zero and the
  // file is a regular file, the write() function may detect and return
  // errors as described below. In the absence of errors, or if error
  // detection is not performed, the write() function shall return zero
  // and have no other results."
  ASSERT_EQ(write(fd.get(), str, 0), 0) << errno;
  ASSERT_EQ(fstat(fd.get(), &st), 0);
  ASSERT_EQ(st.st_size, static_cast<off_t>(len));

  // Zero-extend the file up to the sentinel value.
  char sentinel = 'a';
  ASSERT_EQ(write(fd.get(), &sentinel, 1), 1);
  ASSERT_EQ(fstat(fd.get(), &st), 0);
  ASSERT_EQ(st.st_size, static_cast<off_t>(len + zeros + 1));

  // Validate the file contents.
  {
    auto expected = std::make_unique<char[]>(len + zeros + 1);
    memcpy(expected.get(), str, len);
    memset(&expected[len], 0, zeros);
    expected[len + zeros] = 'a';

    auto buf = std::make_unique<char[]>(len + zeros + 1);
    ASSERT_EQ(lseek(fd.get(), 0, SEEK_SET), 0);
    ASSERT_EQ(read(fd.get(), buf.get(), len + zeros + 1), static_cast<ssize_t>(len + zeros + 1));
    ASSERT_EQ(memcmp(buf.get(), expected.get(), len + zeros + 1), 0);
  }

  // Truncate and observe the (old) sentinel value has been
  // overwritten with zeros.
  ASSERT_EQ(ftruncate(fd.get(), len), 0);
  zeros *= 2;
  ASSERT_EQ(lseek(fd.get(), len + zeros, SEEK_SET), static_cast<off_t>(len + zeros));
  ASSERT_EQ(write(fd.get(), &sentinel, 1), 1);
  ASSERT_EQ(fstat(fd.get(), &st), 0);
  ASSERT_EQ(st.st_size, static_cast<off_t>(len + zeros + 1));

  {
    auto expected = std::make_unique<char[]>(len + zeros + 1);
    memcpy(expected.get(), str, len);
    memset(&expected[len], 0, zeros);
    expected[len + zeros] = 'a';

    auto buf = std::make_unique<char[]>(len + zeros + 1);
    ASSERT_EQ(lseek(fd.get(), 0, SEEK_SET), 0);
    ASSERT_EQ(read(fd.get(), buf.get(), len + zeros + 1), static_cast<ssize_t>(len + zeros + 1));
    ASSERT_EQ(memcmp(buf.get(), expected.get(), len + zeros + 1), 0);
  }

  ASSERT_EQ(unlink(filename.c_str()), 0);
}

INSTANTIATE_TEST_SUITE_P(/*no prefix*/, LseekTest, testing::ValuesIn(AllTestFilesystems()),
                         testing::PrintToStringParamName());

}  // namespace
}  // namespace fs_test
