// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// This file contains a directory which contains blobs.

#ifndef SRC_STORAGE_BLOBFS_DIRECTORY_H_
#define SRC_STORAGE_BLOBFS_DIRECTORY_H_

#include <fidl/fuchsia.io/cpp/common_types.h>
#include <lib/zx/result.h>
#include <zircon/types.h>

#include <cstddef>
#include <string_view>

#include <fbl/ref_ptr.h>

#include "src/storage/lib/vfs/cpp/vfs.h"
#include "src/storage/lib/vfs/cpp/vfs_types.h"
#include "src/storage/lib/vfs/cpp/vnode.h"

namespace blobfs {

class Blobfs;

// The root directory of blobfs. This directory is a flat container of all blobs in the filesystem.
class Directory final : public fs::Vnode {
 public:
  explicit Directory(Blobfs* bs);
  ~Directory() final;

  // fs::Vnode interface.
  fuchsia_io::NodeProtocolKinds GetProtocols() const final;
  zx_status_t Readdir(fs::VdirCookie* cookie, void* dirents, size_t len, size_t* out_actual) final;
  zx_status_t Read(void* data, size_t len, size_t off, size_t* out_actual) final;
  zx_status_t Write(const void* data, size_t len, size_t offset, size_t* out_actual) final;
  zx_status_t Append(const void* data, size_t len, size_t* out_end, size_t* out_actual) final;
  zx_status_t Lookup(std::string_view name, fbl::RefPtr<fs::Vnode>* out) final;
  zx::result<fs::VnodeAttributes> GetAttributes() const final;
  zx::result<fbl::RefPtr<Vnode>> Create(std::string_view name, fs::CreationType type) final;
  zx_status_t Unlink(std::string_view name, bool must_be_dir) final;
  void Sync(SyncCallback closure) final;

 private:
  Blobfs* const blobfs_;
};

}  // namespace blobfs

#endif  // SRC_STORAGE_BLOBFS_DIRECTORY_H_
