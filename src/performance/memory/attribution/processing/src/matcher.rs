// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::BTreeMap;
use zerotrie::ZeroTrieSimpleAscii;

/// A bitset of bucket indices backed by a `usize`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BucketSet(usize);

impl BucketSet {
    pub const MAX_BUCKETS: usize = usize::BITS as usize;

    pub fn from_index(bucket_idx: usize) -> Result<Self, MatcherError> {
        if bucket_idx >= Self::MAX_BUCKETS {
            return Err(MatcherError::TooManyBuckets(bucket_idx + 1));
        }
        Ok(Self(1usize << bucket_idx))
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Returns the smallest bucket index in the set, or `None` if the set is empty.
    pub fn min_index(self) -> Option<usize> {
        if self.is_empty() { None } else { Some(self.0.trailing_zeros() as usize) }
    }
}

/// Prefix trie that maps string prefixes to a [`BucketSet`] of matching bucket indices.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct BucketTrie {
    /// Set of buckets that do not constrain this dimension (`None` or empty pattern).
    unconstrained: BucketSet,
    /// Prefix trie mapping each ASCII prefix to the bitmask of buckets matched by that prefix.
    trie: ZeroTrieSimpleAscii<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatcherError {
    InvalidPattern(String),
    TooManyBuckets(usize),
}

impl std::fmt::Display for MatcherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPattern(msg) => write!(f, "invalid bucket name pattern: {msg}"),
            Self::TooManyBuckets(count) => write!(
                f,
                "too many bucket definitions ({count}); maximum supported is {}",
                BucketSet::MAX_BUCKETS
            ),
        }
    }
}

impl std::error::Error for MatcherError {}

impl BucketTrie {
    /// Builds a combined prefix trie from an iterator of `(bucket_index, &[String])`.
    ///
    /// Each bucket `i` is represented in a [`BucketSet`]. Because bucket prefixes are not
    /// necessarily prefix-free, shorter prefixes' sets are unioned into any longer prefixes
    /// that start with them, and matching unions all prefix values encountered along a single
    /// cursor traversal.
    pub fn build<'a>(
        patterns: impl Iterator<Item = (usize, &'a [String])>,
    ) -> Result<Self, MatcherError> {
        let mut unconstrained = BucketSet::default();
        let mut prefixes: BTreeMap<&'a str, BucketSet> = BTreeMap::new();

        for (bucket_idx, bucket_prefixes) in patterns {
            let bucket = BucketSet::from_index(bucket_idx)?;
            if bucket_prefixes.is_empty() {
                unconstrained = unconstrained.union(bucket);
            } else {
                for prefix in bucket_prefixes {
                    let entry = prefixes.entry(prefix.as_str()).or_default();
                    *entry = entry.union(bucket);
                }
            }
        }

        // Propagate bucket sets from shorter prefixes to longer prefixes that share them so every
        // node value in the trie holds the full set of buckets matching up to that prefix.
        let keys: Vec<&'a str> = prefixes.keys().copied().collect();
        for i in 0..keys.len() {
            let prefix = keys[i];
            let prefix_set = prefixes[prefix];
            for &other in &keys[i + 1..] {
                if other.starts_with(prefix) {
                    let entry = prefixes.get_mut(other).unwrap();
                    *entry = entry.union(prefix_set);
                } else if !prefix.is_empty() {
                    // Keys in BTreeMap are sorted lexicographically, so once a key no longer
                    // starts with a non-empty `prefix`, no subsequent key can start with it.
                    break;
                }
            }
        }

        let raw_prefixes: BTreeMap<&'a str, usize> =
            prefixes.into_iter().map(|(k, v)| (k, v.0)).collect();
        let trie = ZeroTrieSimpleAscii::try_from_btree_map_str(&raw_prefixes)
            .map_err(|e| MatcherError::InvalidPattern(format!("{e:?}")))?;
        Ok(Self { unconstrained, trie })
    }

    /// Traverses the trie at most once for `haystack` (stopping at the first null byte `0` or when
    /// the trie cursor becomes empty) and returns the [`BucketSet`] of all buckets whose pattern
    /// matches a prefix of `haystack` (unioned with `unconstrained`).
    pub fn match_bytes(&self, haystack: &[u8]) -> BucketSet {
        let mut matched = self.unconstrained;
        if self.trie.is_empty() {
            return matched;
        }
        let mut cursor = self.trie.cursor();
        if let Some(v) = cursor.take_value() {
            matched = matched.union(BucketSet(v));
        }
        for &b in haystack {
            if b == 0 || cursor.is_empty() {
                break;
            }
            cursor.step(b);
            if let Some(v) = cursor.take_value() {
                matched = matched.union(BucketSet(v));
            }
        }
        matched
    }

    pub fn match_str(&self, haystack: &str) -> BucketSet {
        self.match_bytes(haystack.as_bytes())
    }

