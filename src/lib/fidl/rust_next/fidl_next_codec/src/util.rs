// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Helper types for encoding and decoding.

use core::hint::unreachable_unchecked;
use core::marker::PhantomData;
use core::mem::MaybeUninit;

use crate::{Constrained, Encodable, Encode, EncodeError, Unconstrained, Wire};

/// A type which cannot be constructed.
pub enum Never {}

impl Unconstrained for Never {}

/// A type which cannot be constructed and encodes as a `W`.
pub struct EncodableNever<W> {
    _never: Never,
    _phantom: PhantomData<W>,
}

impl<W: Wire + Constrained> Encodable for EncodableNever<W> {
    type Encoded = W;
}

unsafe impl<E: ?Sized, W: Wire + Constrained> Encode<E> for EncodableNever<W> {
    fn encode(
        self,
        _: &mut E,
        _: &mut MaybeUninit<Self::Encoded>,
        _: W::Constraint,
    ) -> Result<(), EncodeError> {
        // SAFETY: `EncodableNever` cannot exist because it has a `Never` field.
        // Therefore, this code can never be reached.
        unsafe { unreachable_unchecked() }
    }
}
