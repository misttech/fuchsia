// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::mem::MaybeUninit;

use fidl_next_codec::wire::WireI32;
use fidl_next_codec::{
    Constrained, Decode, DecodeError, Encode, EncodeError, Slot, ValidationError, Wire,
};

use zerocopy::IntoBytes;

/// A FIDL protocol epitaph.
#[derive(Clone, Copy, Debug, IntoBytes)]
#[repr(C)]
pub struct WireEpitaph {
    /// The error status.
    pub error: WireI32,
}

impl WireEpitaph {
    /// Returns a new epitaph with the given error.
    pub fn new(error: i32) -> Self {
        Self { error: WireI32(error) }
    }
}

impl Constrained for WireEpitaph {
    type Constraint = ();

    fn validate(_: Slot<'_, Self>, _: Self::Constraint) -> Result<(), ValidationError> {
        Ok(())
    }
}

unsafe impl Wire for WireEpitaph {
    type Narrowed<'de> = Self;

    #[inline]
    fn zero_padding(_: &mut MaybeUninit<Self>) {
        // Wire epitaphs have no padding
    }
}

unsafe impl<E: ?Sized> Encode<WireEpitaph, E> for WireEpitaph {
    #[inline]
    fn encode(
        self,
        _: &mut E,
        out: &mut MaybeUninit<WireEpitaph>,
        _: (),
    ) -> Result<(), EncodeError> {
        out.write(self);
        Ok(())
    }
}

unsafe impl<E: ?Sized> Encode<WireEpitaph, E> for &WireEpitaph {
    #[inline]
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<WireEpitaph>,
        constraint: (),
    ) -> Result<(), EncodeError> {
        Encode::encode(*self, encoder, out, constraint)
    }
}

unsafe impl<D: ?Sized> Decode<D> for WireEpitaph {
    #[inline]
    fn decode(_: Slot<'_, Self>, _: &mut D, _: ()) -> Result<(), DecodeError> {
        Ok(())
    }
}
