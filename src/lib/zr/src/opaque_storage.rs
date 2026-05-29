// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::opaque::Opaque;
use core::mem::MaybeUninit;

/// Trait implemented by type selectors to force a specific memory alignment.
pub trait AlignmentForcer {
    type Type: Copy + Default;
}

pub struct AlignSelector<const ALIGN: usize>;

impl AlignmentForcer for AlignSelector<1> {
    type Type = u8;
}

impl AlignmentForcer for AlignSelector<2> {
    type Type = u16;
}

impl AlignmentForcer for AlignSelector<4> {
    type Type = u32;
}

impl AlignmentForcer for AlignSelector<8> {
    type Type = u64;
}

impl AlignmentForcer for AlignSelector<16> {
    type Type = u128;
}

/// Helper union overlaying the alignment type and raw byte storage,
/// ensuring exactly the requested SIZE and ALIGN properties.
#[repr(C)]
pub union OpaqueStorageBytes<const SIZE: usize, const ALIGN: usize>
where
    AlignSelector<ALIGN>: AlignmentForcer,
{
    _align: <AlignSelector<ALIGN> as AlignmentForcer>::Type,
    storage: [MaybeUninit<u8>; SIZE],
}

/// A generic, safe opaque storage container with exact const size and alignment constraints.
///
/// This integrates with `Opaque<T>` to guarantee the Rust compiler knows the underlying memory
/// is interior-mutable (via `UnsafeCell`) and potentially uninitialized (via `MaybeUninit`).
pub type OpaqueStorage<const SIZE: usize, const ALIGN: usize> =
    Opaque<OpaqueStorageBytes<SIZE, ALIGN>>;
