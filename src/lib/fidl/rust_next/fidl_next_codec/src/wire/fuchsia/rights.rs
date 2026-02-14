// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::fmt;
use core::mem::MaybeUninit;

use munge::munge;

use crate::{
    Constrained, Decode, DecodeError, Encode, EncodeError, FromWire, FromWireRef, IntoNatural,
    Slot, ValidationError, Wire, wire,
};

/// The wire type for [`zx::Rights`].
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct Rights {
    inner: wire::Uint32,
}

impl Constrained for Rights {
    type Constraint = ();

    fn validate(_: Slot<'_, Self>, _: Self::Constraint) -> Result<(), ValidationError> {
        Ok(())
    }
}

unsafe impl Wire for Rights {
    type Narrowed<'de> = Self;

    #[inline]
    fn zero_padding(out: &mut MaybeUninit<Self>) {
        munge!(let Self { inner } = out);
        wire::Uint32::zero_padding(inner);
    }
}

impl Rights {
    /// Returns a `Rights` with the same value as this wire type.
    pub fn to_rights(self) -> zx::Rights {
        zx::Rights::from_bits_retain(*self.inner)
    }
}

impl fmt::Debug for Rights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_rights().fmt(f)
    }
}

unsafe impl<D: ?Sized> Decode<D> for Rights {
    fn decode(
        slot: Slot<'_, Self>,
        decoder: &mut D,
        _: Self::Constraint,
    ) -> Result<(), DecodeError> {
        munge!(let Self { inner } = slot);
        wire::Uint32::decode(inner, decoder, ())
    }
}

unsafe impl<E: ?Sized> Encode<Rights, E> for zx::Rights {
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<Rights>,
        constraint: (),
    ) -> Result<(), EncodeError> {
        munge!(let Rights { inner } = out);
        self.bits().encode(encoder, inner, constraint)
    }
}

unsafe impl<E: ?Sized> Encode<Rights, E> for &zx::Rights {
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<Rights>,
        constraint: (),
    ) -> Result<(), EncodeError> {
        Encode::encode(*self, encoder, out, constraint)
    }
}

impl FromWire<Rights> for zx::Rights {
    fn from_wire(wire: Rights) -> Self {
        Self::from_wire_ref(&wire)
    }
}

impl FromWireRef<Rights> for zx::Rights {
    fn from_wire_ref(wire: &Rights) -> Self {
        Self::from_bits_retain(*wire.inner)
    }
}

impl IntoNatural for Rights {
    type Natural = zx::Rights;
}
