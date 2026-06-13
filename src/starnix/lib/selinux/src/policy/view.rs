// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::arrays::SimpleArrayView;
use super::parser::{PolicyCursor, PolicyData, PolicyOffset};
use super::{Counted, Parse, PolicyValidationContext, Validate};

use hashbrown::hash_table::HashTable;
use rapidhash::RapidHasher;
use static_assertions::const_assert;
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned, little_endian as le};

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

    /// The start offset of the object in the policy data.
    fn start(&self) -> PolicyOffset {
        self.start
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
        let cursor = PolicyCursor::new_at(policy_data, self.start);
        let (object, _) =
            T::parse(cursor).map_err(Into::<anyhow::Error>::into).expect("policy should be valid");
        object
    }
}

impl<T: Validate + Parse> Validate for View<T> {
    type Error = anyhow::Error;

    fn validate(&self, context: &PolicyValidationContext) -> Result<(), Self::Error> {
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
    fn new(policy_data: PolicyData, offset: PolicyOffset, remaining: u32) -> Self {
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
pub(super) struct ArrayView<M, D> {
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

fn parse_array_data<'a, D: Parse>(
    cursor: PolicyCursor<'a>,
    count: u32,
) -> Result<PolicyCursor<'a>, anyhow::Error> {
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

    fn parse<'a>(cursor: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let start = cursor.offset();
        let (metadata, cursor) = M::parse(cursor).map_err(Into::<anyhow::Error>::into)?;
        let count = metadata.count();
        let cursor = parse_array_data::<D>(cursor, count)?;
        Ok((Self::new(start, count), cursor))
    }
}

/// A trait for types that can be used as keys in a hash table.
///
/// This trait is used by [`HashedBlocksView`] to store and retrieve values.
pub(super) trait Hashable {
    type Key: Parse + Hash + Eq;
    type Value: Parse + Walk;

    /// Returns a reference to the key.
    fn key(&self) -> &Self::Key;

    /// Returns a [`SimpleArrayView`] into the values.
    fn values(&self) -> &SimpleArrayView<Self::Value>;
}

/// Stores an mapping from a [`D::Key`] to a set of [`D::Value`]s
#[derive(Debug, Clone)]
pub(super) struct CustomKeyHashedView<D: Hashable> {
    /// Stores the offset to D::Key.
    index: HashTable<PolicyOffset>,
    _phantom: PhantomData<D>,
}

impl<D: Hashable + Parse> CustomKeyHashedView<D> {
    /// Returns an iterator over the entries with the specified `key` and parses and
    /// emits those values.
    pub(super) fn find_all(
        &self,
        query_key: D::Key,
        policy_data: &PolicyData,
    ) -> impl Iterator<Item = D::Value> {
        let key_offset = self.index.find(compute_hash(&query_key), |&key_offset| {
            let cursor = PolicyCursor::new_at(policy_data, key_offset);
            let (key, _) = D::Key::parse(cursor)
                .map_err(Into::<anyhow::Error>::into)
                .expect("policy should be valid");

            key == query_key
        });

        key_offset.into_iter().flat_map(move |&key_offset| {
            let cursor = PolicyCursor::new_at(policy_data, key_offset);
            let (entry, _) = D::parse(cursor)
                .map_err(Into::<anyhow::Error>::into)
                .expect("policy should be valid");

            entry.values().data().iter(policy_data).map(move |v| v.parse(policy_data))
        })
    }

    pub(super) fn iter<'a>(
        &'a self,
        policy_data: &'a PolicyData,
    ) -> impl Iterator<Item = Result<D, anyhow::Error>> + 'a {
        self.index.iter().map(move |&offset| {
            let cursor = PolicyCursor::new_at(policy_data, offset);
            let (entry, _) = D::parse(cursor).map_err(Into::<anyhow::Error>::into)?;
            Ok(entry)
        })
    }
}

fn compute_hash<V: Hash>(val: &V) -> u64 {
    let mut hasher = RapidHasher::default();
    val.hash(&mut hasher);
    hasher.finish()
}

impl<D: Hashable + Parse> Parse for CustomKeyHashedView<D> {
    type Error = anyhow::Error;

    /// Parses (D::Key, SimpleArrayView<D::Value>) entries and stores the keys into a CustomKeyHashedView.
    fn parse<'a>(cursor: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        // Parse the count of entries.
        let (metadata, cursor) = le::U32::parse(cursor).map_err(Into::<anyhow::Error>::into)?;
        let count = metadata.count();

        // The index will store [`count`] entries. Reserve the necessary capacity ahead to avoid resizing later on.
        let mut index = HashTable::with_capacity(count as usize);

