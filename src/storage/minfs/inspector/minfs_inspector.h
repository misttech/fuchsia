// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_MINFS_INSPECTOR_MINFS_INSPECTOR_H_
#define SRC_STORAGE_MINFS_INSPECTOR_MINFS_INSPECTOR_H_

#include <lib/zx/result.h>
#include <zircon/types.h>

#include <cstdint>
#include <memory>
#include <vector>

#include <storage/buffer/block_buffer.h>

#include "src/storage/lib/disk_inspector/buffer_factory.h"
#include "src/storage/lib/vfs/cpp/journal/format.h"
#include "src/storage/lib/vfs/cpp/transaction/transaction_handler.h"
#include "src/storage/minfs/format.h"

namespace minfs {

// Bare-bone minfs inspector that loads metadata from the backing block
// device and provides functions to return parsed structs.
// TODO(https://fxbug.dev/42124096): Since this can run on corrupt data, more thought needs
// to be put on the potential edge cases that can happen during corruption.
// Care needs to be put into what dependencies are used when exposing new
// information from this class.
class MinfsInspector {
 public:
  // Creates a MinfsInspector from a block device. Tries to load the
  // superblock from disk upon creation by calling ReloadSuperblock().
  static zx::result<std::unique_ptr<MinfsInspector>> Create(
      std::unique_ptr<fs::TransactionHandler> handler,
      std::unique_ptr<disk_inspector::BufferFactory> factory);

  // This function is used to initialize minfs metadata buffers and to load the relevant data.
  zx_status_t Initialize();

  // Initializes the |superblock_| buffer and tries to load the superblock
  // from disk into the buffer. The MinfsInspector should be considered invalid
  // and should not be used if this function fails as either VmoBuffers cannot
  // be created or we cannot read even the first block from the underlying
  // block device.
  zx_status_t ReloadSuperblock();

  // Initializes the |inode_bitmap_|, |inode_table_|, and |journal_| buffers
  // based on |superblock_| and tries to load the associated structs from disk
  // into these buffers. Note: we do not consider the failure of initializing
  // and loading of any of these buffers to be errors to crash the program as
  // the class should still work to a reasonable degree in the case of debugging
  // a superblock with corruptions. For cases of failure, these buffers have
  // undefined size and data inside. It is up to users to make sure that they
  // make valid calls using other functions in this class.
  void ReloadMetadataFromSuperblock();

  // Returns a copy of |superblock_|.
  Superblock InspectSuperblock();

  // Returns the number of inodes from |superblock_|.
  uint64_t GetInodeCount();

  // Returns the number of journal entires calculated from |superblock_|.
  uint64_t GetJournalEntryCount();

  // The following functions need to load data from disk, leading to the possibility of failed
  // loads. Since they need to return values, we have zx::results for all of the return types. In
  // addition, they all depend on the loaded |superblock_| value to get where to start indexing.

  // Loads the inode table blocks for which the inodes from |start_index| inclusive
  // to |end_index| exclusive from disk and returns the Inodes in the range as
  // a vector.
  zx::result<std::vector<Inode>> InspectInodeRange(uint64_t start_index, uint64_t end_index);

  // Loads the inode bitmap blocks for which the inode allocation bits for inodes
  // from |start_index| inclusive to |end_index| exclusive from disk and returns
  // the inode indices for which the corresponding bits are allocated.
  zx::result<std::vector<uint64_t>> InspectInodeAllocatedInRange(uint64_t start_index,
                                                                 uint64_t end_index);

  // Loads the first journal block
  zx::result<fs::JournalInfo> InspectJournalSuperblock();

  // Loads the |index| element journal entry block and returns it as a struct
  // of type T. Only supports casting to fs::JournalPrefix, fs::JournalHeaderBlock,
  // and fs::JournalCommitBlock.
  template <typename T>
  zx::result<T> InspectJournalEntryAs(uint64_t index);

  // Loads and returns the backup superblock.
  zx::result<Superblock> InspectBackupSuperblock();

  // Writes the |superblock| argument to disk and sets |superblock_| to |superblock|
  // if the write succeeds.
  zx::result<> WriteSuperblock(Superblock superblock);

 private:
  explicit MinfsInspector(std::unique_ptr<fs::TransactionHandler> handler,
                          std::unique_ptr<disk_inspector::BufferFactory> buffer_factory);

  zx_status_t LoadJournalEntry(storage::BlockBuffer* buffer, uint64_t index);

  std::unique_ptr<fs::TransactionHandler> handler_;
  std::unique_ptr<disk_inspector::BufferFactory> buffer_factory_;
  Superblock superblock_;
  // Scratch buffer initialized to be a single block in the Create method.
  // Functions that use this buffer should try to treat it as an initialized
  // buffer only valid for the duration of the function without any presaved
  // state or ability for the function to save state.
  std::unique_ptr<storage::BlockBuffer> buffer_;
};

}  // namespace minfs

#endif  // SRC_STORAGE_MINFS_INSPECTOR_MINFS_INSPECTOR_H_
