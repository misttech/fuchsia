// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Provides encoding for FIDL types.

mod error;

use core::mem::MaybeUninit;
use core::ptr::copy_nonoverlapping;

pub use self::error::EncodeError;

use crate::{
    Constrained, CopyOptimization, Encoder, EncoderExt as _, WireBox, WireF32, WireF64, WireI16,
    WireI32, WireI64, WireU16, WireU32, WireU64,
};

/// Encodes a value.
///
/// # Safety
///
/// `encode` must initialize all non-padding bytes of `out`.
pub unsafe trait Encode<W: Constrained, E: ?Sized>: Sized {
    /// Whether the conversion from `Self` to `W` is equivalent to copying the
    /// raw bytes of `Self`.
    ///
    /// Copy optimization is disabled by default.
    const COPY_OPTIMIZATION: CopyOptimization<Self, W> = CopyOptimization::disable();

    /// Encodes this value into an encoder and output.
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<W>,
        constraint: W::Constraint,
    ) -> Result<(), EncodeError>;
}

/// Encodes an optional value.
///
/// # Safety
///
/// `encode_option` must initialize all non-padding bytes of `out`.
pub unsafe trait EncodeOption<W: Constrained, E: ?Sized>: Sized {
    /// Encodes this optional value into an encoder and output.
    fn encode_option(
        this: Option<Self>,
        encoder: &mut E,
        out: &mut MaybeUninit<W>,
        constraint: W::Constraint,
    ) -> Result<(), EncodeError>;
}

unsafe impl<W, E, T> Encode<W, E> for Box<T>
where
    W: Constrained,
    E: ?Sized,
    T: Encode<W, E>,
{
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<W>,
        constraint: <W as Constrained>::Constraint,
    ) -> Result<(), EncodeError> {
        T::encode(*self, encoder, out, constraint)
    }
}

unsafe impl<'a, W, E, T> Encode<W, E> for &'a Box<T>
where
    W: Constrained,
    E: ?Sized,
    &'a T: Encode<W, E>,
{
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<W>,
        constraint: <W as Constrained>::Constraint,
    ) -> Result<(), EncodeError> {
        <&'a T>::encode(self, encoder, out, constraint)
    }
}

unsafe impl<W, E, T> EncodeOption<W, E> for Box<T>
where
    W: Constrained,
    E: ?Sized,
    T: EncodeOption<W, E>,
{
    fn encode_option(
        this: Option<Self>,
        encoder: &mut E,
        out: &mut MaybeUninit<W>,
        constraint: <W as Constrained>::Constraint,
    ) -> Result<(), EncodeError> {
        T::encode_option(this.map(|value| *value), encoder, out, constraint)
    }
}

unsafe impl<'a, W, E, T> EncodeOption<W, E> for &'a Box<T>
where
    W: Constrained,
    E: ?Sized,
    &'a T: EncodeOption<W, E>,
{
    fn encode_option(
        this: Option<Self>,
        encoder: &mut E,
        out: &mut MaybeUninit<W>,
        constraint: <W as Constrained>::Constraint,
    ) -> Result<(), EncodeError> {
        <&'a T>::encode_option(this.map(|value| &**value), encoder, out, constraint)
    }
}

macro_rules! impl_primitive {
    ($ty:ty) => {
        impl_primitive!($ty, $ty);
    };
    ($ty:ty, $enc:ty) => {
        unsafe impl<E: ?Sized> Encode<$enc, E> for $ty {
            const COPY_OPTIMIZATION: CopyOptimization<$ty, $enc> =
                CopyOptimization::<$ty, $enc>::PRIMITIVE;

            #[inline]
            fn encode(
                self,
                encoder: &mut E,
                out: &mut MaybeUninit<$enc>,
                constraint: <$enc as Constrained>::Constraint,
            ) -> Result<(), EncodeError> {
                Encode::encode(&self, encoder, out, constraint)
            }
        }

        unsafe impl<'a, E: ?Sized> Encode<$enc, E> for &'a $ty {
            #[inline]
            fn encode(
                self,
                _: &mut E,
                out: &mut MaybeUninit<$enc>,
                _constraint: <$enc as Constrained>::Constraint,
            ) -> Result<(), EncodeError> {
                out.write(<$enc>::from(*self));
                Ok(())
            }
        }

        unsafe impl<E: Encoder + ?Sized> EncodeOption<WireBox<'static, $enc>, E> for $ty {
            #[inline]
            fn encode_option(
                this: Option<Self>,
                encoder: &mut E,
                out: &mut MaybeUninit<WireBox<'static, $enc>>,
                constraint: (),
            ) -> Result<(), EncodeError> {
                if let Some(value) = this {
                    encoder.encode_next(value, constraint)?;
                    WireBox::encode_present(out);
                } else {
                    WireBox::encode_absent(out);
                }

                Ok(())
            }
        }

        unsafe impl<E: Encoder + ?Sized> EncodeOption<WireBox<'static, $enc>, E> for &$ty {
            #[inline]
            fn encode_option(
                this: Option<Self>,
                encoder: &mut E,
                out: &mut MaybeUninit<WireBox<'static, $enc>>,
                constraint: (),
            ) -> Result<(), EncodeError> {
                <$ty>::encode_option(this.cloned(), encoder, out, constraint)
            }
        }
    };
}

