// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::fmt;
use core::marker::PhantomData;
use core::mem::{ManuallyDrop, MaybeUninit};

use fidl_next_codec::wire::{RawWireUnion, WireResult};
use fidl_next_codec::{
    Chunk, Constrained, Decode, DecodeError, Decoder, Encode, EncodeError, Encoder, FromWire,
    FromWireRef, IntoNatural, Slot, ValidationError, Wire, munge,
};

use crate::wire::WireFrameworkError;
use crate::{FlexibleResult, FrameworkError};

/// A flexible FIDL result.
#[repr(transparent)]
pub struct WireFlexibleResult<'de, T, E> {
    raw: RawWireUnion,
    _phantom: PhantomData<(&'de mut [Chunk], T, E)>,
}

impl<T, E> Drop for WireFlexibleResult<'_, T, E> {
    fn drop(&mut self) {
        match self.raw.ordinal() {
            ORD_OK => {
                let _ = unsafe { self.raw.get().read_unchecked::<T>() };
            }
            ORD_ERR => {
                let _ = unsafe { self.raw.get().read_unchecked::<E>() };
            }
            ORD_FRAMEWORK_ERR => {
                let _ = unsafe { self.raw.get().read_unchecked::<WireFrameworkError>() };
            }
            _ => unsafe { ::core::hint::unreachable_unchecked() },
        }
    }
}

impl<T, E> Constrained for WireFlexibleResult<'_, T, E>
where
    T: Constrained<Constraint = ()>,
    E: Constrained<Constraint = ()>,
{
    type Constraint = ();

    fn validate(_: Slot<'_, Self>, _: Self::Constraint) -> Result<(), ValidationError> {
        Ok(())
    }
}

unsafe impl<T, E> Wire for WireFlexibleResult<'static, T, E>
where
    T: Wire<Constraint = ()>,
    E: Wire<Constraint = ()>,
{
    type Narrowed<'de> = WireFlexibleResult<'de, T::Narrowed<'de>, E::Narrowed<'de>>;

    #[inline]
    fn zero_padding(out: &mut MaybeUninit<Self>) {
        munge!(let Self { raw, _phantom: _ } = out);
        RawWireUnion::zero_padding(raw);
    }
}

const ORD_OK: u64 = 1;
const ORD_ERR: u64 = 2;
const ORD_FRAMEWORK_ERR: u64 = 3;

impl<'de, T, E> WireFlexibleResult<'de, T, E> {
    /// Returns whether the flexible result is `Ok`.
    pub fn is_ok(&self) -> bool {
        self.raw.ordinal() == ORD_OK
    }

    /// Returns whether the flexible result if `Err`.
    pub fn is_err(&self) -> bool {
        self.raw.ordinal() == ORD_ERR
    }

    /// Returns whether the flexible result is `FrameworkErr`.
    pub fn is_framework_err(&self) -> bool {
        self.raw.ordinal() == ORD_FRAMEWORK_ERR
    }

    /// Returns the `Ok` value of the result, if any.
    pub fn ok(&self) -> Option<&T> {
        self.is_ok().then(|| unsafe { self.raw.get().deref_unchecked() })
    }

    /// Returns the `Err` value of the result, if any.
    pub fn err(&self) -> Option<&E> {
        self.is_err().then(|| unsafe { self.raw.get().deref_unchecked() })
    }

    /// Returns the `FrameworkErr` value of the result, if any.
    pub fn framework_err(&self) -> Option<FrameworkError> {
        self.is_framework_err()
            .then(|| unsafe { (*self.raw.get().deref_unchecked::<WireFrameworkError>()).into() })
    }

    /// Returns the contained `Ok` value.
    ///
    /// Panics if the result was not `Ok`.
    pub fn unwrap(&self) -> &T {
        self.ok().unwrap()
    }

    /// Returns the contained `Err` value.
    ///
    /// Panics if the result was not `Err`.
    pub fn unwrap_err(&self) -> &E {
        self.err().unwrap()
    }

    /// Returns the contained `FrameworkErr` value.
    ///
    /// Panics if the result was not `FrameworkErr`.
    pub fn unwrap_framework_err(&self) -> FrameworkError {
        self.framework_err().unwrap()
    }

    /// Returns a `FlexibleResult` of a reference to the value or framework error.
    pub fn as_ref(&self) -> FlexibleResult<&T, &E> {
        match self.raw.ordinal() {
            ORD_OK => unsafe { FlexibleResult::Ok(self.raw.get().deref_unchecked()) },
            ORD_ERR => unsafe { FlexibleResult::Err(self.raw.get().deref_unchecked()) },
            ORD_FRAMEWORK_ERR => unsafe {
                FlexibleResult::FrameworkErr(
                    (*self.raw.get().deref_unchecked::<WireFrameworkError>()).into(),
                )
            },
            _ => unsafe { ::core::hint::unreachable_unchecked() },
        }
    }

