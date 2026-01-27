// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/pthread/pthread_create.h"

#include "../threads/thread.h"
#include "src/__support/common.h"

namespace LIBC_NAMESPACE_DECL {

LLVM_LIBC_FUNCTION(int, pthread_create,
                   (pthread_t* __restrict thread, const pthread_attr_t* __restrict attr,
                    __pthread_start_t func, void* arg)) {
  ThreadAttributes attrs;
  if (attr) {
    if (attr->_a_stackaddr) [[unlikely]] {
      return ENOTSUP;
    }
    attrs.stack = PageRoundedSize{attr->_a_stacksize};
    attrs.guard = PageRoundedSize{attr->_a_guardsize};
    attrs.detached = attr->_a_detach;
    if (attr->__name) {
      attrs.name = ZxName{attr->__name};
    }
  }
  return ThreadCreateAndStart<ToPthread, PthreadError>(
      thread, attrs.WithDefaultName("pthread_create:%p", func), ToThreadFunction(func), arg);
}

}  // namespace LIBC_NAMESPACE_DECL
