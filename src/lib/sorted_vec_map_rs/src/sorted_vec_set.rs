// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::borrow::Borrow;
use std::collections::BTreeSet;
use std::fmt::Debug;
use std::slice;

/// An ordered set built on a `Vec`.
///
/// This set is optimized for reducing the memory usage of data that rarely or never changes.
/// Insertions and removals take linear time while lookups take logarithmic time.
#[derive(Eq, PartialEq, PartialOrd, Ord, Hash, Clone, Default)]
pub struct SortedVecSet<T> {
    vec: Vec<T>,
}

impl<T> SortedVecSet<T> {
    /// Constructs a new, empty `SortedVecSet`.
    pub fn new() -> Self {
        Self { vec: Vec::new() }
    }

    /// Returns true if there are no elements in the set.
    pub fn is_empty(&self) -> bool {
        self.vec.is_empty()
    }

    /// Returns the number of elements in the set.
    pub fn len(&self) -> usize {
        self.vec.len()
    }

    /// Returns true if the set contains the given value.
    pub fn contains<Q>(&self, value: &Q) -> bool
    where
        T: Borrow<Q> + Ord,
        Q: Ord + ?Sized,
    {
        self.vec.binary_search_by(|probe| probe.borrow().cmp(value)).is_ok()
    }

    /// Inserts a value into the set. Returns true if the value was not already present.
    pub fn insert(&mut self, value: T) -> bool
    where
        T: Ord,
    {
        match self.vec.binary_search(&value) {
            Ok(_) => false,
            Err(index) => {
                self.vec.insert(index, value);
                true
            }
        }
    }

    /// Returns an iterator over the elements of the set, in sorted order.
    pub fn iter(&self) -> slice::Iter<'_, T> {
        self.vec.iter()
    }

    /// Returns an iterator yielding elements from both sets in sorted order, without duplicates.
    ///
    /// Time complexity: O(N + M) where N is the number of elements in `self` and M is the number of elements in `other`.
    pub fn union<'a>(&'a self, other: &'a Self) -> Union<'a, T> {
        let mut iter1 = self.iter();
        let mut iter2 = other.iter();
        Union { next1: iter1.next(), next2: iter2.next(), iter1, iter2 }
    }

    /// Returns an iterator yielding elements in `self` that are not in `other`.
    ///
    /// Time complexity: O(N + M) where N is the number of elements in `self` and M is the number of elements in `other`.
    pub fn difference<'a>(&'a self, other: &'a Self) -> Difference<'a, T> {
        let mut iter1 = self.iter();
        let mut iter2 = other.iter();
        Difference { next1: iter1.next(), next2: iter2.next(), iter1, iter2 }
    }
}

/// An iterator yielding elements from the union of two sets.
pub struct Union<'a, T> {
    iter1: std::slice::Iter<'a, T>,
    iter2: std::slice::Iter<'a, T>,
    next1: Option<&'a T>,
    next2: Option<&'a T>,
}

impl<'a, T: Ord> Iterator for Union<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        match (self.next1, self.next2) {
            (Some(v1), Some(v2)) => match v1.cmp(v2) {
                std::cmp::Ordering::Less => {
                    let res = v1;
                    self.next1 = self.iter1.next();
                    Some(res)
                }
                std::cmp::Ordering::Equal => {
                    let res = v1;
                    self.next1 = self.iter1.next();
                    self.next2 = self.iter2.next();
                    Some(res)
                }
                std::cmp::Ordering::Greater => {
                    let res = v2;
                    self.next2 = self.iter2.next();
                    Some(res)
                }
            },
            (Some(v1), None) => {
                let res = v1;
                self.next1 = self.iter1.next();
                Some(res)
            }
            (None, Some(v2)) => {
                let res = v2;
                self.next2 = self.iter2.next();
                Some(res)
            }
            (None, None) => None,
        }
    }
}

/// An iterator yielding elements in one set that are not in another.
pub struct Difference<'a, T> {
    iter1: std::slice::Iter<'a, T>,
    iter2: std::slice::Iter<'a, T>,
    next1: Option<&'a T>,
    next2: Option<&'a T>,
}

impl<'a, T: Ord> Iterator for Difference<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match (self.next1, self.next2) {
                (Some(v1), Some(v2)) => match v1.cmp(v2) {
                    std::cmp::Ordering::Less => {
                        let res = v1;
                        self.next1 = self.iter1.next();
                        return Some(res);
                    }
                    std::cmp::Ordering::Equal => {
                        self.next1 = self.iter1.next();
                        self.next2 = self.iter2.next();
                    }
                    std::cmp::Ordering::Greater => {
                        self.next2 = self.iter2.next();
                    }
                },
                (Some(v1), None) => {
                    let res = v1;
                    self.next1 = self.iter1.next();
                    return Some(res);
                }
                (None, _) => return None,
            }
        }
    }
}

impl<T: Debug> Debug for SortedVecSet<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

impl<T: Ord> FromIterator<T> for SortedVecSet<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut vec = Vec::from_iter(iter);
        vec.sort();
        vec.dedup();
        Self { vec }
    }
}

impl<T> From<SortedVecSet<T>> for Vec<T> {
    fn from(set: SortedVecSet<T>) -> Self {
        set.vec
    }
}

impl<T: Ord> From<Vec<T>> for SortedVecSet<T> {
    fn from(mut vec: Vec<T>) -> Self {
        vec.sort();
        vec.dedup();
        Self { vec }
    }
}

impl<T: Ord, const N: usize> From<[T; N]> for SortedVecSet<T> {
    fn from(arr: [T; N]) -> Self {
        let mut vec = Vec::from(arr);
        vec.sort();
        vec.dedup();
        Self { vec }
    }
}

