// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::fmt;
use core::marker::PhantomData;
use core::mem::{ManuallyDrop, MaybeUninit};

use fidl_next_codec::{
    Chunk, Constrained, Decode, DecodeError, Decoder, Encode, EncodeError, Encoder, FromWire,
    FromWireRef, IntoNatural, Slot, ValidationError, Wire, munge,
};

use crate::wire;

/// A flexible FIDL response.
#[repr(transparent)]
pub struct Flexible<'de, T> {
    raw: wire::Union,
    _phantom: PhantomData<(&'de mut [Chunk], T)>,
}

impl<T> Drop for Flexible<'_, T> {
    fn drop(&mut self) {
        match self.raw.ordinal() {
            ORD_OK => {
                let _ = unsafe { self.raw.get().read_unchecked::<T>() };
            }
            ORD_FRAMEWORK_ERR => {
                let _ = unsafe { self.raw.get().read_unchecked::<wire::FrameworkError>() };
            }
            _ => unsafe { ::core::hint::unreachable_unchecked() },
        }
    }
}

impl<T: Constrained<Constraint = ()>> Constrained for Flexible<'_, T> {
    type Constraint = ();

    fn validate(_: Slot<'_, Self>, _: Self::Constraint) -> Result<(), ValidationError> {
        Ok(())
    }
}

unsafe impl<T> Wire for Flexible<'static, T>
where
    T: Wire<Constraint = ()>,
{
    type Narrowed<'de> = Flexible<'de, T::Narrowed<'de>>;

    #[inline]
    fn zero_padding(out: &mut MaybeUninit<Self>) {
        munge!(let Self { raw, _phantom: _ } = out);
        wire::Union::zero_padding(raw);
    }
}

const ORD_OK: u64 = 1;
const ORD_FRAMEWORK_ERR: u64 = 3;

impl<T> Flexible<'_, T> {
    /// Returns whether the flexible response is `Ok`.
    pub fn is_ok(&self) -> bool {
        self.raw.ordinal() == ORD_OK
    }

    /// Returns whether the flexible response is `FrameworkErr`.
    pub fn is_framework_err(&self) -> bool {
        self.raw.ordinal() == ORD_FRAMEWORK_ERR
    }

    /// Returns the `Ok` value of the response, if any.
    pub fn ok(&self) -> Option<&T> {
        self.is_ok().then(|| unsafe { self.raw.get().deref_unchecked() })
    }

    /// Returns the `FrameworkErr` value of the response, if any.
    pub fn framework_err(&self) -> Option<crate::FrameworkError> {
        self.is_framework_err()
            .then(|| unsafe { (*self.raw.get().deref_unchecked::<wire::FrameworkError>()).into() })
    }

    /// Returns the contained `Ok` value.
    ///
    /// Panics if the response was not `Ok`.
    pub fn unwrap(&self) -> &T {
        self.ok().unwrap()
    }

    /// Returns the contained `FrameworkErr` value.
    ///
    /// Panics if the response was not `FrameworkErr`.
    pub fn unwrap_framework_err(&self) -> crate::FrameworkError {
        self.framework_err().unwrap()
    }

    /// Returns a `Flexible` of a reference to the value or framework error.
    pub fn as_ref(&self) -> crate::Flexible<&T> {
        match self.raw.ordinal() {
            ORD_OK => unsafe { crate::Flexible::Ok(self.raw.get().deref_unchecked()) },
            ORD_FRAMEWORK_ERR => unsafe {
                crate::Flexible::FrameworkErr(
                    (*self.raw.get().deref_unchecked::<wire::FrameworkError>()).into(),
                )
            },
            _ => unsafe { ::core::hint::unreachable_unchecked() },
        }
    }

    /// Returns a `Result` of the `Ok` value and a potential `FrameworkError`.
    pub fn as_result(&self) -> Result<&T, crate::FrameworkError> {
        match self.raw.ordinal() {
            ORD_OK => unsafe { Ok(self.raw.get().deref_unchecked()) },
            ORD_FRAMEWORK_ERR => unsafe {
                Err((*self.raw.get().deref_unchecked::<wire::FrameworkError>()).into())
            },
            _ => unsafe { ::core::hint::unreachable_unchecked() },
        }
    }

    /// Returns a `Flexible` of an `Owned` value or framework error.
    pub fn to_flexible(self) -> crate::Flexible<T> {
        let this = ManuallyDrop::new(self);
        match this.raw.ordinal() {
            ORD_OK => unsafe { crate::Flexible::Ok(this.raw.get().read_unchecked()) },
            ORD_FRAMEWORK_ERR => unsafe {
                crate::Flexible::FrameworkErr(
                    this.raw.get().read_unchecked::<wire::FrameworkError>().into(),
                )
            },
            _ => unsafe { ::core::hint::unreachable_unchecked() },
        }
    }
}

impl<T: Clone> Clone for Flexible<'_, T> {
    fn clone(&self) -> Self {
        Self {
            raw: match self.raw.ordinal() {
                ORD_OK => unsafe { self.raw.clone_inline_unchecked::<T>() },
                ORD_FRAMEWORK_ERR => unsafe {
                    self.raw.clone_inline_unchecked::<wire::FrameworkError>()
                },
                _ => unsafe { ::core::hint::unreachable_unchecked() },
            },
            _phantom: PhantomData,
        }
    }
}

