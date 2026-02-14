// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zircon/compiler.h>

#include <cstdint>

#include "threads_impl.h"

// The compiler supports __builtin_* names that just call these.

extern "C" __EXPORT void* __get_unsafe_stack_start(void) {
#if HAVE_UNSAFE_STACK
  return __pthread_self()->unsafe_stack.iov_base;
#endif
  return nullptr;
}

extern "C" __EXPORT void* __get_unsafe_stack_top(void) {
#if HAVE_UNSAFE_STACK
  const struct iovec* stack = &__pthread_self()->unsafe_stack;
  return reinterpret_cast<std::byte*>(stack->iov_base) + stack->iov_len;
#endif
  return nullptr;
}

extern "C" __EXPORT void* __get_unsafe_stack_ptr(void) {
#if HAVE_UNSAFE_STACK
  return reinterpret_cast<void*>(__pthread_self()->abi.unsafe_sp);
#endif
  return nullptr;
}
