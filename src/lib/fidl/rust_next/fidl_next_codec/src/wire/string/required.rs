// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::fmt;
use core::mem::MaybeUninit;
use core::ops::Deref;
use core::str::{from_utf8, from_utf8_unchecked};

use munge::munge;

use crate::{
    Constrained, Decode, DecodeError, Decoder, Encode, EncodeError, Encoder, FromWire, FromWireRef,
    IntoNatural, Slot, ValidationError, Wire, WireVector,
};

/// A FIDL string
#[repr(transparent)]
pub struct WireString<'de> {
    vec: WireVector<'de, u8>,
}

unsafe impl Wire for WireString<'static> {
    type Owned<'de> = WireString<'de>;

    #[inline]
    fn zero_padding(out: &mut MaybeUninit<Self>) {
        munge!(let Self { vec } = out);
        WireVector::<u8>::zero_padding(vec);
    }
}

impl WireString<'_> {
    /// Encodes that a string is present in a slot.
    #[inline]
    pub fn encode_present(out: &mut MaybeUninit<Self>, len: u64) {
        munge!(let Self { vec } = out);
        WireVector::encode_present(vec, len);
    }

    /// Returns the length of the string in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.vec.len()
    }

    /// Returns whether the string is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns a reference to the underlying `str`.
    #[inline]
    pub fn as_str(&self) -> &str {
        unsafe { from_utf8_unchecked(self.vec.as_slice()) }
    }

    /// Validate that this string's length falls within the limit.
    fn validate_max_len(slot: Slot<'_, Self>, limit: u64) -> Result<(), crate::ValidationError> {
        munge!(let Self { vec } = slot);
        match WireVector::validate_max_len(vec, limit) {
            Ok(()) => Ok(()),
            Err(ValidationError::VectorTooLong { count, limit }) => {
                Err(ValidationError::StringTooLong { count, limit })
            }
            Err(e) => Err(e),
        }
    }
}

impl Constrained for WireString<'_> {
    type Constraint = u64;

    fn validate(slot: Slot<'_, Self>, constraint: u64) -> Result<(), ValidationError> {
        Self::validate_max_len(slot, constraint)
    }
}

impl Deref for WireString<'_> {
    type Target = str;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Debug for WireString<'_> {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(f)
    }
}

unsafe impl<D: Decoder + ?Sized> Decode<D> for WireString<'static> {
    #[inline]
    fn decode(slot: Slot<'_, Self>, decoder: &mut D, constraint: u64) -> Result<(), DecodeError> {
        munge!(let Self { mut vec } = slot);

        match unsafe { WireVector::decode_raw(vec.as_mut(), decoder, constraint) } {
            Ok(()) => (),
            Err(DecodeError::Validation(ValidationError::VectorTooLong { count, limit })) => {
                return Err(DecodeError::Validation(ValidationError::StringTooLong {
                    count,
                    limit,
                }));
            }
            Err(e) => {
                return Err(e);
            }
        };
        let vec = unsafe { vec.deref_unchecked() };

        // Check if the string is valid ASCII (fast path)
        if !vec.as_slice().is_ascii() {
            // Fall back to checking if the string is valid UTF-8 (slow path)
            // We're using `from_utf8` more like an `is_utf8` here.
            let _ = from_utf8(vec.as_slice())?;
        }

        Ok(())
    }
}

unsafe impl<E: Encoder + ?Sized> Encode<WireString<'static>, E> for String {
    #[inline]
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<WireString<'static>>,
        constraint: u64,
    ) -> Result<(), EncodeError> {
        self.as_str().encode(encoder, out, constraint)
    }
}

unsafe impl<E: Encoder + ?Sized> Encode<WireString<'static>, E> for &String {
    #[inline]
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<WireString<'static>>,
        constraint: u64,
    ) -> Result<(), EncodeError> {
        self.as_str().encode(encoder, out, constraint)
    }
}

unsafe impl<E: Encoder + ?Sized> Encode<WireString<'static>, E> for &str {
    #[inline]
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<WireString<'static>>,
        _constraint: u64,
    ) -> Result<(), EncodeError> {
        encoder.write(self.as_bytes());
        WireString::encode_present(out, self.len() as u64);
        Ok(())
    }
}

impl FromWire<WireString<'_>> for String {
    #[inline]
    fn from_wire(wire: WireString<'_>) -> Self {
        String::from_wire_ref(&wire)
    }
}

impl IntoNatural for WireString<'_> {
    type Natural = String;
}

impl FromWireRef<WireString<'_>> for String {
    #[inline]
    fn from_wire_ref(wire: &WireString<'_>) -> Self {
        unsafe { String::from_utf8_unchecked(Vec::from_wire_ref(&wire.vec)) }
    }
}
