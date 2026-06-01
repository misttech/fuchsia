// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::opaque::Opaque;

/// A generic, safe opaque storage container with exact const size constraints.
///
/// This integrates with `Opaque<T>` to guarantee the Rust compiler knows the underlying memory
/// is interior-mutable (via `UnsafeCell`) and potentially uninitialized (via `MaybeUninit`).
pub type OpaqueBytes<const SIZE: usize> = Opaque<[u8; SIZE]>;
