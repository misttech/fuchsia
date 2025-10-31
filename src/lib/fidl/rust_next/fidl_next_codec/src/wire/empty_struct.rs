// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::mem::MaybeUninit;

use crate::{Unconstrained, Wire};

/// An empty struct's wire representation. C/C++ memory layout rules (and hence
/// FIDL wire rules) require every object to have a unique address so we have to
/// make a single, tiny type for empty structs.
#[repr(u8)]
#[derive(Clone, Copy)]
pub enum WireEmptyStructPlaceholder {
    /// Empty structs are represented as a single 0u8.
    Zero = 0,
}

unsafe impl Wire for WireEmptyStructPlaceholder {
    type Owned<'de> = Self;

    #[inline]
    fn zero_padding(_: &mut MaybeUninit<Self>) {}
}
impl Unconstrained for WireEmptyStructPlaceholder {}

impl core::fmt::Debug for WireEmptyStructPlaceholder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "(empty)")
    }
}
