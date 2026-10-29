// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::fmt;
use core::mem::MaybeUninit;

use munge::munge;

use crate::{
    Constrained, Decode, DecodeError, Encode, EncodeError, FromWire, IntoNatural, Slot,
    ValidationError, Wire, wire,
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

// SAFETY:
// - Lifetime erasure: `Status` has no lifetimes, so `Narrowed` is `Self`.
// - Padding: `Status` is transparent over `Int32`, which has no padding.
unsafe impl Wire for Status {
    type Narrowed<'de> = Self;

    #[inline]
    fn zero_padding(out: &mut MaybeUninit<Self>) {
        munge!(let Self { inner } = out);
        wire::Int32::zero_padding(inner);
    }
}

impl Status {
    /// Returns the raw status code.
    pub fn into_raw(self) -> i32 {
        *self.inner
    }

    /// Returns a `zx::Status` with the same value as this wire type.
    pub fn to_status(self) -> zx::Status {
        zx::Status::try_from_raw(*self.inner).unwrap()
    }
}

impl From<zx::Status> for Status {
    fn from(value: zx::Status) -> Self {
        Self { inner: wire::Int32(value.into_raw()) }
    }
}

impl fmt::Debug for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_status().fmt(f)
    }
}

// SAFETY: `decode` delegates to `Int32::decode`, which initializes the underlying `Int32`
// and ensures `slot` contains a valid decoded `Status`.
unsafe impl<D: ?Sized> Decode<D> for Status {
    fn decode(
        slot: Slot<'_, Self>,
        decoder: &mut D,
        _: Self::Constraint,
    ) -> Result<(), DecodeError> {
        munge!(let Self { mut inner } = slot);
        wire::Int32::decode(inner.as_mut(), decoder, ())?;
        if *inner == 0 {
            return Err(DecodeError::InvalidNonZeroInteger);
        }
        Ok(())
    }
}

// SAFETY: `encode` delegates to the `Encode` implementation of the raw `i32` status value,
// which initializes all non-padding bytes of `out`.
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

// SAFETY: `encode` delegates to `zx::Status`'s `Encode` implementation, which initializes
// all non-padding bytes of `out`.
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
        Self::from_wire(&wire)
    }
}

impl<'a> FromWire<&'a Status> for zx::Status {
    fn from_wire(wire: &'a Status) -> Self {
        wire.to_status()
    }
}

impl IntoNatural for Status {
    type Natural = zx::Status;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CHUNK_SIZE;

    #[test]
    fn test_status_decode_zero() {
        let mut buffer = [0u8; CHUNK_SIZE];
        // SAFETY: `buffer` is sufficiently sized and aligned for `Status`.
        let mut slot = unsafe { Slot::<Status>::new_unchecked(buffer.as_mut_ptr().cast()) };
        Status::decode(slot.as_mut(), &mut (), ()).expect_err("successfully decoded 0 as Status");
    }

    #[test]
    fn test_status_decode_error() {
        let mut buffer = [0u8; CHUNK_SIZE];
        let status_raw = zx::Status::NOT_SUPPORTED.into_raw();
        buffer[..4].copy_from_slice(&status_raw.to_le_bytes());

        // SAFETY: `buffer` is sufficiently sized and aligned for `Status`.
        let mut slot = unsafe { Slot::<Status>::new_unchecked(buffer.as_mut_ptr().cast()) };
        Status::decode(slot.as_mut(), &mut (), ()).expect("failed to decode error as Status");
        // SAFETY: `slot` was successfully decoded and initialized.
        let wire_result = unsafe { slot.as_ptr().cast::<Status>().read() };
        assert_eq!(wire_result.to_status(), zx::Status::NOT_SUPPORTED);
        assert_eq!(
            <zx::Status as FromWire<Status>>::from_wire(wire_result),
            zx::Status::NOT_SUPPORTED
        );
    }

    #[test]
    fn test_status_result_encode() {
        let mut out = MaybeUninit::<Status>::uninit();
        let status = zx::Status::NOT_FOUND;
        status.encode(&mut (), &mut out, ()).unwrap();
        // SAFETY: `encode` succeeded, so `out` is initialized.
        let encoded = unsafe { out.assume_init() };
        assert_eq!(encoded.into_raw(), zx::Status::NOT_FOUND.into_raw());
    }
}