macro_rules! impl_primitives {
    ($($ty:ty $(, $enc:ty)?);* $(;)?) => {
        $(
            impl_primitive!($ty $(, $enc)?);
        )*
    }
}

impl_primitives! {
    ();

    bool;

    i8;
    i16, WireI16; i32, WireI32; i64, WireI64;
    WireI16; WireI32; WireI64;

    u8;
    u16, WireU16; u32, WireU32; u64, WireU64;
    WireU16; WireU32; WireU64;

    f32, WireF32; f64, WireF64;
    WireF32; WireF64;
}

fn encode_to_array<A, W, E, T, const N: usize>(
    value: A,
    encoder: &mut E,
    out: &mut MaybeUninit<[W; N]>,
    constraint: <W as Constrained>::Constraint,
) -> Result<(), EncodeError>
where
    A: AsRef<[T]> + IntoIterator,
    A::Item: Encode<W, E>,
    W: Constrained,
    E: ?Sized,
    T: Encode<W, E>,
{
    if T::COPY_OPTIMIZATION.is_enabled() {
        // SAFETY: `T` has copy optimization enabled and so is safe to copy to the output.
        unsafe {
            copy_nonoverlapping(value.as_ref().as_ptr().cast(), out.as_mut_ptr(), 1);
        }
    } else {
        for (i, item) in value.into_iter().enumerate() {
            // SAFETY: `out` is a `MaybeUninit<[T::Encoded; N]>` and so consists of `N` copies of
            // `T::Encoded` in order with no additional padding. We can make a `&mut MaybeUninit` to
            // the `i`th element by:
            // 1. Getting a pointer to the contents of the `MaybeUninit<[T::Encoded; N]>` (the
            //    pointer is of type `*mut [T::Encoded; N]`).
            // 2. Casting it to `*mut MaybeUninit<T::Encoded>`. Note that `MaybeUninit<T>` always
            //    has the same layout as `T`.
            // 3. Adding `i` to reach the `i`th element.
            // 4. Dereferencing as `&mut`.
            let out_i = unsafe { &mut *out.as_mut_ptr().cast::<MaybeUninit<W>>().add(i) };
            item.encode(encoder, out_i, constraint)?;
        }
    }
    Ok(())
}

unsafe impl<W, E, T, const N: usize> Encode<[W; N], E> for [T; N]
where
    W: Constrained,
    E: ?Sized,
    T: Encode<W, E>,
{
    const COPY_OPTIMIZATION: CopyOptimization<Self, [W; N]> = T::COPY_OPTIMIZATION.infer_array();

    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<[W; N]>,
        constraint: <W as Constrained>::Constraint,
    ) -> Result<(), EncodeError> {
        encode_to_array(self, encoder, out, constraint)
    }
}

unsafe impl<'a, W, E, T, const N: usize> Encode<[W; N], E> for &'a [T; N]
where
    W: Constrained,
    E: ?Sized,
    T: Encode<W, E>,
    &'a T: Encode<W, E>,
{
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<[W; N]>,
        constraint: <W as Constrained>::Constraint,
    ) -> Result<(), EncodeError> {
        encode_to_array(self, encoder, out, constraint)
    }
}

