// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <errno.h>
#include <malloc.h>
#include <zircon/compiler.h>

// TODO(https://fxbug.dev/489136393): This file will be removed after a
// toolchain roll brings in sanitizer runtimes that provide reallocarray.

__EXPORT [[gnu::weak]] void* reallocarray(void* ptr, size_t nmemb, size_t size) {
  size_t total_size;
  if (mul_overflow(nmemb, size, &total_size)) [[unlikely]] {
    errno = ENOMEM;
    return nullptr;
  }
  return realloc(ptr, total_size);
}
