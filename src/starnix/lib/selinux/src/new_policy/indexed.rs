// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fmt::Debug;
use std::hash::{BuildHasher, Hash, Hasher};
use std::ops::Deref;

use hashbrown::HashTable;

use super::NewPolicy;
use super::error::{ParseError, SerializeError, ValidateError};
use super::parser::PolicyCursor;
use super::traits::{HasName, HasPolicyId, Parse, PolicyId, Serialize, Validate};

/// Helper to hash a byte slice name using the rapidhash hasher.
fn hash_name(hasher: &rapidhash::RapidBuildHasher, name: &[u8]) -> u64 {
    let mut state = hasher.build_hasher();
    name.hash(&mut state);
    state.finish()
}

/// Container that provides $O(1)$ lookups by both policy ID and Name.
///
/// It wraps a collection `C` (typically `Box<[T]>`) and builds indexing tables
/// during construction (which happens after the underlying collection has been
/// parsed).
///
/// The order of elements in the original collection is currently retained, to
/// allow the `IdAndNameIndexed<C>` to serialize the collection into exactly the same
/// byte sequence as it was parsed from.
#[derive(Clone)]
pub struct IdAndNameIndexed<C> {
    container: C,
    id_to_index: Box<[Option<u32>]>,
    name_to_index: HashTable<u32>,
    hasher: rapidhash::RapidBuildHasher,
}

impl<C, T> IdAndNameIndexed<C>
where
    C: Deref<Target = [T]>,
    T: HasPolicyId + HasName,
{
    /// Constructs a new [`IdAndNameIndexed`] wrapping the supplied `container`.
    ///
    /// Panics if the container has more than `u32::MAX` items.
    pub fn new(container: C) -> Self {
        assert!(container.len() <= u32::MAX as usize, "too many items in IdAndNameIndexed");
        let mut id_to_index = Vec::with_capacity(container.len() + 1);
        let hasher = rapidhash::RapidBuildHasher::default();
        let mut name_to_index = HashTable::with_capacity(container.len());

        for (index, item) in container.iter().enumerate() {
            let id = item.id().as_u32() as usize;
            if id >= id_to_index.len() {
                id_to_index.resize(id + 1, None);
            }
            id_to_index[id] = Some(index as u32);

            let name = item.name();
            let hash = hash_name(&hasher, name);
            name_to_index.insert_unique(hash, index as u32, |&idx| {
                hash_name(&hasher, container[idx as usize].name())
            });
        }

        Self { container, id_to_index: id_to_index.into_boxed_slice(), name_to_index, hasher }
    }

    /// Returns a reference to the item with the specified `id`, if it exists.
    pub fn get_by_id(&self, id: T::Id) -> Option<&T> {
        let idx = *self.id_to_index.get(id.as_u32() as usize)?;
        idx.map(|i| &self.container[i as usize])
    }

    /// Returns a reference to the item with the specified `name`, if it exists.
    pub fn get_by_name(&self, name: &[u8]) -> Option<&T> {
        let hash = hash_name(&self.hasher, name);
        let idx =
            self.name_to_index.find(hash, |&idx| self.container[idx as usize].name() == name)?;
        Some(&self.container[*idx as usize])
    }
}

impl<C> Deref for IdAndNameIndexed<C> {
    type Target = C;
    fn deref(&self) -> &Self::Target {
        &self.container
    }
}

impl<C: Debug> Debug for IdAndNameIndexed<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdAndNameIndexed").field("container", &self.container).finish()
    }
}

impl<C: PartialEq> PartialEq for IdAndNameIndexed<C> {
    fn eq(&self, other: &Self) -> bool {
        self.container == other.container
    }
}

impl<C: Eq> Eq for IdAndNameIndexed<C> {}

impl<C, T> Parse for IdAndNameIndexed<C>
where
    C: Parse + Deref<Target = [T]>,
    T: HasPolicyId + HasName,
{
    fn parse(cursor: &mut PolicyCursor<'_>) -> Result<Self, ParseError> {
        let container = C::parse(cursor)?;
        Ok(Self::new(container))
    }
}

impl<C: Serialize> Serialize for IdAndNameIndexed<C> {
    fn serialize(&self, writer: &mut Vec<u8>) -> Result<(), SerializeError> {
        self.container.serialize(writer)
    }
}

impl<C, T> Validate for IdAndNameIndexed<C>
where
    C: Deref<Target = [T]>,
    T: Validate,
{
    fn validate(&self, policy: &NewPolicy) -> Result<(), ValidateError> {
        for item in self.container.iter() {
            item.validate(policy)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_policy::id_type::IdType;
    use crate::new_policy::traits::{HasName, HasPolicyId};

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
        let indexed = IdAndNameIndexed::new(items);
        assert!(indexed.get_by_id(TestId::for_test(1)).is_none());
        assert!(indexed.get_by_name(b"foo").is_none());
    }

    #[test]
    fn test_indexed_lookup() {
        let items: &[TestItem] = &[
            TestItem { id: TestId::for_test(1), name: "foo" },
            TestItem { id: TestId::for_test(2), name: "bar" },
        ];
        let indexed = IdAndNameIndexed::new(items);

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
}
