// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_LIB_PAVER_CONFIG_H_
#define SRC_STORAGE_LIB_PAVER_CONFIG_H_

#include <vector>

namespace paver {

enum class Arch {
  kX64,
  kArm64,
  kRiscv64,
};

// Get the architecture of the currently running platform.
inline constexpr Arch GetCurrentArch() {
#if defined(__x86_64__)
  return Arch::kX64;
#elif defined(__aarch64__)
  return Arch::kArm64;
#elif defined(__riscv)
  return Arch::kRiscv64;
#else
#error "Unknown arch"
#endif
}

struct PaverConfig {
  // The architecture of the system the paver is running on.
  Arch arch = GetCurrentArch();
  // In GptPartitioner, when matching multiple GPT-formatted devices, we pick the target GPT using
  // the existence of the Fuchsia system partition.  The system partition is detected based on
  // matching any of these labels.
  std::vector<const char*> system_partition_names = {"super", "fvm"};
};

}  // namespace paver

#endif  // SRC_STORAGE_LIB_PAVER_CONFIG_H_