    /// Returns a `Result` of the `Ok` value and a potential `FrameworkError`.
    pub fn as_response(&self) -> Result<&WireResult<'_, T, E>, FrameworkError> {
        match self.raw.ordinal() {
            ORD_OK | ORD_ERR => unsafe {
                Ok(&*(self as *const Self as *const WireResult<'_, T, E>))
            },
            ORD_FRAMEWORK_ERR => unsafe {
                Err((*self.raw.get().deref_unchecked::<WireFrameworkError>()).into())
            },
            _ => unsafe { ::core::hint::unreachable_unchecked() },
        }
    }

    /// Returns a nested `Result` of the `Ok` and `Err` values, and a potential `FrameworkError`.
    pub fn as_result(&self) -> Result<Result<&T, &E>, FrameworkError> {
        match self.raw.ordinal() {
            ORD_OK => unsafe { Ok(Ok(self.raw.get().deref_unchecked())) },
            ORD_ERR => unsafe { Ok(Err(self.raw.get().deref_unchecked())) },
            ORD_FRAMEWORK_ERR => unsafe {
                Err((*self.raw.get().deref_unchecked::<WireFrameworkError>()).into())
            },
            _ => unsafe { ::core::hint::unreachable_unchecked() },
        }
    }

    /// Returns a `FlexibleResult` of a value or framework error.
    pub fn to_flexible_result(self) -> FlexibleResult<T, E> {
        let this = ManuallyDrop::new(self);
        match this.raw.ordinal() {
            ORD_OK => unsafe { FlexibleResult::Ok(this.raw.get().read_unchecked()) },
            ORD_ERR => unsafe { FlexibleResult::Err(this.raw.get().read_unchecked()) },
            ORD_FRAMEWORK_ERR => unsafe {
                FlexibleResult::FrameworkErr(
                    this.raw.get().read_unchecked::<WireFrameworkError>().into(),
                )
            },
            _ => unsafe { ::core::hint::unreachable_unchecked() },
        }
    }
}

impl<T: Clone, E: Clone> Clone for WireFlexibleResult<'_, T, E> {
    fn clone(&self) -> Self {
        Self {
            raw: match self.raw.ordinal() {
                ORD_OK => unsafe { self.raw.clone_inline_unchecked::<T>() },
                ORD_ERR => unsafe { self.raw.clone_inline_unchecked::<E>() },
                ORD_FRAMEWORK_ERR => unsafe {
                    self.raw.clone_inline_unchecked::<WireFrameworkError>()
                },
                _ => unsafe { ::core::hint::unreachable_unchecked() },
            },
            _phantom: PhantomData,
        }
    }
}

impl<T, E> fmt::Debug for WireFlexibleResult<'_, T, E>
where
    T: fmt::Debug,
    E: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_ref().fmt(f)
    }
}

unsafe impl<'de, D, T, E> Decode<D> for WireFlexibleResult<'de, T, E>
where
    D: Decoder<'de> + ?Sized,
    T: Decode<D, Constraint = ()>,
    E: Decode<D, Constraint = ()>,
{
    fn decode(slot: Slot<'_, Self>, decoder: &mut D, _: ()) -> Result<(), DecodeError> {
        munge!(let Self { mut raw, _phantom: _ } = slot);

        match RawWireUnion::encoded_ordinal(raw.as_mut()) {
            ORD_OK => RawWireUnion::decode_as::<D, T>(raw, decoder, ())?,
            ORD_ERR => RawWireUnion::decode_as::<D, E>(raw, decoder, ())?,
            ORD_FRAMEWORK_ERR => {
                RawWireUnion::decode_as::<D, WireFrameworkError>(raw, decoder, ())?
            }
            ord => return Err(DecodeError::InvalidUnionOrdinal(ord as usize)),
        }

        Ok(())
    }
}

unsafe impl<Enc, WT, T, WE, E> Encode<WireFlexibleResult<'static, WT, WE>, Enc>
    for FlexibleResult<T, E>