        let mut key_offset = cursor.offset();
        let mut tail = cursor;
        for _ in 0..count {
            let (entry, next) = D::parse(tail).map_err(Into::<anyhow::Error>::into)?;
            tail = next;

            let key: &D::Key = entry.key();
            index.insert_unique(compute_hash(&key), key_offset, |&key_offset| {
                let policy_cursor = PolicyCursor::new_at(tail.data(), key_offset);
                let (key, _) = D::Key::parse(policy_cursor)
                    .map_err(Into::<anyhow::Error>::into)
                    .expect("policy should be valid");
                compute_hash::<D::Key>(&key)
            });
            key_offset = tail.offset();
        }

        Ok((Self { _phantom: PhantomData, index }, tail))
    }
}

impl<D: Hashable + Parse> Validate for CustomKeyHashedView<D>
where
    SimpleArrayView<D::Value>: Validate<Error = anyhow::Error>,
{
    type Error = anyhow::Error;

    fn validate(&self, context: &PolicyValidationContext) -> Result<(), Self::Error> {
        for key_offset in self.index.iter() {
            let cursor = PolicyCursor::new_at(&context.data, *key_offset);
            let (entry, _) = D::parse(cursor).map_err(Into::<anyhow::Error>::into)?;

            entry.values().validate(context)?;
        }
        Ok(())
    }
}

/// An iterator giving views of the objects in the underlying [`policy_data`] found from
/// a single entry of a `HashedArrayView`.
struct HashedArrayViewEntryIter<'a, D: HasMetadata> {
    policy_data: &'a PolicyData,
    limit: PolicyOffset,
    metadata: D::Metadata,
    offset: Option<PolicyOffset>,
}

/// An unsigned integer in the range [0, 0x1000000) stored in three unaligned bytes.
//
// TODO: https://fxbug.dev/479180246 - it would be better to get this type from a library
// somewhere. Probably not https://docs.rs/u24 (because of "The type has the same size,
// alignment, and memory layout as a little-endian encoded u32" in that implementation's
// specification). But maybe from somewhere else?
#[derive(Clone, Copy, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct U24 {
    low: u8,
    middle: u8,
    high: u8,
}

// U24's space-efficiency is its reason for existence.
const_assert!(std::mem::size_of::<U24>() == 3);
const_assert!(std::mem::align_of::<U24>() == 1);

impl U24 {
    /// The zero value.
    pub const ZERO: Self = Self { low: 0, middle: 0, high: 0 };
}

impl TryFrom<u32> for U24 {
    type Error = ();

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        if 0x1000000 <= value {
            Err(())
        } else {
            Ok(Self {
                low: (value & 0xff) as u8,
                middle: ((value >> 8) & 0xff) as u8,
                high: ((value >> 16) & 0xff) as u8,
            })
        }
    }
}

impl From<U24> for u32 {
    fn from(value: U24) -> u32 {
        ((value.high as u32) << 16) + ((value.middle as u32) << 8) + (value.low as u32)
    }
}

impl<'a, D: HasMetadata + Walk> Iterator for HashedArrayViewEntryIter<'a, D>
where
    D::Metadata: Eq,
{
    type Item = View<D>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(offset) = self.offset
            && offset < self.limit
        {
            let element = View::<D>::at(offset);
            let metadata = element.read_metadata(&self.policy_data);
            if metadata == self.metadata {
                self.offset = Some(D::walk(&self.policy_data, offset));
                Some(element)
            } else {
                self.offset = None;
                None
            }
        } else {
            None
        }
    }
}

/// A view into the data of an array of objects, with efficient lookup based on metadata hash.
///
/// This struct contains only a vector of offsets into the policy data, to allow efficient lookup
/// of vector elements with matching metadata.
#[derive(Debug, Clone)]
pub(super) struct HashedArrayView<D: HasMetadata> {
    phantom: PhantomData<D>,
    index: HashTable<U24>,
    /// The offset in the policy data at which the elements indexed by this [`HashedArrayView`]
    /// end. Iteration of elements by this [`HashedArrayView`] must not look for an element at
    /// or beyond this offset.
    limit: PolicyOffset,
}

impl<D: HasMetadata> HashedArrayView<D>
where
    D::Metadata: Hash,
{
    fn metadata_hash(metadata: &D::Metadata) -> u64 {
        let mut hasher = RapidHasher::default();
        metadata.hash(&mut hasher);
        hasher.finish()
    }
}

