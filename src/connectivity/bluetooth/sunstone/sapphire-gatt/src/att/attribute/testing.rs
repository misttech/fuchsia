// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::att::attribute::Attribute;
use crate::att::pdu::ErrorCode;
use sapphire_peer_cache::PeerId;
use sapphire_uuid::Uuid;

/// A mock implementation of the `Attribute` trait for unit and integration testing.
///
/// This mock is utilized by tests to verify GATT search and discovery procedures
/// without requiring production attribute implementations.
#[derive(Debug)]
pub struct MockAttribute {
    /// The Type UUID of the attribute.
    uuid: Uuid,
}

impl MockAttribute {
    /// Creates a new `MockAttribute` with the given UUID.
    pub fn new(uuid: Uuid) -> Self {
        Self { uuid }
    }
}

impl Attribute for MockAttribute {
    /// Returns the Type UUID of this attribute.
    fn uuid(&self) -> &Uuid {
        &self.uuid
    }

    /// Mock reading from the attribute.
    async fn read_chunk(
        &self,
        _peer_id: PeerId,
        _offset: u16,
        _buf: &mut [u8],
    ) -> Result<usize, ErrorCode> {
        todo!()
    }

    /// Mock writing to the attribute.
    async fn write_chunk(
        &self,
        _peer_id: PeerId,
        _offset: u16,
        _data: &[u8],
    ) -> Result<(), ErrorCode> {
        todo!()
    }
}
