// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::fmt;
use core::mem::MaybeUninit;

use fidl_next_codec::{
    Decode, DecodeError, Encode, EncodeError, FromWire, FromWireRef, IntoNatural, Slot,
    Unconstrained, Wire, WireI32, munge,
};

use crate::concurrency::hint::unreachable_unchecked;

/// An internal framework error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum FrameworkError {
    /// The protocol method was not recognized by the receiver.
    UnknownMethod = -2,
}

/// An internal framework error.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct WireFrameworkError {
    inner: WireI32,
}

unsafe impl Wire for WireFrameworkError {
    type Decoded<'de> = Self;

    #[inline]
    fn zero_padding(_: &mut MaybeUninit<Self>) {}
}

impl Unconstrained for WireFrameworkError {}

impl fmt::Debug for WireFrameworkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        FrameworkError::from(*self).fmt(f)
    }
}

impl From<WireFrameworkError> for FrameworkError {
    fn from(value: WireFrameworkError) -> Self {
        match *value.inner {
            -2 => Self::UnknownMethod,
            _ => unsafe { unreachable_unchecked() },
        }
    }
}

unsafe impl<D: ?Sized> Decode<D> for WireFrameworkError {
    fn decode(slot: Slot<'_, Self>, _: &mut D, _: ()) -> Result<(), DecodeError> {
        munge!(let Self { inner } = slot);
        match **inner {
            -2 => Ok(()),
            code => Err(DecodeError::InvalidFrameworkError(code)),
        }
    }
}

unsafe impl<E: ?Sized> Encode<WireFrameworkError, E> for FrameworkError {
    fn encode(
        self,
        _: &mut E,
        out: &mut MaybeUninit<WireFrameworkError>,
        _: (),
    ) -> Result<(), EncodeError> {
        munge!(let WireFrameworkError { inner } = out);
        inner.write(WireI32(match self {
            FrameworkError::UnknownMethod => -2,
        }));

        Ok(())
    }
}

unsafe impl<E: ?Sized> Encode<WireFrameworkError, E> for &FrameworkError {
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<WireFrameworkError>,
        constraint: (),
    ) -> Result<(), EncodeError> {
        Encode::encode(*self, encoder, out, constraint)
    }
}

impl FromWire<WireFrameworkError> for FrameworkError {
    #[inline]
    fn from_wire(wire: WireFrameworkError) -> Self {
        Self::from_wire_ref(&wire)
    }
}

impl IntoNatural for WireFrameworkError {
    type Natural = FrameworkError;
}

impl FromWireRef<WireFrameworkError> for FrameworkError {
    #[inline]
    fn from_wire_ref(wire: &WireFrameworkError) -> Self {
        Self::from(*wire)
    }
}
