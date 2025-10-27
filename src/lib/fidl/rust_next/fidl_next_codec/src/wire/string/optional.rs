// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::fmt;
use core::mem::MaybeUninit;
use core::str::from_utf8;

use munge::munge;

use crate::{
    Constrained, Decode, DecodeError, Decoder, EncodeError, EncodeOption, Encoder, FromWireOption,
    FromWireOptionRef, IntoNatural, Slot, ValidationError, Wire, WireOptionalVector, WireString,
    WireVector,
};

/// An optional FIDL string
#[repr(transparent)]
pub struct WireOptionalString<'de> {
    vec: WireOptionalVector<'de, u8>,
}

unsafe impl Wire for WireOptionalString<'static> {
    type Decoded<'de> = WireOptionalString<'de>;

    #[inline]
    fn zero_padding(out: &mut MaybeUninit<Self>) {
        munge!(let Self { vec } = out);
        WireOptionalVector::<u8>::zero_padding(vec);
    }
}

impl WireOptionalString<'_> {
    /// Encodes that a string is present in a slot.
    #[inline]
    pub fn encode_present(out: &mut MaybeUninit<Self>, len: u64) {
        munge!(let Self { vec } = out);
        WireOptionalVector::encode_present(vec, len);
    }

    /// Encodes that a string is absent in a slot.
    #[inline]
    pub fn encode_absent(out: &mut MaybeUninit<Self>) {
        munge!(let Self { vec } = out);
        WireOptionalVector::encode_absent(vec);
    }

    /// Returns whether the optional string is present.
    #[inline]
    pub fn is_some(&self) -> bool {
        self.vec.is_some()
    }

    /// Returns whether the optional string is absent.
    #[inline]
    pub fn is_none(&self) -> bool {
        self.vec.is_none()
    }

    /// Returns a reference to the underlying string, if any.
    #[inline]
    pub fn as_ref(&self) -> Option<&WireString<'_>> {
        self.vec.as_ref().map(|vec| unsafe { &*(vec as *const WireVector<'_, u8>).cast() })
    }

    /// Validate that this string's length falls within the limit.
    fn validate_max_len(slot: Slot<'_, Self>, limit: u64) -> Result<(), crate::ValidationError> {
        munge!(let Self { vec } = slot);
        match WireOptionalVector::validate_max_len(vec, limit) {
            Ok(()) => Ok(()),
            Err(ValidationError::VectorTooLong { count, limit }) => {
                Err(ValidationError::StringTooLong { count, limit })
            }
            Err(e) => Err(e),
        }
    }
}

impl Constrained for WireOptionalString<'_> {
    type Constraint = u64;

    fn validate(slot: Slot<'_, Self>, constraint: Self::Constraint) -> Result<(), ValidationError> {
        Self::validate_max_len(slot, constraint)
    }
}

impl fmt::Debug for WireOptionalString<'_> {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_ref().fmt(f)
    }
}

unsafe impl<D: Decoder + ?Sized> Decode<D> for WireOptionalString<'static> {
    #[inline]
    fn decode(slot: Slot<'_, Self>, decoder: &mut D, constraint: u64) -> Result<(), DecodeError> {
        munge!(let Self { mut vec } = slot);

        let result = unsafe { WireOptionalVector::decode_raw(vec.as_mut(), decoder, constraint) };
        match result {
            Ok(()) => (),
            Err(DecodeError::Validation(ValidationError::VectorTooLong { count, limit })) => {
                return Err(DecodeError::Validation(ValidationError::StringTooLong {
                    count,
                    limit,
                }));
            }
            Err(e) => return Err(e),
        }
        let vec = unsafe { vec.deref_unchecked() };
        if let Some(bytes) = vec.as_ref() {
            // Check if the string is valid ASCII (fast path)
            if !bytes.as_slice().is_ascii() {
                // Fall back to checking if the string is valid UTF-8 (slow path)
                // We're using `from_utf8` more like an `is_utf8` here.
                let _ = from_utf8(bytes)?;
            }
        }

        Ok(())
    }
}

unsafe impl<E: Encoder + ?Sized> EncodeOption<WireOptionalString<'static>, E> for String {
    #[inline]
    fn encode_option(
        this: Option<Self>,
        encoder: &mut E,
        out: &mut MaybeUninit<WireOptionalString<'static>>,
        constraint: u64,
    ) -> Result<(), EncodeError> {
        <&str>::encode_option(this.as_deref(), encoder, out, constraint)
    }
}

unsafe impl<E: Encoder + ?Sized> EncodeOption<WireOptionalString<'static>, E> for &String {
    #[inline]
    fn encode_option(
        this: Option<Self>,
        encoder: &mut E,
        out: &mut MaybeUninit<WireOptionalString<'static>>,
        constraint: u64,
    ) -> Result<(), EncodeError> {
        <&str>::encode_option(this.map(String::as_str), encoder, out, constraint)
    }
}

unsafe impl<E: Encoder + ?Sized> EncodeOption<WireOptionalString<'static>, E> for &str {
    #[inline]
    fn encode_option(
        this: Option<Self>,
        encoder: &mut E,
        out: &mut MaybeUninit<WireOptionalString<'static>>,
        _constraint: u64,
    ) -> Result<(), EncodeError> {
        if let Some(string) = this {
            encoder.write(string.as_bytes());
            WireOptionalString::encode_present(out, string.len() as u64);
        } else {
            WireOptionalString::encode_absent(out);
        }
        Ok(())
    }
}

impl FromWireOption<WireOptionalString<'_>> for String {
    #[inline]
    fn from_wire_option(wire: WireOptionalString<'_>) -> Option<Self> {
        Vec::from_wire_option(wire.vec).map(|vec| unsafe { String::from_utf8_unchecked(vec) })
    }
}

impl IntoNatural for WireOptionalString<'_> {
    type Natural = Option<String>;
}

impl FromWireOptionRef<WireOptionalString<'_>> for String {
    #[inline]
    fn from_wire_option_ref(wire: &WireOptionalString<'_>) -> Option<Self> {
        Vec::from_wire_option_ref(&wire.vec).map(|vec| unsafe { String::from_utf8_unchecked(vec) })
    }
}