    /// Returns the [`BucketSet`] of all buckets whose pattern matches a prefix of at least one
    /// element in `haystacks` (unioned with `unconstrained`).
    pub fn match_any_str(&self, haystacks: &[&str]) -> BucketSet {
        let mut matched = self.unconstrained;
        if !self.trie.is_empty() {
            for haystack in haystacks {
                matched = matched.union(self.match_str(haystack));
            }
        }
        matched
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single_trie(prefixes: &[&str]) -> BucketTrie {
        let owned: Vec<String> = prefixes.iter().map(|s| (*s).to_string()).collect();
        BucketTrie::build(std::iter::once((0, owned.as_slice()))).unwrap()
    }

    #[test]
    fn test_bucket_set_operations() {
        let b0 = BucketSet::from_index(0).unwrap();
        let b2 = BucketSet::from_index(2).unwrap();
        let b3 = BucketSet::from_index(3).unwrap();

        assert!(BucketSet::default().is_empty());
        assert_eq!(BucketSet::default().min_index(), None);

        let u = b2.union(b3);
        assert!(!u.is_empty());
        assert_eq!(u.min_index(), Some(2));
        assert_eq!(u.union(b0).min_index(), Some(0));
        assert_eq!(u.intersection(b3), b3);
    }

    #[test]
    fn test_bucket_trie_prefix_matching() {
        let b0 = BucketSet::from_index(0).unwrap();
        let empty = BucketSet::default();

        let t = single_trie(&["fxfs.cm"]);
        assert_eq!(t.match_str("fxfs.cm"), b0);
        assert_eq!(t.match_str("fxfs.cm:extra"), b0);
        assert_eq!(t.match_str("path/to/fxfs.cm"), empty);
        assert_eq!(t.match_str("fxfs_cm"), empty);

        let t_start = single_trie(&["ext4!"]);
        assert_eq!(t_start.match_str("ext4!vmo"), b0);
        assert_eq!(t_start.match_str("prefix_ext4!vmo"), empty);

        let t_or = single_trie(&["blob-", "driver-host-#bluetooth"]);
        assert_eq!(t_or.match_str("blob-1234"), b0);
        assert_eq!(t_or.match_str("driver-host-#bluetooth"), b0);
        assert_eq!(t_or.match_str("other-process"), empty);
    }

    #[test]
    fn test_bucket_trie_resolvers_and_prefixes() {
        let b0 = BucketSet::from_index(0).unwrap();
        let empty = BucketSet::default();

        let t_resolvers = single_trie(&[
            "pkg-resolver.cm",
            "dns_resolver.cm",
            "process_resolver.cm",
            "loopback_resolver.cm",
        ]);
        assert_eq!(t_resolvers.match_str("pkg-resolver.cm"), b0);
        assert_eq!(t_resolvers.match_str("dns_resolver.cm"), b0);
        assert_eq!(t_resolvers.match_str("process_resolver.cm"), b0);
        assert_eq!(t_resolvers.match_str("loopback_resolver.cm"), b0);
        assert_eq!(t_resolvers.match_str("other-resolver.cm"), empty);

        let t_star = single_trie(&["netcfg"]);
        assert_eq!(t_star.match_str("netcfg.cm"), b0);
        assert_eq!(t_star.match_str("netcfg_test.cm"), b0);
        assert_eq!(t_star.match_str("other_netcfg.cm"), empty);

        let t_all = single_trie(&[]);
        assert_eq!(t_all.match_str(""), b0);
        assert_eq!(t_all.match_str("anything"), b0);
    }

    #[test]
    fn test_bucket_trie_overlapping_non_prefix_free_sets() {
        let p0 = vec!["driver-host-#bluetooth".to_string()];
        let p1 = vec!["driver-host-".to_string()];
        let p2 = vec!["driver-host-#bluetooth".to_string(), "fxfs.cm".to_string()];
        let p3: Vec<String> = vec![];
        let trie = BucketTrie::build(
            [(0, p0.as_slice()), (1, p1.as_slice()), (2, p2.as_slice()), (3, p3.as_slice())]
                .into_iter(),
        )
        .unwrap();

        // "driver-host-#bluetooth" matches bucket 0, 1, 2, and unconstrained bucket 3.
        assert_eq!(trie.match_str("driver-host-#bluetooth"), BucketSet(0b1111));
        assert_eq!(trie.match_str("driver-host-#bluetooth:suffix"), BucketSet(0b1111));
        // "driver-host-#other" matches bucket 1 and unconstrained bucket 3.
        assert_eq!(trie.match_str("driver-host-#other"), BucketSet(0b1010));
        // "fxfs.cm" matches bucket 2 and unconstrained bucket 3.
        assert_eq!(trie.match_str("fxfs.cm"), BucketSet(0b1100));
        // "unmatched" matches only unconstrained bucket 3.
        assert_eq!(trie.match_str("unmatched"), BucketSet(0b1000));
    }

    #[test]
    fn test_bucket_trie_rejects_non_ascii_patterns() {
        let non_ascii = vec!["foo🦀bar".to_string()];
        assert!(BucketTrie::build(std::iter::once((0, non_ascii.as_slice()))).is_err());
    }
}
