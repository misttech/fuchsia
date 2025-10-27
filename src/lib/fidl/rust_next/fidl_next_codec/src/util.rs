// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Helper types for encoding and decoding.

use core::hint::unreachable_unchecked;
use core::mem::MaybeUninit;

use crate::{Constrained, Encode, EncodeError, Unconstrained};

/// A type which cannot be constructed.
pub enum Never {}

impl Unconstrained for Never {}

unsafe impl<W: Constrained, E: ?Sized> Encode<W, E> for Never {
    fn encode(
        self,
        _: &mut E,
        _: &mut MaybeUninit<W>,
        _: W::Constraint,
    ) -> Result<(), EncodeError> {
        // SAFETY: `Never` cannot exist, so this code can never be reached.
        unsafe { unreachable_unchecked() }
    }
}
