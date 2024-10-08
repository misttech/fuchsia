// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/lib/vfs/cpp/journal/inspector_journal.h"

#include <zircon/assert.h>

#include <cstddef>
#include <cstdint>
#include <memory>

#include "src/storage/lib/disk_inspector/common_types.h"
#include "src/storage/lib/disk_inspector/disk_inspector.h"
#include "src/storage/lib/vfs/cpp/journal/format.h"
#include "src/storage/lib/vfs/cpp/journal/inspector_journal_entries.h"

namespace fs {

void JournalObject::GetValue(const void** out_buffer, size_t* out_buffer_size) const {
  ZX_DEBUG_ASSERT_MSG(false, "Invalid GetValue call for non primitive data type.");
}

std::unique_ptr<disk_inspector::DiskObject> JournalObject::GetElementAt(uint32_t index) const {
  switch (index) {
    case 0: {
      // uint64_t magic.
      return std::make_unique<disk_inspector::DiskObjectUint64>("magic", &(journal_info_.magic));
    }
    case 1: {
      // uint64_t start_block
      return std::make_unique<disk_inspector::DiskObjectUint64>("start_block",
                                                                &(journal_info_.start_block));
    }
    case 2: {
      // uint64_t reserved
      return std::make_unique<disk_inspector::DiskObjectUint64>("reserved",
                                                                &(journal_info_.reserved));
    }
    case 3: {
      // uint64_t timestamp
      return std::make_unique<disk_inspector::DiskObjectUint64>("timestamp",
                                                                &(journal_info_.timestamp));
    }
    case 4: {
      // uint64_t checksum
      return std::make_unique<disk_inspector::DiskObjectUint32>("checksum",
                                                                &(journal_info_.checksum));
    }
    case 5: {
      return std::make_unique<JournalEntries>(start_block_ + fs::kJournalMetadataBlocks,
                                              length_ - fs::kJournalMetadataBlocks, read_block_);
    }
  }
  return nullptr;
}

}  // namespace fs