impl<T: Ord> From<BTreeSet<T>> for SortedVecSet<T> {
    fn from(set: BTreeSet<T>) -> Self {
        Self { vec: Vec::from_iter(set) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(vec![], 1 => false; "empty_set")]
    #[test_case(vec![1], 1 => true; "contains_element")]
    #[test_case(vec![0, 1], 0 => true; "contains_first")]
    #[test_case(vec![0, 1, 2], 3 => false; "does_not_contain")]
    fn test_contains(initial: Vec<i32>, lookup: i32) -> bool {
        let set: SortedVecSet<i32> = initial.into();
        set.contains(&lookup)
    }

    #[test_case(vec![], 50 => (true, vec![50]); "insert_empty")]
    #[test_case(vec![50], 47 => (true, vec![47, 50]); "insert_lesser")]
    #[test_case(vec![47, 50], 48 => (true, vec![47, 48, 50]); "insert_middle")]
    #[test_case(vec![47, 48, 50], 48 => (false, vec![47, 48, 50]); "insert_duplicate")]
    fn test_insert(initial: Vec<i32>, value: i32) -> (bool, Vec<i32>) {
        let mut set: SortedVecSet<i32> = initial.into();
        let inserted = set.insert(value);
        (inserted, set.into())
    }

    #[test_case(vec![50, 47, 48, 50, 47], vec![47, 48, 50]; "duplicates_and_unsorted")]
    #[test_case(vec![], vec![]; "empty")]
    #[test_case(vec![1, 2, 3], vec![1, 2, 3]; "already_sorted")]
    fn test_from_iter(input: Vec<i32>, expected: Vec<i32>) {
        let set: SortedVecSet<i32> = input.into_iter().collect();
        let actual: Vec<i32> = set.into();
        assert_eq!(actual, expected);
    }

    #[test_case(vec![1, 2, 3], vec![1, 2, 3]; "simple_conversion")]
    #[test_case(vec![], vec![]; "empty_conversion")]
    fn test_into_vec(input: Vec<i32>, expected: Vec<i32>) {
        let set: SortedVecSet<i32> = input.into();
        let actual: Vec<i32> = set.into();
        assert_eq!(actual, expected);
    }

    #[test_case(vec![3, 1, 2, 2], vec![1, 2, 3]; "removes_duplicates_and_sorts")]
    #[test_case(vec![], vec![]; "empty_vec")]
    fn test_from_vec(input: Vec<i32>, expected: Vec<i32>) {
        let set: SortedVecSet<i32> = input.into();
        let actual: Vec<i32> = set.into();
        assert_eq!(actual, expected);
    }

    #[test_case([3, 1, 2, 2], vec![1, 2, 3]; "array_conversion")]
    fn test_from_array(input: [i32; 4], expected: Vec<i32>) {
        let set: SortedVecSet<i32> = input.into();
        let actual: Vec<i32> = set.into();
        assert_eq!(actual, expected);
    }

    #[test_case(vec![], None => 0; "empty")]
    #[test_case(vec![], Some(1) => 1; "insert_into_empty")]
    #[test_case(vec![1], Some(2) => 2; "insert_new")]
    #[test_case(vec![1, 2], Some(1) => 2; "insert_duplicate")]
    fn test_len(initial: Vec<i32>, to_insert: Option<i32>) -> usize {
        let mut set: SortedVecSet<i32> = initial.into();
        if let Some(v) = to_insert {
            set.insert(v);
        }
        set.len()
    }

    #[test_case(vec![1, 2, 3], vec![3, 4, 5], vec![1, 2, 3, 4, 5]; "overlapping")]
    #[test_case(vec![1, 2, 3], vec![4, 5, 6], vec![1, 2, 3, 4, 5, 6]; "disjoint")]
    #[test_case(vec![1, 2, 3], vec![], vec![1, 2, 3]; "empty_rhs")]
    #[test_case(vec![], vec![1, 2, 3], vec![1, 2, 3]; "empty_lhs")]
    fn test_union(lhs: Vec<i32>, rhs: Vec<i32>, expected: Vec<i32>) {
        let set1: SortedVecSet<i32> = lhs.into();
        let set2: SortedVecSet<i32> = rhs.into();
        let actual: Vec<i32> = set1.union(&set2).cloned().collect();
        assert_eq!(actual, expected);
    }

    #[test_case(vec![1, 2, 3], vec![3, 4, 5], vec![1, 2]; "lhs_difference")]
    #[test_case(vec![3, 4, 5], vec![1, 2, 3], vec![4, 5]; "rhs_difference")]
    #[test_case(vec![1, 2, 3], vec![1, 2, 3], vec![]; "identical_sets")]
    #[test_case(vec![1, 2, 3], vec![], vec![1, 2, 3]; "empty_rhs")]
    #[test_case(vec![], vec![1, 2, 3], vec![]; "empty_lhs")]
    fn test_difference(lhs: Vec<i32>, rhs: Vec<i32>, expected: Vec<i32>) {
        let set1: SortedVecSet<i32> = lhs.into();
        let set2: SortedVecSet<i32> = rhs.into();
        let actual: Vec<i32> = set1.difference(&set2).cloned().collect();
        assert_eq!(actual, expected);
    }

    #[test_case(vec![3, 1, 2], vec![1, 2, 3]; "btreeset_conversion")]
    #[test_case(vec![], vec![]; "empty_btreeset")]
    fn test_from_btreeset(input: Vec<i32>, expected: Vec<i32>) {
        let btree_set: BTreeSet<i32> = input.into_iter().collect();
        let set: SortedVecSet<i32> = btree_set.into();
        let actual: Vec<i32> = set.into();
        assert_eq!(actual, expected);
    }
}
