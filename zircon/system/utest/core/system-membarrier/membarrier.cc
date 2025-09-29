// Copyright 2025 The Fuchsia Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zircon/syscalls-next.h>

#include <zxtest/zxtest.h>

#include "zircon/system/utest/core/needs-next.h"

NEEDS_NEXT_SYSCALL(zx_system_barrier);

namespace {

TEST(Membarrier, InvalidOptions) {
  NEEDS_NEXT_SKIP(zx_system_barrier);
  EXPECT_EQ(ZX_ERR_INVALID_ARGS, zx_system_barrier(0xFFFFFFFF));
}

TEST(Membarrier, DataMemory) {
  NEEDS_NEXT_SKIP(zx_system_barrier);
  ASSERT_EQ(ZX_OK, zx_system_barrier(ZX_SYSTEM_BARRIER_DATA_MEMORY));
}

TEST(Membarrier, InstructionStream) {
  NEEDS_NEXT_SKIP(zx_system_barrier);
  ASSERT_EQ(ZX_OK, zx_system_barrier(ZX_SYSTEM_BARRIER_INSTRUCTION_STREAM));
}

}  // namespace