impl<T> fmt::Debug for Flexible<'_, T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_ref().fmt(f)
    }
}

unsafe impl<'de, D, T> Decode<D> for Flexible<'de, T>
where
    D: Decoder<'de> + ?Sized,
    T: Decode<D, Constraint = ()>,
{
    fn decode(
        slot: Slot<'_, Self>,
        decoder: &mut D,
        constraint: Self::Constraint,
    ) -> Result<(), DecodeError> {
        munge!(let Self { mut raw, _phantom: _ } = slot);

        match wire::Union::encoded_ordinal(raw.as_mut()) {
            ORD_OK => wire::Union::decode_as::<D, T>(raw, decoder, constraint)?,
            ORD_FRAMEWORK_ERR => {
                wire::Union::decode_as::<D, wire::FrameworkError>(raw, decoder, ())?
            }
            ord => return Err(DecodeError::InvalidUnionOrdinal(ord as usize)),
        }

        Ok(())
    }
}

unsafe impl<E, WT, T> Encode<Flexible<'static, WT>, E> for crate::Flexible<T>
where
    E: Encoder + ?Sized,
    WT: Wire<Constraint = ()>,
    T: Encode<WT, E>,
{
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<Flexible<'static, WT>>,
        constraint: WT::Constraint,
    ) -> Result<(), EncodeError> {
        munge!(let Flexible { raw, _phantom: _ } = out);

        match self {
            Self::Ok(value) => {
                wire::Union::encode_as::<E, WT>(value, ORD_OK, encoder, raw, constraint)?
            }
            Self::FrameworkErr(error) => wire::Union::encode_as::<E, wire::FrameworkError>(
                error,
                ORD_FRAMEWORK_ERR,
                encoder,
                raw,
                (),
            )?,
        }

        Ok(())
    }
}

unsafe impl<'a, E, WT, T> Encode<Flexible<'static, WT>, E> for &'a crate::Flexible<T>
where
    E: Encoder + ?Sized,
    WT: Wire<Constraint = ()>,
    &'a T: Encode<WT, E>,
{
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<Flexible<'static, WT>>,
        constraint: WT::Constraint,
    ) -> Result<(), EncodeError> {
        self.as_ref().encode(encoder, out, constraint)
    }
}

impl<T, WT> FromWire<Flexible<'_, WT>> for crate::Flexible<T>
where
    T: FromWire<WT>,
{
    fn from_wire(wire: Flexible<'_, WT>) -> Self {
        match wire.to_flexible() {
            crate::Flexible::Ok(value) => Self::Ok(T::from_wire(value)),
            crate::Flexible::FrameworkErr(framework_error) => Self::FrameworkErr(framework_error),
        }
    }
}

impl<T: IntoNatural> IntoNatural for Flexible<'_, T> {
    type Natural = crate::Flexible<T::Natural>;
}

impl<T, WT> FromWireRef<Flexible<'_, WT>> for crate::Flexible<T>
where
    T: FromWireRef<WT>,
{
    fn from_wire_ref(wire: &Flexible<'_, WT>) -> Self {
        match wire.as_ref() {
            crate::Flexible::Ok(value) => Self::Ok(T::from_wire_ref(value)),
            crate::Flexible::FrameworkErr(framework_error) => Self::FrameworkErr(framework_error),
        }
    }
}

#[cfg(test)]
mod tests {
    use fidl_next_codec::{DecoderExt as _, EncoderExt, chunks};

    use crate::wire;

    #[test]
    fn encode_flexible() {
        assert_eq!(
            Vec::encode(crate::Flexible::Ok(0x12345678)).unwrap(),
            chunks![
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x78, 0x56, 0x34, 0x12, 0x00, 0x00,
                0x01, 0x00,
            ],
        );
        assert_eq!(
            Vec::encode(crate::Flexible::<i32>::FrameworkErr(crate::FrameworkError::UnknownMethod))
                .unwrap(),
            chunks![
                0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFE, 0xFF, 0xFF, 0xFF, 0x00, 0x00,
                0x01, 0x00,
            ],
        );
    }

    #[test]
    fn decode_flexible() {
        assert_eq!(
            chunks![
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x78, 0x56, 0x34, 0x12, 0x00, 0x00,
                0x01, 0x00,
            ]
            .as_mut_slice()
            .decode::<wire::Flexible<'_, wire::Int32>>()
            .unwrap()
            .as_ref()
            .unwrap()
            .0,
            0x12345678,
        );
        assert_eq!(
            chunks![
                0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFE, 0xFF, 0xFF, 0xFF, 0x00, 0x00,
                0x01, 0x00,
            ]
            .as_mut_slice()
            .decode::<wire::Flexible<'_, wire::Int32>>()
            .unwrap()
            .as_ref()
            .unwrap_framework_err(),
            crate::FrameworkError::UnknownMethod,
        );
    }
}
