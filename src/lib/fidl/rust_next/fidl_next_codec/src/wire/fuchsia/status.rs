// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::fmt;
use core::mem::MaybeUninit;

use munge::munge;

use crate::{
    Constrained, Decode, DecodeError, Encode, EncodeError, FromWire, FromWireRef, IntoNatural,
    Slot, ValidationError, Wire, wire,
};

/// The wire type for [`zx::Status`].
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct Status {
    inner: wire::Int32,
}

impl Constrained for Status {
    type Constraint = ();

    fn validate(_: Slot<'_, Self>, _: Self::Constraint) -> Result<(), ValidationError> {
        Ok(())
    }
}

unsafe impl Wire for Status {
    type Narrowed<'de> = Self;

    #[inline]
    fn zero_padding(out: &mut MaybeUninit<Self>) {
        munge!(let Self { inner } = out);
        wire::Int32::zero_padding(inner);
    }
}

impl Status {
    /// Returns a `Status` with the same value as this wire type.
    pub fn to_status(self) -> zx::Status {
        zx::Status::from_raw(*self.inner)
    }
}

impl fmt::Debug for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_status().fmt(f)
    }
}

unsafe impl<D: ?Sized> Decode<D> for Status {
    fn decode(
        slot: Slot<'_, Self>,
        decoder: &mut D,
        _: Self::Constraint,
    ) -> Result<(), DecodeError> {
        munge!(let Self { inner } = slot);
        wire::Int32::decode(inner, decoder, ())
    }
}

unsafe impl<E: ?Sized> Encode<Status, E> for zx::Status {
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<Status>,
        constraint: (),
    ) -> Result<(), EncodeError> {
        munge!(let Status { inner } = out);
        self.into_raw().encode(encoder, inner, constraint)
    }
}

unsafe impl<E: ?Sized> Encode<Status, E> for &zx::Status {
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<Status>,
        constraint: (),
    ) -> Result<(), EncodeError> {
        Encode::encode(*self, encoder, out, constraint)
    }
}

impl FromWire<Status> for zx::Status {
    fn from_wire(wire: Status) -> Self {
        Self::from_wire_ref(&wire)
    }
}

impl FromWireRef<Status> for zx::Status {
    fn from_wire_ref(wire: &Status) -> Self {
        Self::from_raw(*wire.inner)
    }
}

impl IntoNatural for Status {
    type Natural = zx::Status;
}
