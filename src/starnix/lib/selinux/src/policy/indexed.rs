// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fmt::Debug;
use std::hash::{BuildHasher, Hash, Hasher};

use hashbrown::HashTable;
use hashbrown::hash_table::Entry;

use super::error::{ParseError, SerializeError, ValidateError};
use super::parser::{PolicyCursor, PolicyWriter, SymbolArray};
use super::traits::{HasName, HasPolicyId, Parse, PolicyId, Serialize, Validate};
use super::{Policy, U24Index};

/// Helper to hash a byte slice name using the rapidhash hasher.
pub fn hash_name(hasher: &rapidhash::RapidBuildHasher, name: &[u8]) -> u64 {
    let mut state = hasher.build_hasher();
    name.hash(&mut state);
    state.finish()
}

/// Trait for slice-backed containers that can be indexed by [`IdAndNameIndexed`].
pub trait AsSlice {
    type Item;
    fn as_slice(&self) -> &[Self::Item];
}

impl<T> AsSlice for &[T] {
    type Item = T;
    fn as_slice(&self) -> &[T] {
        self
    }
}

impl<T> AsSlice for Box<[T]> {
    type Item = T;
    fn as_slice(&self) -> &[T] {
        self
    }
}

impl<T> AsSlice for SymbolArray<T> {
    type Item = T;
    fn as_slice(&self) -> &[T] {
        self.as_ref()
    }
}

/// Container that provides $O(1)$ lookups by both policy ID and Name.
///
/// It wraps a collection `C` (typically `Box<[T]>`) and builds indexing tables
/// during construction (which happens after the underlying collection has been
/// parsed).
///
/// The order of elements in the original collection is currently retained, to
/// allow [`IdAndNameIndexed`] to serialize the collection into exactly the same
/// byte sequence as it was parsed from.
pub struct IdAndNameIndexed<C> {
    container: C,
    id_to_index: Box<[Option<U24Index>]>,
    name_to_index: HashTable<U24Index>,
    hasher: rapidhash::RapidBuildHasher,
}

impl<C: AsSlice> IdAndNameIndexed<C> {
    /// Returns an iterator over the elements in the indexed collection.
    pub fn iter(&self) -> std::slice::Iter<'_, C::Item> {
        self.container.as_slice().iter()
    }

    /// Returns the number of elements in the indexed collection.
    pub fn len(&self) -> usize {
        self.container.as_slice().len()
    }

    /// Returns `true` if the indexed collection contains no elements.
    pub fn is_empty(&self) -> bool {
        self.container.as_slice().is_empty()
    }
}

impl<'a, C: AsSlice> IntoIterator for &'a IdAndNameIndexed<C> {
    type Item = &'a C::Item;
    type IntoIter = std::slice::Iter<'a, C::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<C> IdAndNameIndexed<C>
where
    C: AsSlice,
    C::Item: HasPolicyId + HasName,
{
    /// Constructs a new [`IdAndNameIndexed`] wrapping the supplied `container`.
    ///
    /// Returns an error if `container` contains more than `U24Index::MAX` items.
    pub(super) fn new(container: C) -> Result<Self, ParseError> {
        let items = container.as_slice();
        let mut id_to_index = Vec::with_capacity(items.len() + 1);
        let hasher = rapidhash::RapidBuildHasher::default();
        let mut name_to_index: HashTable<U24Index> = HashTable::with_capacity(items.len());

        for (index, item) in items.iter().enumerate() {
            let id = item.id().as_u32() as usize;
            if id >= id_to_index.len() {
                id_to_index.resize(id + 1, None);
            } else if id_to_index[id].is_some() {
                return Err(ParseError::DuplicateId { id: item.id().as_u32() });
            }
            let u24_idx: U24Index = index.try_into()?;
            id_to_index[id] = Some(u24_idx);

            let name = item.name();
            let hash = hash_name(&hasher, name);
            let Entry::Vacant(entry) = name_to_index.entry(
                hash,
                |&idx| items[idx].name() == name,
                |&idx| hash_name(&hasher, items[idx].name()),
            ) else {
                return Err(ParseError::DuplicateName { name: name.into() });
            };
            entry.insert(u24_idx);
        }

        id_to_index.shrink_to_fit();
        name_to_index.shrink_to_fit(|&idx| hash_name(&hasher, items[idx].name()));

        Ok(Self { container, id_to_index: id_to_index.into_boxed_slice(), name_to_index, hasher })
    }

    /// Returns a reference to the item with the specified `id`, if it exists.
    pub fn get_by_id(&self, id: <C::Item as HasPolicyId>::Id) -> Option<&C::Item> {
        let idx = *self.id_to_index.get(id.as_u32() as usize)?;
        idx.map(|i| &self.container.as_slice()[i])
    }

    /// Returns a reference to the item with the specified `name`, if it exists.
    pub fn get_by_name(&self, name: &[u8]) -> Option<&C::Item> {
        let items = self.container.as_slice();
        let hash = hash_name(&self.hasher, name);
        let idx = self.name_to_index.find(hash, |&idx| items[idx].name() == name)?;
        Some(&items[*idx])
    }
}

#[cfg(test)]
impl<T> IdAndNameIndexed<SymbolArray<T>> {
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        self.container.as_mut_slice()
    }
}

