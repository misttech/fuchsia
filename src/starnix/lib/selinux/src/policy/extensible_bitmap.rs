// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub use crate::new_policy::bitmap::ExtensibleBitmap;

pub(super) const MAX_BITMAP_ITEMS: u32 = 0x40;
pub(super) const MAP_NODE_BITS: u32 = 64;

use super::parser::PolicyCursor;
use super::{Parse, PolicyValidationContext, Validate};

impl Parse for ExtensibleBitmap {
    type Error = anyhow::Error;

    fn parse<'a>(cursor: PolicyCursor<'a>) -> Result<(Self, PolicyCursor<'a>), Self::Error> {
        let offset = cursor.offset() as usize;
        let slice = &cursor.data().as_ref()[offset..];
        let mut new_cursor = crate::new_policy::parser::PolicyCursor::new(slice);
        let bitmap = <Self as crate::new_policy::traits::Parse>::parse(&mut new_cursor)
            .map_err(|e| anyhow::anyhow!("Parse error: {:?}", e))?;
        let bytes_parsed = new_cursor.offset();
        let new_offset = cursor.offset() + bytes_parsed as u32;
        Ok((bitmap, PolicyCursor::new_at(cursor.data(), new_offset)))
    }
}

impl Validate for ExtensibleBitmap {
    type Error = anyhow::Error;

    fn validate(&self, context: &PolicyValidationContext) -> Result<(), Self::Error> {
        crate::new_policy::traits::Validate::validate(self, &context.new_policy).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::parser::PolicyData;
    use std::sync::Arc;

    #[test]
    fn test_old_parse_compatibility() {
        let bytes = [
            64, 0, 0, 0, // map_item_size_bits = 64
            128, 0, 0, 0, // high_bit = 128
            2, 0, 0, 0, // count = 2
            // Item 1
            0, 0, 0, 0, // start_bit = 0
            5, 0, 0, 0, 0, 0, 0, 0, // map = 5 (bits 0 and 2 set)
            // Item 2
            64, 0, 0, 0, // start_bit = 64
            2, 0, 0, 0, 0, 0, 0, 0, // map = 2 (bit 65 set)
        ];
        let data: PolicyData = Arc::from(bytes);
        let cursor = PolicyCursor::new(&data);
        let (bitmap, tail) = ExtensibleBitmap::parse(cursor).unwrap();
        assert_eq!(tail.offset(), bytes.len() as u32);
        assert!(bitmap.is_set(0));
        assert!(!bitmap.is_set(1));
        assert!(bitmap.is_set(2));
        assert!(bitmap.is_set(65));
    }
}