where
    Enc: Encoder + ?Sized,
    WT: Wire<Constraint = ()>,
    T: Encode<WT, Enc>,
    WE: Wire<Constraint = ()>,
    E: Encode<WE, Enc>,
{
    fn encode(
        self,
        encoder: &mut Enc,
        out: &mut MaybeUninit<WireFlexibleResult<'static, WT, WE>>,
        _: (),
    ) -> Result<(), EncodeError> {
        munge!(let WireFlexibleResult { raw, _phantom: _ } = out);

        match self {
            Self::Ok(value) => RawWireUnion::encode_as::<Enc, WT>(value, ORD_OK, encoder, raw, ())?,
            Self::Err(error) => {
                RawWireUnion::encode_as::<Enc, WE>(error, ORD_ERR, encoder, raw, ())?
            }
            Self::FrameworkErr(error) => RawWireUnion::encode_as::<Enc, WireFrameworkError>(
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

unsafe impl<'a, Enc, WT, T, WE, E> Encode<WireFlexibleResult<'static, WT, WE>, Enc>
    for &'a FlexibleResult<T, E>
where
    Enc: Encoder + ?Sized,
    WT: Wire<Constraint = ()>,
    &'a T: Encode<WT, Enc>,
    WE: Wire<Constraint = ()>,
    &'a E: Encode<WE, Enc>,
{
    fn encode(
        self,
        encoder: &mut Enc,
        out: &mut MaybeUninit<WireFlexibleResult<'static, WT, WE>>,
        _: (),
    ) -> Result<(), EncodeError> {
        self.as_ref().encode(encoder, out, ())
    }
}

impl<T, WT, E, WE> FromWire<WireFlexibleResult<'_, WT, WE>> for FlexibleResult<T, E>
where
    T: FromWire<WT>,
    E: FromWire<WE>,
{
    fn from_wire(wire: WireFlexibleResult<'_, WT, WE>) -> Self {
        match wire.to_flexible_result() {
            FlexibleResult::Ok(value) => Self::Ok(T::from_wire(value)),
            FlexibleResult::Err(error) => Self::Err(E::from_wire(error)),
            FlexibleResult::FrameworkErr(framework_error) => Self::FrameworkErr(framework_error),
        }
    }
}

impl<T: IntoNatural, E: IntoNatural> IntoNatural for WireFlexibleResult<'_, T, E> {
    type Natural = FlexibleResult<T::Natural, E::Natural>;
}

impl<T, WT, E, WE> FromWireRef<WireFlexibleResult<'_, WT, WE>> for FlexibleResult<T, E>
where
    T: FromWireRef<WT>,
    E: FromWireRef<WE>,
{
    fn from_wire_ref(wire: &WireFlexibleResult<'_, WT, WE>) -> Self {
        match wire.as_ref() {
            FlexibleResult::Ok(value) => Self::Ok(T::from_wire_ref(value)),
            FlexibleResult::Err(error) => Self::Err(E::from_wire_ref(error)),
            FlexibleResult::FrameworkErr(framework_error) => Self::FrameworkErr(framework_error),
        }
    }
}

#[cfg(test)]
mod tests {
    use fidl_next_codec::wire::WireI32;
    use fidl_next_codec::{DecoderExt as _, EncoderExt as _, chunks};

    use super::{FlexibleResult, WireFlexibleResult};
    use crate::FrameworkError;

    #[test]
    fn encode_flexible_result() {
        assert_eq!(
            Vec::encode(FlexibleResult::<(), i32>::Ok(())).unwrap(),
            chunks![
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x01, 0x00,
            ],
        );
        assert_eq!(
            Vec::encode(FlexibleResult::<(), i32>::Err(0x12345678)).unwrap(),
            chunks![
                0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x78, 0x56, 0x34, 0x12, 0x00, 0x00,
                0x01, 0x00,
            ],
        );
        assert_eq!(
            Vec::encode(FlexibleResult::<(), i32>::FrameworkErr(FrameworkError::UnknownMethod))
                .unwrap(),
            chunks![
                0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFE, 0xFF, 0xFF, 0xFF, 0x00, 0x00,
                0x01, 0x00,
            ],
        );
    }

    #[test]
    fn decode_flexible_result() {
        assert_eq!(
            chunks![
                0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x01, 0x00,
            ]
            .as_mut_slice()
            .decode::<WireFlexibleResult<'_, (), WireI32>>()
            .unwrap()
            .as_ref(),
            FlexibleResult::<_, &WireI32>::Ok(&()),
        );
        assert_eq!(
            chunks![
                0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x78, 0x56, 0x34, 0x12, 0x00, 0x00,
                0x01, 0x00,
            ]
            .as_mut_slice()
            .decode::<WireFlexibleResult<'_, (), WireI32>>()
            .unwrap()
            .as_ref(),
            FlexibleResult::<&(), _>::Err(&WireI32(0x12345678)),
        );
        assert_eq!(
            chunks![
                0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFE, 0xFF, 0xFF, 0xFF, 0x00, 0x00,
                0x01, 0x00,
            ]
            .as_mut_slice()
            .decode::<WireFlexibleResult<'_, (), WireI32>>()
            .unwrap()
            .as_ref(),
            FlexibleResult::<&(), &WireI32>::FrameworkErr(FrameworkError::UnknownMethod),
        );
    }
}
