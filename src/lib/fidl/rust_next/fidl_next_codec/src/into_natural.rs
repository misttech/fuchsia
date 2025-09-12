// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{FromWire, WireF32, WireF64, WireI16, WireI32, WireI64, WireU16, WireU32, WireU64};

/// Associates a good default type for a wire type to convert into.
pub trait IntoNatural: Sized {
    /// A good default type for this wire type to convert into.
    type Natural: FromWire<Self>;

    /// Converts this type into its natural equivalent.
    fn into_natural(self) -> Self::Natural {
        Self::Natural::from_wire(self)
    }
}

macro_rules! impl_primitive {
    ($ty:ty) => {
        impl_primitive!($ty, $ty);
    };
    ($wire:ty, $natural:ty) => {
        impl IntoNatural for $wire {
            type Natural = $natural;
        }
    };
}

macro_rules! impl_primitives {
    ($($wire:ty $(, $natural:ty)?);* $(;)?) => {
        $(
            impl_primitive!($wire $(, $natural)?);
        )*
    }
}

impl_primitives! {
    ();

    bool;

    i8;
    WireI16, i16;
    WireI32, i32;
    WireI64, i64;

    u8;
    WireU16, u16;
    WireU32, u32;
    WireU64, u64;

    WireF32, f32;
    WireF64, f64;
}

impl<W: IntoNatural, const N: usize> IntoNatural for [W; N] {
    type Natural = [W::Natural; N];
}
