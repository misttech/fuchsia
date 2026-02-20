// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_LIB_ESCHER_FS_FUCHSIA_DATA_SOURCE_H_
#define SRC_UI_LIB_ESCHER_FS_FUCHSIA_DATA_SOURCE_H_

#include <fidl/fuchsia.io/cpp/fidl.h>
#include <lib/fit/function.h>
#include <lib/vfs/cpp/pseudo_dir.h>

#include <memory>
#include <optional>

#include "src/ui/lib/escher/fs/hack_filesystem.h"

namespace escher {

// Implementation of HackFilesystem which reads data from the Fuchsia component's local filesystem.
class FuchsiaDataSource : public HackFilesystem {
 public:
  FuchsiaDataSource(const std::shared_ptr<vfs::PseudoDir>& root_dir);
  FuchsiaDataSource();

  // |HackFilesystem|
  bool InitializeWithRealFiles(const std::vector<HackFilePath>& paths, const char* root) override;
  bool InitializeWithRealFilesInDir(const std::vector<HackFilePath>& paths,
                                    fidl::ClientEnd<fuchsia_io::Directory> client) override;

  const std::optional<fidl::SyncClient<fuchsia_io::Directory>>& base_dir() const override {
    return base_dir_;
  }

 private:
  static bool LoadFileAtDir(HackFilesystem* fs, const HackFilePath& path);
  bool DoInitializeWithRealFiles(const std::vector<HackFilePath>& paths,
                                 fit::function<bool(const HackFilePath& path)> load);
  std::shared_ptr<vfs::PseudoDir> root_dir_;
  std::optional<fidl::SyncClient<fuchsia_io::Directory>> base_dir_;
};

}  // namespace escher

#endif  // SRC_UI_LIB_ESCHER_FS_FUCHSIA_DATA_SOURCE_H_
