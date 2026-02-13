// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/pthread/pthread_getattr_np.h"

#include "src/__support/common.h"
#include "threads_impl.h"

namespace LIBC_NAMESPACE_DECL {

LLVM_LIBC_FUNCTION(int, pthread_getattr_np, (pthread_t th, pthread_attr_t* attr)) {
  __pthread* t = reinterpret_cast<__pthread*>(th);
  *attr = {
      ._a_stacksize = t->safe_stack.iov_len,
      ._a_stackaddr = t->safe_stack.iov_base,
      ._a_detach =
          t->zxr_thread.state.load(std::memory_order_acquire) == zxr_thread_t::State::DETACHED,
  };
  return 0;
}

}  // namespace LIBC_NAMESPACE_DECL