unsafe impl<W, E, T> Encode<W, E> for Option<T>
where
    W: Constrained,
    E: ?Sized,
    T: EncodeOption<W, E>,
{
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<W>,
        constraint: <W as Constrained>::Constraint,
    ) -> Result<(), EncodeError> {
        T::encode_option(self, encoder, out, constraint)
    }
}

unsafe impl<'a, W, E, T> Encode<W, E> for &'a Option<T>
where
    W: Constrained,
    E: ?Sized,
    Option<&'a T>: Encode<W, E>,
{
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<W>,
        constraint: <W as Constrained>::Constraint,
    ) -> Result<(), EncodeError> {
        self.as_ref().encode(encoder, out, constraint)
    }
}

#[cfg(test)]
mod tests {
    use crate::chunks;
    use crate::testing::{assert_encoded, assert_encoded_with_constraint};

    #[test]
    fn encode_unit() {
        assert_encoded((), &chunks![]);
    }

    #[test]
    fn encode_bool() {
        assert_encoded(true, &chunks![0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_encoded(false, &chunks![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn encode_ints() {
        assert_encoded(0xa3u8, &chunks![0xa3, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_encoded(-0x45i8, &chunks![0xbb, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);

        assert_encoded(0x1234u16, &chunks![0x34, 0x12, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_encoded(-0x1234i16, &chunks![0xcc, 0xed, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);

        assert_encoded(0x12345678u32, &chunks![0x78, 0x56, 0x34, 0x12, 0x00, 0x00, 0x00, 0x00]);
        assert_encoded(-0x12345678i32, &chunks![0x88, 0xa9, 0xcb, 0xed, 0x00, 0x00, 0x00, 0x00]);

        assert_encoded(
            0x123456789abcdef0u64,
            &chunks![0xf0, 0xde, 0xbc, 0x9a, 0x78, 0x56, 0x34, 0x12],
        );
        assert_encoded(
            -0x123456789abcdef0i64,
            &chunks![0x10, 0x21, 0x43, 0x65, 0x87, 0xa9, 0xcb, 0xed],
        );
    }

    #[test]
    fn encode_floats() {
        assert_encoded(
            ::core::f32::consts::PI,
            &chunks![0xdb, 0x0f, 0x49, 0x40, 0x00, 0x00, 0x00, 0x00],
        );
        assert_encoded(
            ::core::f64::consts::PI,
            &chunks![0x18, 0x2d, 0x44, 0x54, 0xfb, 0x21, 0x09, 0x40],
        );
    }

    #[test]
    fn encode_box() {
        assert_encoded(None::<u64>, &chunks![0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert_encoded(
            Some(0x123456789abcdef0u64),
            &chunks![
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xf0, 0xde, 0xbc, 0x9a, 0x78, 0x56,
                0x34, 0x12,
            ],
        );
    }

    #[test]
    fn encode_vec() {
        assert_encoded_with_constraint::<crate::WireOptionalVector<'_, crate::WireU32>, _>(
            None::<Vec<u32>>,
            &chunks![
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00,
            ],
            (1000, ()),
        );
        assert_encoded_with_constraint::<crate::WireOptionalVector<'_, crate::WireU32>, _>(
            Some(vec![0x12345678u32, 0x9abcdef0u32]),
            &chunks![
                0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0x78, 0x56, 0x34, 0x12, 0xf0, 0xde, 0xbc, 0x9a,
            ],
            (1000, ()),
        );
        assert_encoded_with_constraint::<crate::WireOptionalVector<'_, crate::WireU32>, _>(
            Some(Vec::<u32>::new()),
            &chunks![
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff,
            ],
            (1000, ()),
        );
    }

    #[test]
    fn encode_string() {
        assert_encoded_with_constraint::<crate::WireOptionalString<'_>, _>(
            None::<String>,
            &chunks![
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00,
            ],
            1000,
        );
        assert_encoded_with_constraint::<crate::WireOptionalString<'_>, _>(
            Some("0123".to_string()),
            &chunks![
                0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0x30, 0x31, 0x32, 0x33, 0x00, 0x00, 0x00, 0x00,
            ],
            1000,
        );
        assert_encoded_with_constraint::<crate::WireOptionalString<'_>, _>(
            Some(String::new()),
            &chunks![
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff,
            ],
            1000,
        );
    }
}
