// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fit/defer.h>
#include <lib/sync/completion.h>

#include "src/storage/lib/block_client/cpp/fake_block_device.h"
#include "src/storage/minfs/runner.h"
#include "src/storage/minfs/test/unit/journal_integration_fixture.h"

namespace minfs {
namespace {

constexpr uint8_t kFill = 0xe8;

class TruncateTest : public JournalIntegrationFixture {
 private:
  // Create a file with 2 blocks, then truncate down to 1 block. If the transaction succeeds we
  // should see the new length, but if it fails, we should still see the old length with the old
  // contents.
  void PerformOperation(Minfs& fs) final {
    auto root = fs.VnodeGet(kMinfsRootIno);
    ASSERT_TRUE(root.is_ok());
    zx::result foo = root->Create("foo", fs::CreationType::kFile);
    ASSERT_TRUE(foo.is_ok()) << foo.status_string();
    auto close = fit::defer([foo]() { ASSERT_EQ(foo->Close(), ZX_OK); });
    std::vector<uint8_t> buf(kMinfsBlockSize + 10, kFill);
    size_t written;
    ASSERT_EQ(foo->Write(buf.data(), buf.size(), 0, &written), ZX_OK);
    ASSERT_EQ(written, buf.size());

    ASSERT_EQ(foo->Truncate(1), ZX_OK);
  }
};

TEST_F(TruncateTest, EnsureOldDataWhenTransactionFails) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);

  // See the note in journal_test.cc regarding tuning these numbers.
  auto bcache = CutOffDevice(write_count() - UINT64_C(12) * kDiskBlocksPerFsBlock);

  // Since we cut off the transaction, we should see the old length with the old contents.
  auto fs_or = Runner::Create(loop.dispatcher(), std::move(bcache), MountOptions{});
  ASSERT_TRUE(fs_or.is_ok());

  // Open the 'foo' file.
  auto root = fs_or->minfs().VnodeGet(kMinfsRootIno);
  ASSERT_TRUE(root.is_ok());
  fbl::RefPtr<fs::Vnode> foo;
  ASSERT_EQ(root->Lookup("foo", &foo), ZX_OK);
  ASSERT_EQ(foo->Open(&foo), ZX_OK);
  auto close = fit::defer([foo]() { ASSERT_EQ(foo->Close(), ZX_OK); });

  // Read the file.
  std::vector<uint8_t> buf(kMinfsBlockSize + 10);
  size_t read;
  ASSERT_EQ(foo->Read(buf.data(), buf.size(), 0, &read), ZX_OK);
  ASSERT_EQ(buf.size(), read);

  // And now check the file.
  for (uint8_t c : buf) {
    EXPECT_EQ(kFill, c);
  }
}

TEST(TruncateSameSizeTest, DoesNotCommitTransaction) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);

  constexpr uint64_t kBlockCount = 1 << 15;
  auto device = std::make_unique<block_client::FakeBlockDevice>(kBlockCount, kMinfsBlockSize);
  auto* device_ptr = device.get();
  auto bcache_or = Bcache::Create(std::move(device), kBlockCount);
  ASSERT_TRUE(bcache_or.is_ok());
  ASSERT_TRUE(Mkfs(bcache_or.value().get()).is_ok());
  auto fs_or = Runner::Create(loop.dispatcher(), *std::move(bcache_or), MountOptions{});
  ASSERT_TRUE(fs_or.is_ok());

  auto root = fs_or->minfs().VnodeGet(kMinfsRootIno);
  ASSERT_TRUE(root.is_ok());
  zx::result foo = root->Create("foo", fs::CreationType::kFile);
  ASSERT_TRUE(foo.is_ok()) << foo.status_string();
  auto close = fit::defer([foo]() { ASSERT_EQ(foo->Close(), ZX_OK); });

  auto sync_fs = [&]() {
    sync_completion_t completion;
    fs_or->minfs().Sync([&completion](zx_status_t status) {
      EXPECT_EQ(status, ZX_OK);
      sync_completion_signal(&completion);
    });
    ASSERT_EQ(sync_completion_wait(&completion, zx::duration::infinite().get()), ZX_OK);
  };

  ASSERT_NO_FATAL_FAILURE(sync_fs());
  device_ptr->ResetBlockCounts();

  // Truncating an already-empty file to 0 should not issue any block writes.
  ASSERT_EQ(foo->Truncate(0), ZX_OK);
  ASSERT_NO_FATAL_FAILURE(sync_fs());
  EXPECT_EQ(device_ptr->GetWriteBlockCount(), 0u);

  // Write some data, flush it to disk, and reset block counts.
  std::vector<uint8_t> buf(100, kFill);
  size_t written = 0;
  ASSERT_EQ(foo->Write(buf.data(), buf.size(), 0, &written), ZX_OK);
  ASSERT_EQ(written, buf.size());
  ASSERT_NO_FATAL_FAILURE(sync_fs());
  EXPECT_GT(device_ptr->GetWriteBlockCount(), 0u);
  device_ptr->ResetBlockCounts();

  // Truncating a non-empty file to its current size should not issue any block writes.
  ASSERT_EQ(foo->Truncate(buf.size()), ZX_OK);
  ASSERT_NO_FATAL_FAILURE(sync_fs());
  EXPECT_EQ(device_ptr->GetWriteBlockCount(), 0u);
}

}  // namespace
}  // namespace minfs
