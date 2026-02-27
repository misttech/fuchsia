// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_UFS_UFS_PDEV_H_
#define SRC_DEVICES_BLOCK_DRIVERS_UFS_UFS_PDEV_H_

#include <fidl/fuchsia.hardware.platform.device/cpp/wire.h>

#include "src/devices/block/drivers/ufs/ufs.h"

namespace ufs {

class UfsPdev final : public Ufs {
 public:
  using Ufs::Ufs;

 protected:
  zx::result<> InitResources() override;
  zx_status_t StopResources() override;
};

}  // namespace ufs

#endif  // SRC_DEVICES_BLOCK_DRIVERS_UFS_UFS_PDEV_H_
