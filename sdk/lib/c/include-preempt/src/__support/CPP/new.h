// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef PREEMPT_SRC___SUPPORT_CPP_NEW_H_
#define PREEMPT_SRC___SUPPORT_CPP_NEW_H_

// This header has to exist basically because the adjacent tuple.h does.  Since
// that header uses <tuple>, it will also reach <new>.  But the llvm-libc
// "src/__support/CPP/new.h" conflicts with that.  Fortunately, as with tuple.h
// it's all just meant to be a namespace-renamed polyfill for what libc++
// already provides, _mostly_.  However, new.h also provides AllocChecker new!

#include <stdlib.h>

#include <new>

#include "src/__support/common.h"
#include "src/__support/macros/config.h"

namespace LIBC_NAMESPACE_DECL {

// TODO(mcgrathr): This matches llvm-libc's AllocChecker, which is not as
// useful as fbl::AllocChecker and should be cleaned up.  But that's orthogonal
// to why it's here.  It shouldn't be here because "src/__support/CPP/new.h"
// should be a pure (namespace-renamed) polyfill for <new> and the nonstandard
// AllocChecker APIs should be in a different header.  Until that is done
// upstream, it's just reimplemented here.
class AllocChecker {
 public:
  explicit operator bool() const { return success_; }

  static void* alloc(size_t s, AllocChecker& ac) { return ac.Checked(::malloc(s)); }

  static void* aligned_alloc(size_t s, std::align_val_t align, AllocChecker& ac) {
    return ac.Checked(::aligned_alloc(static_cast<size_t>(align), s));
  }

 private:
  void* Checked(void* ptr) {
    success_ = ptr != nullptr;
    return ptr;
  }

  bool success_ = false;
};

}  // namespace LIBC_NAMESPACE_DECL

inline void* operator new(size_t size, LIBC_NAMESPACE::AllocChecker& ac) noexcept {
  return LIBC_NAMESPACE::AllocChecker::alloc(size, ac);
}

inline void* operator new(size_t size, std::align_val_t align,
                          LIBC_NAMESPACE::AllocChecker& ac) noexcept {
  return LIBC_NAMESPACE::AllocChecker::aligned_alloc(size, align, ac);
}

inline void* operator new[](size_t size, LIBC_NAMESPACE::AllocChecker& ac) noexcept {
  return LIBC_NAMESPACE::AllocChecker::alloc(size, ac);
}

inline void* operator new[](size_t size, std::align_val_t align,
                            LIBC_NAMESPACE::AllocChecker& ac) noexcept {
  return LIBC_NAMESPACE::AllocChecker::aligned_alloc(size, align, ac);
}

#endif  // PREEMPT_SRC___SUPPORT_CPP_NEW_H_
