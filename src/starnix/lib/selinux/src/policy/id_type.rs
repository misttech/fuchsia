// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fmt;
use std::marker::PhantomData;
use std::num::{NonZeroU8, NonZeroU16, NonZeroU32};

use fuchsia_rcu::RcuDroppable;
use zerocopy::{Immutable, IntoBytes};

use super::traits::PolicyId;

/// Type-safe wrapper around a primitive integer representing a policy identifier.
///
/// The `Tag` phantom parameter ensures that identifiers for different domain types
/// (e.g., [`super::types::TypeId`]) cannot be mixed up at compile time, even if they share
/// the same underlying integer representation.
#[derive(
    Copy, Clone, Debug, Hash, Eq, Ord, PartialEq, PartialOrd, RcuDroppable, IntoBytes, Immutable,
)]
#[repr(transparent)]
pub struct IdType<T, Tag> {
    value: T,
    _phantom: PhantomData<Tag>,
}

impl<T, Tag> IdType<T, Tag> {
    /// Constructs a new [`IdType`] wrapping the specified `id` value.
    pub(super) const fn new(id: T) -> Self {
        Self { value: id, _phantom: PhantomData }
    }
}

impl<Tag> IdType<NonZeroU16, Tag> {
    /// Constructs an [`IdType`] directly from a 16-bit identifier value.
    pub(super) fn from_u16(value: u16) -> Option<Self> {
        let value = NonZeroU16::new(value)?;
        Some(Self { value, _phantom: PhantomData })
    }

    /// Returns the wrapped 16-bit identifier value.
    pub(super) fn as_u16(&self) -> u16 {
        self.value.get()
    }
}

#[cfg(test)]
impl<T, Tag> IdType<T, Tag>
where
    Self: PolicyId,
{
    /// Helper to construct an ID from a raw `u32` in tests. Panics if the value is invalid.
    pub fn for_test(value: u32) -> Self {
        Self::from_u32(value).expect("valid test ID")
    }
}

// Implement PolicyId for 8-bit IDs (wrapped in NonZeroU8)
impl<Tag> PolicyId for IdType<NonZeroU8, Tag>
where
    Tag: Copy + Clone + std::fmt::Debug + Eq + std::hash::Hash,
{
    fn as_u32(&self) -> u32 {
        self.value.get() as u32
    }

    fn from_u32(value: u32) -> Option<Self> {
        let val_u8 = u8::try_from(value).ok()?;
        let value = NonZeroU8::new(val_u8)?;
        Some(Self { value, _phantom: PhantomData })
    }
}

// Implement PolicyId for 16-bit IDs (wrapped in NonZeroU16)
impl<Tag> PolicyId for IdType<NonZeroU16, Tag>
where
    Tag: Copy + Clone + std::fmt::Debug + Eq + std::hash::Hash,
{
    fn as_u32(&self) -> u32 {
        self.value.get() as u32
    }

    fn from_u32(value: u32) -> Option<Self> {
        let val_u16 = u16::try_from(value).ok()?;
        let value = NonZeroU16::new(val_u16)?;
        Some(Self { value, _phantom: PhantomData })
    }
}

// Implement PolicyId for 32-bit IDs (wrapped in NonZeroU32)
impl<Tag> PolicyId for IdType<NonZeroU32, Tag>
where
    Tag: Copy + Clone + std::fmt::Debug + Eq + std::hash::Hash,
{
    fn as_u32(&self) -> u32 {
        self.value.get()
    }

    fn from_u32(value: u32) -> Option<Self> {
        let value = NonZeroU32::new(value)?;
        Some(Self { value, _phantom: PhantomData })
    }
}

impl<T: fmt::Display, Tag> fmt::Display for IdType<T, Tag> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.value, f)
    }
}