impl<D: Parse + HasMetadata + Walk> HashedArrayView<D>
where
    D::Metadata: Eq + PartialEq + Hash + Debug,
{
    /// Looks up the entry with the specified metadata `key`, and parses and returns the value.
    /// This method is only appropriate to call when expecting to find at most one entry for
    /// `key`; if there is a possibility of more than one element in the underlying
    /// `policy_data` being associated with `key`, call `find_all` instead.
    pub fn find(&self, key: D::Metadata, policy_data: &PolicyData) -> Option<D> {
        let key_hash = Self::metadata_hash(&key);
        let offset = self.index.find(key_hash, |&offset| {
            let element = View::<D>::at(u32::from(offset));
            key == element.read_metadata(policy_data)
        })?;
        let element = View::<D>::at(u32::from(*offset));
        Some(element.parse(policy_data))
    }

    /// Returns an iterator over the entries with the specified metadata `key` and parses and
    /// emits those values.
    pub(super) fn find_all(
        &self,
        key: D::Metadata,
        policy_data: &PolicyData,
    ) -> impl Iterator<Item = D> {
        let key_hash = Self::metadata_hash(&key);
        let offset = self.index.find(key_hash, |&offset| {
            let element = View::<D>::at(u32::from(offset));
            key == element.read_metadata(policy_data)
        });
        (HashedArrayViewEntryIter {
            policy_data: policy_data,
            limit: self.limit,
            metadata: key,
            offset: offset.map(|offset| u32::from(*offset)),
        })
        .map(|element| element.parse(policy_data))
    }

    /// Returns an iterator that emits a view for each reachable element.
    pub(super) fn iter(&self, policy_data: &PolicyData) -> impl Iterator<Item = View<D>> {
        self.index
            .iter()
            .map(|offset| {
                let element = View::<D>::at(u32::from(*offset));
                HashedArrayViewEntryIter {
                    policy_data: policy_data,
                    limit: self.limit,
                    metadata: element.read_metadata(policy_data),
                    offset: Some(u32::from(*offset)),
                }
            })
            .flatten()
    }
}

impl<D: Parse + HasMetadata + Walk> Parse for HashedArrayView<D>
where
    D::Metadata: Eq + Debug + PartialEq + Parse + Hash,
{
    type Error = anyhow::Error;

    fn parse<'a>(cursor: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let (array_view, cursor) = SimpleArrayView::<D>::parse(cursor)?;

        // Allocate a hash table sized appropriately for the array size.
        let mut index = HashTable::with_capacity(array_view.count as usize);

        // Record the offset at which the last array element ends.
        let limit = cursor.offset();

        // Iterate over the elements inserting the first offset at which each is
        // seen into a hash bucket.
        for view in array_view.data().iter(cursor.data()) {
            let metadata = view.read_metadata(cursor.data());

            index
                .entry(
                    Self::metadata_hash(&metadata),
                    |&offset| {
                        let element = View::<D>::at(u32::from(offset));
                        metadata == element.read_metadata(cursor.data())
                    },
                    |&offset| {
                        let element = View::<D>::at(u32::from(offset));
                        Self::metadata_hash(&element.read_metadata(cursor.data()))
                    },
                )
                .or_insert(U24::try_from(view.start()).expect("Policy offsets ought fit in U24!"));
        }

        Ok((Self { phantom: PhantomData, index, limit }, cursor))
    }
}

impl<D: Validate + Parse + HasMetadata + Walk> Validate for HashedArrayView<D>
where
    D::Metadata: Eq,
{
    type Error = anyhow::Error;

    fn validate(&self, context: &PolicyValidationContext) -> Result<(), Self::Error> {
        let policy_data = context.data.clone();
        for element in self
            .index
            .iter()
            .map(|offset| {
                let element = View::<D>::at(u32::from(*offset));
                HashedArrayViewEntryIter::<D> {
                    policy_data: &policy_data,
                    limit: self.limit,
                    metadata: element.read_metadata(&policy_data),
                    offset: Some(u32::from(*offset)),
                }
            })
            .flatten()
        {
            element.validate(context)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::U24;

    #[test]
    fn to_and_from_u24() {
        for i in 0u32..0x10000 {
            let u24_result = U24::try_from(i);
            assert!(u24_result.is_ok());
            let u24 = u24_result.unwrap();
            assert_eq!(i >> 16, u24.high as u32);
            assert_eq!((i >> 8) & 0xff, u24.middle as u32);
            assert_eq!(i & 0xff, u24.low as u32);
            let j = u32::from(u24);
            assert_eq!(i, j);
        }

        assert!(U24::try_from(0x1000000).is_err());
    }
}
