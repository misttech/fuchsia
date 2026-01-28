// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/zx/thread.h>
#include <lib/zx/vmo.h>
#include <zircon/syscalls-next.h>
#include <zircon/syscalls/rseq.h>

#include <zxtest/zxtest.h>

#include "zircon/system/utest/core/needs-next.h"

NEEDS_NEXT_SYSCALL(zx_thread_set_rseq);

namespace {

TEST(RseqTest, SetRseqNotSupported) {
  NEEDS_NEXT_SKIP(zx_thread_set_rseq);
  // For now, even with valid arguments, it returns ZX_ERR_NOT_SUPPORTED.
  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(sizeof(zx_rseq_t), 0, &vmo));

  ASSERT_EQ(zx_thread_set_rseq(vmo.get(), 0, sizeof(zx_rseq_t)), ZX_ERR_NOT_SUPPORTED);
}

}  // namespace
