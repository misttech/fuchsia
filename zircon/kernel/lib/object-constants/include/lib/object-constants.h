// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef ZIRCON_KERNEL_LIB_OBJECT_CONSTANTS_INCLUDE_LIB_OBJECT_CONSTANTS_H_
#define ZIRCON_KERNEL_LIB_OBJECT_CONSTANTS_INCLUDE_LIB_OBJECT_CONSTANTS_H_

#include <stddef.h>
#include <stdint.h>

// Size and alignment constants for Rust dispatcher states stored in C++ OpaqueStorage.
// These values must match the exact size and alignment of their corresponding Rust structs,
// which is enforced by static_asserts in both Rust and C++.

// Size and alignment for CounterDispatcherState.
constexpr size_t kCounterDispatcherStateSize = 64;
constexpr size_t kCounterDispatcherStateAlign = 8;
constexpr size_t kCounterDispatcherStateOffset = 48;

#endif  // ZIRCON_KERNEL_LIB_OBJECT_CONSTANTS_INCLUDE_LIB_OBJECT_CONSTANTS_H_
