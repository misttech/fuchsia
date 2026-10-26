// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/object-constants.h>

#include <ktl/type_traits.h>
#include <object/exceptionate.h>

// Verify that `Exceptionate` is standard-layout and has the exact size and alignment of its Rust
// `OpaqueStorage`, so `Exceptionate*` pointers (including `JobDispatcher::DebugExceptionate` base
// subobjects) directly address the initialized Rust `Exceptionate` instance at offset 0.
static_assert(std::is_standard_layout_v<Exceptionate>);
static_assert(sizeof(Exceptionate) == kExceptionateStorageSize);
static_assert(alignof(Exceptionate) == kExceptionateStorageAlign);
static_assert(sizeof(KernelHandle<ChannelDispatcher>) == sizeof(void*));
static_assert(alignof(KernelHandle<ChannelDispatcher>) == alignof(void*));
