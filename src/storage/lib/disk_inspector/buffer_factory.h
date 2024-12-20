// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_LIB_DISK_INSPECTOR_BUFFER_FACTORY_H_
#define SRC_STORAGE_LIB_DISK_INSPECTOR_BUFFER_FACTORY_H_

#include <lib/zx/result.h>

#include <cstddef>
#include <memory>

#include <storage/buffer/block_buffer.h>

namespace disk_inspector {

// Generic interface to dispense block buffers. Classes or functions that need
// to use block buffers and intend to be operating system agnostic should
// take in a |BufferFactory| to create generic BlockBuffers.
class BufferFactory {
 public:
  virtual ~BufferFactory() = default;

  // Creates a block buffer of size |capacity| to store in |out|.
  virtual zx::result<std::unique_ptr<storage::BlockBuffer>> CreateBuffer(size_t capacity) const = 0;
};

}  // namespace disk_inspector

#endif  // SRC_STORAGE_LIB_DISK_INSPECTOR_BUFFER_FACTORY_H_
