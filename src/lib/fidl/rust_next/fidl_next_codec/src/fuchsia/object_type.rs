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

/// The wire type for [`zx::ObjectType`].
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct WireObjectType {
    inner: WireU32,
}

unsafe impl Wire for WireObjectType {
    type Decoded<'de> = Self;

    #[inline]
    fn zero_padding(out: &mut MaybeUninit<Self>) {
        munge!(let Self { inner } = out);
        WireU32::zero_padding(inner);
    }
}

impl WireObjectType {
    /// Returns an `ObjectType` with the same value as this wire type.
    pub fn to_object_type(self) -> zx::ObjectType {
        zx::ObjectType::from_raw(*self.inner)
    }
}

impl fmt::Debug for WireObjectType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_object_type().fmt(f)
    }
}

unsafe impl<D: ?Sized> Decode<D> for WireObjectType {
    fn decode(slot: Slot<'_, Self>, decoder: &mut D) -> Result<(), DecodeError> {
        munge!(let Self { inner } = slot);
        WireU32::decode(inner, decoder)
    }
}

impl Encodable for zx::ObjectType {
    type Encoded = WireObjectType;
}

unsafe impl<E: ?Sized> Encode<E> for zx::ObjectType {
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<Self::Encoded>,
    ) -> Result<(), EncodeError> {
        self.encode_ref(encoder, out)
    }
}

unsafe impl<E: ?Sized> EncodeRef<E> for zx::ObjectType {
    fn encode_ref(
        &self,
        encoder: &mut E,
        out: &mut MaybeUninit<Self::Encoded>,
    ) -> Result<(), EncodeError> {
        munge!(let WireObjectType { inner } = out);
        self.into_raw().encode(encoder, inner)
    }
}

impl FromWire<WireObjectType> for zx::ObjectType {
    fn from_wire(wire: WireObjectType) -> Self {
        Self::from_wire_ref(&wire)
    }
}

impl FromWireRef<WireObjectType> for zx::ObjectType {
    fn from_wire_ref(wire: &WireObjectType) -> Self {
        Self::from_raw(*wire.inner)
    }
}

impl IntoNatural for WireObjectType {
    type Natural = zx::ObjectType;
}
