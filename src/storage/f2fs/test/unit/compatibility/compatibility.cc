// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fcntl.h>

#include <gtest/gtest.h>

#include "src/storage/f2fs/f2fs.h"
#include "src/storage/f2fs/test/unit/unit_lib.h"
#include "src/storage/f2fs/vnode.h"

namespace f2fs {
namespace {

class SimpleIOTest : public F2fsFakeDevTestFixture {
 public:
  SimpleIOTest()
      : F2fsFakeDevTestFixture(
            TestOptions{.block_count = kSectorCount100MiB, .image_file = "simple_io.img.zst"}) {}
};

TEST_F(SimpleIOTest, SimpleIO) {
  fbl::RefPtr<fs::Vnode> vnode;
  FileTester::Lookup(root_dir_.get(), "test", &vnode);
  auto test_file = fbl::RefPtr<File>::Downcast(std::move(vnode));

  std::string target_string("hello");

  auto r_buf = std::make_unique<char[]>(kPageSize);
  FileTester::ReadFromFile(test_file.get(), r_buf.get(), target_string.length() + 1, 0);
  std::string str(r_buf.get(), target_string.length());
  ASSERT_EQ(str, target_string);

  test_file->Close();
}

class DirectoryTest : public F2fsFakeDevTestFixture {
 public:
  DirectoryTest()
      : F2fsFakeDevTestFixture(TestOptions{.block_count = kSectorCount100MiB,
                                           .image_file = "directory_test.img.zst"}) {}
};

TEST_F(DirectoryTest, Directory) {
  fbl::RefPtr<fs::Vnode> vnode;
  FileTester::Lookup(root_dir_.get(), "depth", &vnode);
  auto test_dir = fbl::RefPtr<Dir>::Downcast(std::move(vnode));

  constexpr int kDirDepth = 60;

  // From "depth", recursively find directories from "0" to "59"
  for (int depth = 0; depth < kDirDepth; ++depth) {
    FileTester::Lookup(test_dir.get(), std::to_string(depth), &vnode);
    test_dir->Close();
    test_dir = fbl::RefPtr<Dir>::Downcast(std::move(vnode));
  }

  // Directory "60" should not be exist
  ASSERT_EQ(test_dir->Lookup(std::to_string(kDirDepth), &vnode), ZX_ERR_NOT_FOUND);

  test_dir->Close();

  // "width" should contain "0", "1", ..., "19", "120", "121", ..., "139"
  FileTester::Lookup(root_dir_.get(), "width", &vnode);
  test_dir = fbl::RefPtr<Dir>::Downcast(std::move(vnode));

  for (int i = 0; i <= 19; ++i) {
    fbl::RefPtr<fs::Vnode> child_vnode;
    FileTester::Lookup(test_dir.get(), std::to_string(i), &child_vnode);
    child_vnode->Close();
  }
  for (int i = 120; i <= 139; ++i) {
    fbl::RefPtr<fs::Vnode> child_vnode;
    FileTester::Lookup(test_dir.get(), std::to_string(i), &child_vnode);
    child_vnode->Close();
  }

  // "width" should not contain "20", "21", ..., "59"
  for (int i = 20; i <= 59; ++i) {
    fbl::RefPtr<fs::Vnode> child_vnode;
    ASSERT_EQ(test_dir->Lookup(std::to_string(i), &vnode), ZX_ERR_NOT_FOUND);
  }

  test_dir->Close();
}

}  // namespace
}  // namespace f2fs
