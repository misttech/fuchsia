// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_C_PTHREAD_ATTR_H_
#define LIB_C_PTHREAD_ATTR_H_

#include "../threads/thread.h"

namespace LIBC_NAMESPACE_DECL {

constexpr pthread_attr_t ToPthreadAttr(ThreadAttributes attr) {
  return {
      ._a_stacksize = attr.stack.get(),
      ._a_guardsize = attr.guard.get(),
      ._a_detach = attr.detached,
  };
}

constexpr ThreadAttributes FromPthreadAttr(const pthread_attr_t* attr) {
  ThreadAttributes attrs;
  if (attr) {
    attrs.stack = PageRoundedSize{attr->_a_stacksize};
    attrs.guard = PageRoundedSize{attr->_a_guardsize};
    attrs.detached = attr->_a_detach;
    if (attr->__name) {
      attrs.name = ZxName{attr->__name};
    }
  }
  return attrs;
}

}  // namespace LIBC_NAMESPACE_DECL

#endif  // LIB_C_PTHREAD_ATTR_H_
