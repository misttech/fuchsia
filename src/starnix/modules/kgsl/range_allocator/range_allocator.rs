// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// TODO(b/480141949): Transition to range-alloc once available.

//! A super-simple monotonically increasing range allocator.
pub struct Allocator {
    current: u64,
    end: u64,
}

impl Allocator {
    pub fn create(start: u64, size: u64) -> Self {
        let end = start.checked_add(size).expect("no overflow");
        Allocator { current: start, end }
    }

    pub fn allocate(&mut self, size: u64, align: u64) -> Option<u64> {
        if self.current >= self.end {
            return None;
        }
        let start = self.current.checked_next_multiple_of(align).expect("no overflow");
        let end = start.checked_add(size).expect("no overflow");
        if end > self.end {
            return None;
        }
        self.current = end;
        Some(start)
    }

    // This method is intentionally no-op. A more intelligent allocator
    // will be added in a future change.
    pub fn free(&mut self, _value: u64, _size: u64) -> Result<(), ()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn test_allocator() {
        let mut a = Allocator::create(0x1000, 0x1000);
        let v = a.allocate(0x100, 1).expect("can allocate");
        assert_eq!(v, 0x1000);
        let v = a.allocate(0x100, 1).expect("can allocate");
        assert_eq!(v, 0x1100);
        let v = a.allocate(0x100, 0x800).expect("can allocate");
        assert_eq!(v, 0x1800);
        let v = a.allocate(0x700, 1).expect("can allocate");
        assert_eq!(v, 0x1900);
        let v = a.allocate(0x100, 1);
        assert!(v.is_none());
        a.free(0x1000, 0x100).expect("can free");
    }
}
