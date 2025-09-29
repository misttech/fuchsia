// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::fmt;
use core::mem::MaybeUninit;

use munge::munge;

use crate::{
    Decode, DecodeError, Encodable, Encode, EncodeError, EncodeRef, FromWire, FromWireRef,
    IntoNatural, Slot, Wire, WireU32,
};

/// The wire type for [`zx::Rights`].
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct WireRights {
    inner: WireU32,
}

unsafe impl Wire for WireRights {
    type Decoded<'de> = Self;

    #[inline]
    fn zero_padding(out: &mut MaybeUninit<Self>) {
        munge!(let Self { inner } = out);
        WireU32::zero_padding(inner);
    }
}

impl WireRights {
    /// Returns a `Rights` with the same value as this wire type.
    pub fn to_rights(self) -> zx::Rights {
        zx::Rights::from_bits_retain(*self.inner)
    }
}

impl fmt::Debug for WireRights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_rights().fmt(f)
    }
}

unsafe impl<D: ?Sized> Decode<D> for WireRights {
    fn decode(slot: Slot<'_, Self>, decoder: &mut D) -> Result<(), DecodeError> {
        munge!(let Self { inner } = slot);
        WireU32::decode(inner, decoder)
    }
}

impl Encodable for zx::Rights {
    type Encoded = WireRights;
}

unsafe impl<E: ?Sized> Encode<E> for zx::Rights {
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<Self::Encoded>,
    ) -> Result<(), EncodeError> {
        self.encode_ref(encoder, out)
    }
}

unsafe impl<E: ?Sized> EncodeRef<E> for zx::Rights {
    fn encode_ref(
        &self,
        encoder: &mut E,
        out: &mut MaybeUninit<Self::Encoded>,
    ) -> Result<(), EncodeError> {
        munge!(let WireRights { inner } = out);
        self.bits().encode(encoder, inner)
    }
}

impl FromWire<WireRights> for zx::Rights {
    fn from_wire(wire: WireRights) -> Self {
        Self::from_wire_ref(&wire)
    }
}

impl FromWireRef<WireRights> for zx::Rights {
    fn from_wire_ref(wire: &WireRights) -> Self {
        Self::from_bits_retain(*wire.inner)
    }
}

impl IntoNatural for WireRights {
    type Natural = zx::Rights;
}
