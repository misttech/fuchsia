// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bt_common::PeerId;
use bt_common::core::{Address, AddressType};
use bt_gatt::pii::GetPeerAddr;
use bt_gatt::types::{Error, Result};
use fuchsia_sync::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct LocalPeerAddrCache {
    cache: Arc<Mutex<HashMap<PeerId, (Address, AddressType)>>>,
}

impl LocalPeerAddrCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_peer_address(&self, id: PeerId, addr: Address, addr_type: AddressType) {
        let mut cache = self.cache.lock();
        let _ = cache.insert(id, (addr, addr_type));
    }
}

impl GetPeerAddr for LocalPeerAddrCache {
    // Note: this is async because GetPeerAddr/get_peer_address returns a Future.
    // Returns the address in little endian order.
    async fn get_peer_address(&self, id: PeerId) -> Result<(Address, AddressType)> {
        self.cache.lock().get(&id).copied().ok_or(Error::PeerNotRecognized(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_async as fasync;

    #[fasync::run_singlethreaded(test)]
    async fn test_local_peer_addr_cache() {
        let cache = LocalPeerAddrCache::new();
        let peer_id = PeerId(123);
        let address = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        let address_type = AddressType::Public;

        // Initially, the address should not be found.
        assert!(cache.get_peer_address(peer_id).await.is_err());

        // Add the peer and verify it can be looked up.
        cache.set_peer_address(peer_id, address, address_type);
        let (found_addr, found_type) = cache.get_peer_address(peer_id).await.unwrap();
        assert_eq!(found_addr, address);
        assert_eq!(found_type, address_type);

        // Check a different peer ID.
        assert!(cache.get_peer_address(PeerId(456)).await.is_err());
    }
}
