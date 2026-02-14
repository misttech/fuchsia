// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::fmt;
use core::mem::{MaybeUninit, forget};
use core::ptr::NonNull;

use munge::munge;

use crate::{
    Constrained, Decode, DecodeError, Decoder, DecoderExt as _, FromWire, FromWireOption,
    FromWireOptionRef, FromWireRef, IntoNatural, Slot, ValidationError, Wire, wire,
};

/// A boxed (optional) FIDL value.
#[repr(C)]
pub struct Box<'de, T> {
    ptr: wire::Pointer<'de, T>,
}

// SAFETY: `WireBox` doesn't add any restrictions on sending across thread boundaries, and so is
// `Send` if `T` is `Send`.
unsafe impl<T: Send> Send for Box<'_, T> {}

// SAFETY: `WireBox` doesn't add any interior mutability, so it is `Sync` if `T` is `Sync`.
unsafe impl<T: Sync> Sync for Box<'_, T> {}

impl<T> Drop for Box<'_, T> {
    fn drop(&mut self) {
        if self.is_some() {
            unsafe {
                self.ptr.as_ptr().drop_in_place();
            }
        }
    }
}

unsafe impl<T: Wire> Wire for Box<'static, T> {
    type Narrowed<'de> = Box<'de, T::Narrowed<'de>>;

    #[inline]
    fn zero_padding(_: &mut MaybeUninit<Self>) {
        // Wire boxes have no padding
    }
}

impl<T> Box<'_, T> {
    /// Encodes that a value is present in an output.
    pub fn encode_present(out: &mut MaybeUninit<Self>) {
        munge!(let Self { ptr } = out);
        wire::Pointer::encode_present(ptr);
    }

    /// Encodes that a value is absent in a slot.
    pub fn encode_absent(out: &mut MaybeUninit<Self>) {
        munge!(let Self { ptr } = out);
        wire::Pointer::encode_absent(ptr);
    }

    /// Returns whether the value is present.
    pub fn is_some(&self) -> bool {
        !self.ptr.as_ptr().is_null()
    }

    /// Returns whether the value is absent.
    pub fn is_none(&self) -> bool {
        !self.is_some()
    }

    /// Returns a reference to the boxed value, if any.
    pub fn as_ref(&self) -> Option<&T> {
        NonNull::new(self.ptr.as_ptr()).map(|ptr| unsafe { ptr.as_ref() })
    }

    /// Returns an `Owned` of the boxed value, if any.
    pub fn into_option(self) -> Option<T> {
        let ptr = self.ptr.as_ptr();
        forget(self);
        if ptr.is_null() { None } else { unsafe { Some(ptr.read()) } }
    }
}

impl<T: fmt::Debug> fmt::Debug for Box<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_ref().fmt(f)
    }
}

unsafe impl<'de, D: Decoder<'de> + ?Sized, T: Decode<D>> Decode<D> for Box<'de, T> {
    fn decode(
        slot: Slot<'_, Self>,
        decoder: &mut D,
        constraint: Self::Constraint,
    ) -> Result<(), DecodeError> {
        munge!(let Self { mut ptr } = slot);

        if wire::Pointer::is_encoded_present(ptr.as_mut())? {
            let mut value = decoder.take_slot::<T>()?;
            T::decode(value.as_mut(), decoder, constraint)?;
            wire::Pointer::set_decoded(ptr, value);
        }

        Ok(())
    }
}

impl<T: FromWire<W>, W> FromWireOption<Box<'_, W>> for T {
    fn from_wire_option(wire: Box<'_, W>) -> Option<Self> {
        wire.into_option().map(T::from_wire)
    }
}

impl<T: IntoNatural> IntoNatural for Box<'_, T> {
    type Natural = Option<T::Natural>;
}

impl<T: FromWireRef<W>, W> FromWireOptionRef<Box<'_, W>> for T {
    fn from_wire_option_ref(wire: &Box<'_, W>) -> Option<Self> {
        wire.as_ref().map(T::from_wire_ref)
    }
}

impl<T: Constrained> Constrained for Box<'_, T> {
    type Constraint = T::Constraint;

    fn validate(slot: Slot<'_, Self>, constraint: Self::Constraint) -> Result<(), ValidationError> {
        munge!(let Self { ptr } = slot);

        let ptr = unsafe { ptr.deref_unchecked() };
        let ptr = ptr.as_ptr();
        let member_slot = unsafe { Slot::new_unchecked(ptr) };
        T::validate(member_slot, constraint)
    }
}

#[cfg(test)]
mod tests {
    use crate::{DecoderExt as _, EncoderExt as _, chunks, wire};

    #[test]
    fn decode_box() {
        assert_eq!(
            chunks![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                .as_mut_slice()
                .decode::<wire::Box<'_, wire::Uint64>>()
                .unwrap()
                .as_ref(),
            None,
        );
        assert_eq!(
            chunks![
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xf0, 0xde, 0xbc, 0x9a, 0x78, 0x56,
                0x34, 0x12,
            ]
            .as_mut_slice()
            .decode::<wire::Box<'_, wire::Uint64>>()
            .unwrap()
            .as_ref(),
            Some(&wire::Uint64(0x123456789abcdef0u64)),
        );
    }

    #[test]
    fn encode_box() {
        assert_eq!(
            Vec::encode(None::<u64>).unwrap(),
            chunks![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        );
        assert_eq!(
            Vec::encode(Some(0x123456789abcdef0u64)).unwrap(),
            chunks![
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xf0, 0xde, 0xbc, 0x9a, 0x78, 0x56,
                0x34, 0x12,
            ],
        );
    }
}
