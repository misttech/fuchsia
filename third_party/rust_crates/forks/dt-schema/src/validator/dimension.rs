// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use serde::Serialize;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
struct Range {
    min: usize,
    max: usize,
}

impl Range {
    /// True if both min and max are zero.
    fn is_zero(&self) -> bool {
        self.min == 0 && self.max == 0
    }

    /// True if either value is not zero.
    fn has_value(&self) -> bool {
        self.min != 0 || self.max != 0
    }

    /// True if min == max.
    fn is_fixed(&self) -> bool {
        self.min == self.max
    }

    /// Return a range that encompasses |self| and |other|.
    fn merge(&self, other: &Range) -> Range {
        if self.is_zero() {
            *other
        } else if other.is_zero() {
            *self
        } else {
            Range {
                min: std::cmp::min(self.min, other.min),
                max: std::cmp::max(self.max, other.max),
            }
        }
    }
}

impl From<[usize; 2]> for Range {
    fn from(value: [usize; 2]) -> Self {
        Range {
            min: value[0],
            max: value[1],
        }
    }
}

impl From<Range> for [usize; 2] {
    fn from(value: Range) -> Self {
        [value.min, value.max]
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Hash, PartialOrd, Ord, Default)]
#[serde(from = "[[usize; 2]; 2]", into = "[[usize; 2]; 2]")]
/// |Dimension| represents constraints on a matrix.
pub struct Dimension {
    /// Range of outer elements possible.
    outer: Range,
    /// Range of elements allowed in each inner element.
    inner: Range,
}

impl From<[[usize; 2]; 2]> for Dimension {
    fn from(value: [[usize; 2]; 2]) -> Self {
        Dimension {
            outer: value[0].into(),
            inner: value[1].into(),
        }
    }
}

impl From<Dimension> for [[usize; 2]; 2] {
    fn from(value: Dimension) -> Self {
        [value.outer.into(), value.inner.into()]
    }
}

impl Dimension {
    /// Merge two dimensions into a third dimension.
    pub fn merge(&self, other: Dimension) -> Dimension {
        Dimension {
            outer: self.outer.merge(&other.outer),
            inner: self.inner.merge(&other.inner),
        }
    }

    /// Given an array with dimensions 1xN, returns the stride
    /// that should be used to turn it into array with dimensions matching this one.
    /// TODO(simonshields): write tests.
    pub fn stride(&self, array_length: usize) -> usize {
        if !self.inner.is_zero() && self.inner.is_fixed() {
            self.inner.max
        } else if !self.outer.is_zero() && self.outer.is_fixed() {
            if array_length % self.outer.max != 0 {
                array_length
            } else {
                self.outer.max
            }
        } else if !self.inner.has_value() {
            array_length
        } else {
            let mut matches = 0;
            let mut ret = 0;
            for dimension in self.inner.min..self.inner.max + 1 {
                if array_length % dimension == 0 {
                    matches += 1;
                    ret = dimension;
                }
            }

            if matches == 1 {
                ret
            } else {
                array_length
            }
        }
    }

    pub fn is_fixed(&self) -> bool {
        self.inner.is_fixed() || self.outer.is_fixed()
    }
}

#[cfg(test)]
mod tests {
    use super::Dimension;
    #[test]
    fn test_dimension_merge() {
        let a: Dimension = [[0, 0], [0, 0]].into();
        let b: Dimension = [[1, 1], [2, 3]].into();
        assert_eq!(a.merge(b), b);

        let a: Dimension = [[1, 4], [5, 9]].into();
        let b: Dimension = [[2, 5], [4, 7]].into();
        assert_eq!(a.merge(b), [[1, 5], [4, 9]].into());
    }

    #[test]
    fn test_stride_simple() {
        let a: Dimension = [[1, 1], [4, 4]].into();
        assert_eq!(a.stride(8), 4);

        let b: Dimension = [[2, 2], [1, 2]].into();
        assert_eq!(b.stride(4), 2);
        assert_eq!(b.stride(3), 3);
    }

    #[test]
    fn test_stride_variable() {
        let a: Dimension = [[1, 2], [3, 8]].into();
        assert_eq!(a.stride(10), 5);
        assert_eq!(a.stride(8), 8);
        assert_eq!(a.stride(7), 7);
        assert_eq!(a.stride(6), 6);
    }
}
