// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::PolicyValidationContext;
use super::parser::{PolicyCursor, PolicyData, PolicyOffset};
use crate::policy::{Counted, Parse, Validate};
use std::marker::PhantomData;
use zerocopy::FromBytes;

/// A trait for types that have metadata.
///
/// Many policy objects have a fixed-sized metadata section that is much faster to parse than the
/// full object. This trait is used when walking the binary policy to find objects of interest
/// efficiently.
pub trait HasMetadata {
    /// The Rust type that represents the metadata.
    type Metadata: FromBytes + Sized;
}

/// A trait for types that can be walked through the policy data.
///
/// This trait is used when walking the binary policy to find objects of interest efficiently.
pub trait Walk {
    /// Walks the policy data to the next object of the given type.
    ///
    /// Returns an error if the cursor cannot be walked to the next object of the given type.
    fn walk(policy_data: &PolicyData, offset: PolicyOffset) -> PolicyOffset;
}

/// A view into a policy object.
///
/// This struct contains the start and end offsets of the object in the policy data. To read the
/// object, use [`View::read`].
#[derive(Debug, Clone, Copy)]
pub struct View<T> {
    phantom: PhantomData<T>,

    /// The start offset of the object in the policy data.
    start: PolicyOffset,

    /// The end offset of the object in the policy data.
    end: PolicyOffset,
}

impl<T> View<T> {
    /// Creates a new view from the start and end offsets.
    pub fn new(start: PolicyOffset, end: PolicyOffset) -> Self {
        Self { phantom: PhantomData, start, end }
    }
}

impl<T: Sized> View<T> {
    /// Creates a new view at the given start offset.
    ///
    /// The end offset is calculated as the start offset plus the size of the object.
    pub fn at(start: PolicyOffset) -> Self {
        let end = start + std::mem::size_of::<T>() as u32;
        Self::new(start, end)
    }
}

impl<T: FromBytes + Sized> View<T> {
    /// Reads the object from the policy data.
    ///
    /// This function requires the object to have a fixed size and simply copies the object from
    /// the policy data.
    ///
    /// For variable-sized objects, use [`View::parse`] instead.
    pub fn read(&self, policy_data: &PolicyData) -> T {
        debug_assert_eq!(self.end - self.start, std::mem::size_of::<T>() as u32);
        let start = self.start as usize;
        let end = self.end as usize;
        T::read_from_bytes(&policy_data[start..end]).unwrap()
    }
}

impl<T: HasMetadata> View<T> {
    /// Returns a view into the metadata of the object.
    ///
    /// Assumes the metadata is at the start of the object.
    pub fn metadata(&self) -> View<T::Metadata> {
        View::<T::Metadata>::at(self.start)
    }

    /// Reads the metadata from the policy data.
    pub fn read_metadata(&self, policy_data: &PolicyData) -> T::Metadata {
        self.metadata().read(policy_data)
    }
}

impl<T: Parse> View<T> {
    /// Parses the object from the policy data.
    ///
    /// This function uses the [`Parse`] trait to parse the object from the policy data.
    ///
    /// If the object has a fixed size, prefer [`View::read`] instead.
    pub fn parse(&self, policy_data: &PolicyData) -> T {
        let cursor = PolicyCursor::new_at(policy_data.clone(), self.start);
        let (object, _) =
            T::parse(cursor).map_err(Into::<anyhow::Error>::into).expect("policy should be valid");
        object
    }
}

impl<T: Validate + Parse> Validate for View<T> {
    type Error = anyhow::Error;

    fn validate(&self, context: &mut PolicyValidationContext) -> Result<(), Self::Error> {
        let object = self.parse(&context.data);
        object.validate(context).map_err(Into::<anyhow::Error>::into)
    }
}

/// A view into the data of an array of objects.
///
/// This struct contains the start offset of the array and the number of objects in the array.
/// To iterate over the objects, use [`ArrayDataView::iter`].
#[derive(Debug, Clone, Copy)]
pub struct ArrayDataView<D> {
    phantom: PhantomData<D>,
    start: PolicyOffset,
    count: u32,
}

impl<D> ArrayDataView<D> {
    /// Creates a new array data view from the start offset and count.
    pub fn new(start: PolicyOffset, count: u32) -> Self {
        Self { phantom: PhantomData, start, count }
    }

    /// Iterates over the objects in the array.
    ///
    /// The iterator returns views into the objects in the array.
    ///
    /// This function requires the policy data to be provided to the iterator because objects in
    /// the array may have variable size.
    pub fn iter(self, policy_data: &PolicyData) -> ArrayDataViewIter<D> {
        ArrayDataViewIter::new(policy_data.clone(), self.start, self.count)
    }
}

/// An iterator over the objects in an array.
///
/// This struct contains the cursor to the start of the array and the number of objects remaining
/// to be iterated over.
pub struct ArrayDataViewIter<D> {
    phantom: PhantomData<D>,
    policy_data: PolicyData,
    offset: PolicyOffset,
    remaining: u32,
}

impl<T> ArrayDataViewIter<T> {
    /// Creates a new array data view iterator from the start cursor and remaining count.
    pub(crate) fn new(policy_data: PolicyData, offset: PolicyOffset, remaining: u32) -> Self {
        Self { phantom: PhantomData, policy_data, offset, remaining }
    }
}

impl<D: Walk> std::iter::Iterator for ArrayDataViewIter<D> {
    type Item = View<D>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining > 0 {
            let start = self.offset;
            self.offset = D::walk(&self.policy_data, start);
            self.remaining -= 1;
            Some(View::new(start, self.offset))
        } else {
            None
        }
    }
}

/// A view into the data of an array of objects.
///
/// This struct contains the start offset of the array and the number of objects in the array.
/// To access the objects in the array, use [`ArrayView::data`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct ArrayView<M, D> {
    phantom: PhantomData<(M, D)>,
    start: PolicyOffset,
    count: u32,
}

impl<M, D> ArrayView<M, D> {
    /// Creates a new array view from the start offset and count.
    pub fn new(start: PolicyOffset, count: u32) -> Self {
        Self { phantom: PhantomData, start, count }
    }
}

impl<M: Sized, D> ArrayView<M, D> {
    /// Returns a view into the metadata of the array.
    pub fn metadata(&self) -> View<M> {
        View::<M>::at(self.start)
    }

    /// Returns a view into the data of the array.
    pub fn data(&self) -> ArrayDataView<D> {
        ArrayDataView::new(self.metadata().end, self.count)
    }
}

fn parse_array_data<D: Parse>(
    cursor: PolicyCursor,
    count: u32,
) -> Result<PolicyCursor, anyhow::Error> {
    let mut tail = cursor;
    for _ in 0..count {
        let (_, next) = D::parse(tail).map_err(Into::<anyhow::Error>::into)?;
        tail = next;
    }
    Ok(tail)
}

impl<M: Counted + Parse + Sized, D: Parse> Parse for ArrayView<M, D> {
    /// [`ArrayView`] abstracts over two types (`M` and `D`) that may have different [`Parse::Error`]
    /// types. Unify error return type via [`anyhow::Error`].
    type Error = anyhow::Error;

    fn parse(cursor: PolicyCursor) -> Result<(Self, PolicyCursor), Self::Error> {
        let start = cursor.offset();
        let (metadata, cursor) = M::parse(cursor).map_err(Into::<anyhow::Error>::into)?;
        let count = metadata.count();
        let cursor = parse_array_data::<D>(cursor, count)?;
        Ok((Self::new(start, count), cursor))
    }
}
