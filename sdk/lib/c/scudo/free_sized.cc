// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <malloc.h>
#include <zircon/compiler.h>

// TODO(https://fxbug.dev/495371178): This file will be removed once sanitizers
// provide built-in support for these functions.

__EXPORT __WEAK void free_sized(void* ptr, size_t size) { free(ptr); }

__EXPORT __WEAK void free_aligned_sized(void* ptr, size_t alignment, size_t size) { free(ptr); }
