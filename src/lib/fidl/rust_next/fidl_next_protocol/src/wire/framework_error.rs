// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::fmt;
use core::mem::MaybeUninit;

use fidl_next_codec::{
    Constrained, Decode, DecodeError, Encode, EncodeError, FromWire, FromWireRef, IntoNatural,
    Slot, ValidationError, Wire, munge,
};

use crate::concurrency::hint::unreachable_unchecked;
use crate::wire;

/// An internal framework error.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct FrameworkError {
    inner: wire::Int32,
}

impl Constrained for FrameworkError {
    type Constraint = ();

    fn validate(_: Slot<'_, Self>, _: Self::Constraint) -> Result<(), ValidationError> {
        Ok(())
    }
}

unsafe impl Wire for FrameworkError {
    type Narrowed<'de> = Self;

    #[inline]
    fn zero_padding(_: &mut MaybeUninit<Self>) {}
}

impl fmt::Debug for FrameworkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        crate::FrameworkError::from(*self).fmt(f)
    }
}

impl From<FrameworkError> for crate::FrameworkError {
    fn from(value: FrameworkError) -> Self {
        match *value.inner {
            -2 => Self::UnknownMethod,
            _ => unsafe { unreachable_unchecked() },
        }
    }
}

unsafe impl<D: ?Sized> Decode<D> for FrameworkError {
    fn decode(slot: Slot<'_, Self>, _: &mut D, _: ()) -> Result<(), DecodeError> {
        munge!(let Self { inner } = slot);
        match **inner {
            -2 => Ok(()),
            code => Err(DecodeError::InvalidFrameworkError(code)),
        }
    }
}

unsafe impl<E: ?Sized> Encode<FrameworkError, E> for crate::FrameworkError {
    fn encode(
        self,
        _: &mut E,
        out: &mut MaybeUninit<FrameworkError>,
        _: (),
    ) -> Result<(), EncodeError> {
        munge!(let FrameworkError { inner } = out);
        inner.write(wire::Int32(match self {
            crate::FrameworkError::UnknownMethod => -2,
        }));

        Ok(())
    }
}

unsafe impl<E: ?Sized> Encode<FrameworkError, E> for &crate::FrameworkError {
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<FrameworkError>,
        constraint: (),
    ) -> Result<(), EncodeError> {
        Encode::encode(*self, encoder, out, constraint)
    }
}

impl FromWire<FrameworkError> for crate::FrameworkError {
    #[inline]
    fn from_wire(wire: FrameworkError) -> Self {
        Self::from_wire_ref(&wire)
    }
}

impl IntoNatural for FrameworkError {
    type Natural = crate::FrameworkError;
}

impl FromWireRef<FrameworkError> for crate::FrameworkError {
    #[inline]
    fn from_wire_ref(wire: &FrameworkError) -> Self {
        Self::from(*wire)
    }
}