impl<C: Debug> Debug for IdAndNameIndexed<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdAndNameIndexed").field("container", &self.container).finish()
    }
}

impl<C> Parse for IdAndNameIndexed<C>
where
    C: Parse + AsSlice,
    C::Item: HasPolicyId + HasName,
{
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let container = C::parse(cursor)?;
        Self::new(container)
    }
}

impl<C: Serialize> Serialize for IdAndNameIndexed<C> {
    fn serialize(&self, writer: &mut PolicyWriter<'_>) -> Result<(), SerializeError> {
        self.container.serialize(writer)
    }
}

impl<C: Validate> Validate for IdAndNameIndexed<C> {
    fn validate(&self, policy: &Policy) -> Result<(), ValidateError> {
        self.container.validate(policy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::id_type::IdType;
    use crate::policy::traits::{HasName, HasPolicyId};

    #[derive(Copy, Clone, Debug, Hash, Eq, Ord, PartialEq, PartialOrd)]
    struct TestTag;
    type TestId = IdType<std::num::NonZeroU16, TestTag>;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct TestItem {
        id: TestId,
        name: &'static str,
    }

    impl HasName for TestItem {
        fn name(&self) -> &[u8] {
            self.name.as_bytes()
        }
    }

    impl HasPolicyId for TestItem {
        type Id = TestId;
        fn id(&self) -> Self::Id {
            self.id
        }
    }

    #[test]
    fn test_indexed_empty() {
        let items: &[TestItem] = &[];
        let indexed = IdAndNameIndexed::new(items).unwrap();
        assert!(indexed.get_by_id(TestId::for_test(1)).is_none());
        assert!(indexed.get_by_name(b"foo").is_none());
    }

    #[test]
    fn test_indexed_lookup() {
        let items: &[TestItem] = &[
            TestItem { id: TestId::for_test(1), name: "foo" },
            TestItem { id: TestId::for_test(2), name: "bar" },
        ];
        let indexed = IdAndNameIndexed::new(items).unwrap();

        assert_eq!(
            indexed.get_by_id(TestId::for_test(1)),
            Some(&TestItem { id: TestId::for_test(1), name: "foo" })
        );
        assert_eq!(
            indexed.get_by_id(TestId::for_test(2)),
            Some(&TestItem { id: TestId::for_test(2), name: "bar" })
        );
        assert_eq!(indexed.get_by_id(TestId::for_test(3)), None);

        assert_eq!(
            indexed.get_by_name(b"foo"),
            Some(&TestItem { id: TestId::for_test(1), name: "foo" })
        );
        assert_eq!(
            indexed.get_by_name(b"bar"),
            Some(&TestItem { id: TestId::for_test(2), name: "bar" })
        );
        assert_eq!(indexed.get_by_name(b"baz"), None);
    }

    #[test]
    fn test_indexed_duplicate_id() {
        let items: &[TestItem] = &[
            TestItem { id: TestId::for_test(1), name: "foo" },
            TestItem { id: TestId::for_test(1), name: "bar" },
        ];
        let result = IdAndNameIndexed::new(items);
        assert!(matches!(result, Err(ParseError::DuplicateId { id: 1 })));
    }

    #[test]
    fn test_indexed_duplicate_name() {
        let items: &[TestItem] = &[
            TestItem { id: TestId::for_test(1), name: "foo" },
            TestItem { id: TestId::for_test(2), name: "foo" },
        ];
        let result = IdAndNameIndexed::new(items);
        assert!(matches!(result, Err(ParseError::DuplicateName { name }) if name == b"foo"));
    }
}
