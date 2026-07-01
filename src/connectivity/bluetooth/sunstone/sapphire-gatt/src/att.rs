// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::num::NonZeroU16;
use thiserror::Error;
mod bearer;
mod l2cap;
pub mod pdu;

pub mod attribute;
pub mod client;
pub mod database;
pub mod server;

/// A valid, non-zero ATT Attribute Handle (0x0001 - 0xFFFF).
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AttributeHandle(NonZeroU16);

impl AttributeHandle {
    /// Creates a new `AttributeHandle` if the given value is non-zero.
    pub const fn new(value: u16) -> Option<Self> {
        if let Some(nonzero) = NonZeroU16::new(value) { Some(Self(nonzero)) } else { None }
    }

    /// Returns the raw `u16` value of this handle.
    pub const fn value(self) -> u16 {
        self.0.get()
    }
}

/// Error type for invalid handle conversions (e.g. converting 0).
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
#[error("Invalid attribute handle")]
pub struct InvalidAttributeHandle;

impl TryFrom<u16> for AttributeHandle {
    type Error = InvalidAttributeHandle;

    fn try_from(value: u16) -> Result<Self, Self::Error> {
        Self::new(value).ok_or(InvalidAttributeHandle)
    }
}

impl From<AttributeHandle> for u16 {
    fn from(handle: AttributeHandle) -> Self {
        handle.value()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::att::bearer::{BearerRx, BearerTx};
    use crate::att::client::Client;
    use crate::att::l2cap::mock::setup_mock_channel;
    use crate::att::server::{Server, ServerError};
    use sapphire_async::executor::BoundedExecutor;
    use sapphire_async::testing::TestExecutor;

    const CLIENT_PREFERRED_MTU: u16 = 512;
    const SERVER_MTU: u16 = 256;

    #[test]
    fn test_attribute_handle_new() {
        assert!(AttributeHandle::new(0).is_none());
        assert_eq!(AttributeHandle::new(1).unwrap().value(), 1);
        assert_eq!(AttributeHandle::new(0xFFFF).unwrap().value(), 0xFFFF);
    }

    #[test]
    fn test_attribute_handle_try_from() {
        assert_eq!(AttributeHandle::try_from(0), Err(InvalidAttributeHandle));
        assert_eq!(AttributeHandle::try_from(1).unwrap(), AttributeHandle::new(1).unwrap());
        assert_eq!(
            AttributeHandle::try_from(0xFFFF).unwrap(),
            AttributeHandle::new(0xFFFF).unwrap()
        );
    }

    #[test]
    fn test_attribute_handle_from() {
        let handle = AttributeHandle::new(42).unwrap();
        let value: u16 = u16::from(handle);
        assert_eq!(value, 42);
    }

    #[test]
    fn test_client_server_integration_handshake() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, test_tx, test_rx) = setup_mock_channel(executor);

            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                BearerRx::new(app_channel.receiver),
                CLIENT_PREFERRED_MTU,
            );

            let mut server =
                Server::new(BearerTx::new(test_tx), BearerRx::new(test_rx), SERVER_MTU);

            let server_handle = executor.spawn(async move {
                let res = server.run().await;
                assert_eq!(res, Err(ServerError::LinkClosed));
                assert_eq!(server.mtu(), SERVER_MTU);
            });

            let client_handle = executor.spawn(async move {
                client.exchange_mtu().await.unwrap();
                assert_eq!(client.mtu(), SERVER_MTU);
            });

            executor.run_until_stalled();

            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }
}
