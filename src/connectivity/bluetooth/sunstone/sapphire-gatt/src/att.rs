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
    use crate::att::attribute::testing::MockAttribute;
    use crate::att::bearer::{BearerRx, BearerTx};
    use crate::att::client::{Client, DiscoveredInformation};
    use crate::att::database::testing::MockDb;
    use crate::att::l2cap::mock::setup_mock_channel;
    use crate::att::server::{Server, ServerError};
    use core::mem::MaybeUninit;
    use sapphire_async::executor::BoundedExecutor;
    use sapphire_async::testing::TestExecutor;
    use sapphire_peer_cache::PeerId;
    use sapphire_uuid::Uuid;

    const CLIENT_PREFERRED_MTU: u16 = 512;
    const SERVER_MTU: u16 = 256;
    const SMALL_TEST_MTU: u16 = 23;
    const READ_BLOB_OFFSET: u16 = 10;

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

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(test_tx),
                BearerRx::new(test_rx),
                SERVER_MTU,
                MockDb::new(),
            );

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

    fn h(val: u16) -> AttributeHandle {
        AttributeHandle::try_from(val).unwrap()
    }

    #[test]
    fn test_client_server_integration_find_information() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            let name_attr = MockAttribute::new(Uuid::from_u16(0x2A00), b"Sunstone"); // handle 1
            let custom_uuid =
                Uuid::from_le_bytes([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
            let custom_attr = MockAttribute::new(custom_uuid, b"Custom"); // handle 2
            db.insert(h(1), name_attr);
            db.insert(h(2), custom_attr);

            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                BearerRx::new(app_channel.receiver),
                CLIENT_PREFERRED_MTU,
            );

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(server_tx),
                BearerRx::new(server_rx),
                SERVER_MTU,
                db,
            );

            // Server task
            let server_handle = executor.spawn(async move {
                // 1. Process Handshake
                server.handle_request().await.unwrap();
                let negotiated = server.mtu();
                assert_eq!(negotiated, SERVER_MTU);

                // 2. Process Find Information request
                server.handle_request().await.unwrap();

                // 3. Process another request (for handle 2)
                server.handle_request().await.unwrap();
            });

            // Client task
            let client_handle = executor.spawn(async move {
                // 1. Handshake
                client.exchange_mtu().await.unwrap();
                let negotiated = client.mtu();
                assert_eq!(negotiated, SERVER_MTU);

                // 2. Discover descriptors starting from 1 to 2
                let mut rx_buf = [MaybeUninit::uninit(); 256];
                let info1 = client.find_information(h(1), h(2), &mut rx_buf).await.unwrap();
                match info1 {
                    DiscoveredInformation::Uuid16(entries) => {
                        assert_eq!(entries.len(), 1);
                        assert_eq!(entries[0].handle.get(), 1);
                        assert_eq!(entries[0].uuid, [0x00, 0x2a]);
                    }
                    _ => panic!("Expected Uuid16 discovered info"),
                }

                // 3. Discover descriptor for handle 2
                let mut rx_buf2 = [MaybeUninit::uninit(); 256];
                let info2 = client.find_information(h(2), h(2), &mut rx_buf2).await.unwrap();
                match info2 {
                    DiscoveredInformation::Uuid128(entries) => {
                        assert_eq!(entries.len(), 1);
                        assert_eq!(entries[0].handle.get(), 2);
                        assert_eq!(
                            entries[0].uuid,
                            [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
                        );
                    }
                    _ => panic!("Expected Uuid128 discovered info"),
                }
            });

            executor.run_until_stalled();
            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_server_integration_find_by_type_value() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            // Group 1: handles 1 to 5. Type 0x2800 (Primary Service), value 0x180D (Heart Rate Service)
            let svc1 = MockAttribute::new_grouped(Uuid::from_u16(0x2800), &[0x0D, 0x18], 5);
            let name = MockAttribute::new(Uuid::from_u16(0x2A00), b"Sunstone");
            // Group 2: handles 6 to 10. Type 0x2800, value 0x180A (Device Info Service)
            let svc2 = MockAttribute::new_grouped(Uuid::from_u16(0x2800), &[0x0A, 0x18], 10);

            db.insert(h(1), svc1);
            db.insert(h(2), name);
            db.insert(h(6), svc2);

            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                BearerRx::new(app_channel.receiver),
                CLIENT_PREFERRED_MTU,
            );

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(server_tx),
                BearerRx::new(server_rx),
                SERVER_MTU,
                db,
            );

            // Server task
            let server_handle = executor.spawn(async move {
                // 1. Handshake
                server.handle_request().await.unwrap();
                // 2. Find by Type Value (Group 1 query)
                server.handle_request().await.unwrap();
                // 3. Find by Type Value (Group 2 query)
                server.handle_request().await.unwrap();
            });

            // Client task
            let client_handle = executor.spawn(async move {
                client.exchange_mtu().await.unwrap();

                let mut rx_buf = [MaybeUninit::uninit(); 256];
                let results1 = client
                    .find_by_type_value(h(1), h(10), 0x2800, &[0x0D, 0x18], &mut rx_buf)
                    .await
                    .unwrap();
                assert_eq!(results1.len(), 1);
                assert_eq!(results1[0].attribute_handle.get(), 1);
                assert_eq!(results1[0].group_end_handle.get(), 5);

                let mut rx_buf2 = [MaybeUninit::uninit(); 256];
                let results2 = client
                    .find_by_type_value(h(1), h(10), 0x2800, &[0x0A, 0x18], &mut rx_buf2)
                    .await
                    .unwrap();
                assert_eq!(results2.len(), 1);
                assert_eq!(results2[0].attribute_handle.get(), 6);
                assert_eq!(results2[0].group_end_handle.get(), 10);
            });

            executor.run_until_stalled();
            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_server_integration_read() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            let long_val = b"012345678901234567890123456789"; // 30 bytes
            let name_attr = MockAttribute::new(Uuid::from_u16(0x2A00), long_val);
            db.insert(h(1), name_attr);

            // Server MTU is SMALL_TEST_MTU bytes (so max read response is SMALL_TEST_MTU - 1 bytes)
            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(server_tx),
                BearerRx::new(server_rx),
                SMALL_TEST_MTU,
                db,
            );

            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                BearerRx::new(app_channel.receiver),
                CLIENT_PREFERRED_MTU,
            );

            // Server task
            let server_handle = executor.spawn(async move {
                // 1. MTU Exchange
                server.handle_request().await.unwrap();
                // 2. Read Request
                server.handle_request().await.unwrap();
            });

            // Client task
            let client_handle = executor.spawn(async move {
                // 1. MTU Exchange (negotiated MTU will be SMALL_TEST_MTU, since server MTU is SMALL_TEST_MTU)
                client.exchange_mtu().await.unwrap();
                assert_eq!(client.mtu(), SMALL_TEST_MTU);

                // 2. Read Request
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let val = client.read(h(1), &mut rx_buf).await.unwrap();
                // Max read response is SMALL_TEST_MTU - 1 bytes
                assert_eq!(val, &long_val[..(SMALL_TEST_MTU - 1) as usize]);
            });

            executor.run_until_stalled();
            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_server_integration_read_entire_value() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            let short_val = b"Sunstone"; // 8 bytes
            let name_attr = MockAttribute::new(Uuid::from_u16(0x2A00), short_val);
            db.insert(h(1), name_attr);

            // Server MTU is SMALL_TEST_MTU bytes (so max read response is SMALL_TEST_MTU - 1 bytes)
            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(server_tx),
                BearerRx::new(server_rx),
                SMALL_TEST_MTU,
                db,
            );

            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                BearerRx::new(app_channel.receiver),
                CLIENT_PREFERRED_MTU,
            );

            // Server task
            let server_handle = executor.spawn(async move {
                // 1. MTU Exchange
                server.handle_request().await.unwrap();
                // 2. Read Request
                server.handle_request().await.unwrap();
            });

            // Client task
            let client_handle = executor.spawn(async move {
                // 1. MTU Exchange (negotiated MTU will be SMALL_TEST_MTU, since server MTU is SMALL_TEST_MTU)
                client.exchange_mtu().await.unwrap();
                assert_eq!(client.mtu(), SMALL_TEST_MTU);

                // 2. Read Request
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let val = client.read(h(1), &mut rx_buf).await.unwrap();

                // Value is smaller than SMALL_TEST_MTU - 1, so it is read entirely without truncation
                assert!(short_val.len() < (SMALL_TEST_MTU - 1) as usize);
                assert_eq!(val, short_val);
            });

            executor.run_until_stalled();
            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_server_integration_read_blob() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            let long_val = b"012345678901234567890123456789"; // 30 bytes
            let name_attr = MockAttribute::new(Uuid::from_u16(0x2A00), long_val);
            db.insert(h(1), name_attr);

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(server_tx),
                BearerRx::new(server_rx),
                SMALL_TEST_MTU,
                db,
            );

            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                BearerRx::new(app_channel.receiver),
                CLIENT_PREFERRED_MTU,
            );

            // Server task
            let server_handle = executor.spawn(async move {
                // 1. MTU Exchange
                server.handle_request().await.unwrap();
                // 2. Read Blob Request
                server.handle_request().await.unwrap();
            });

            // Client task
            let client_handle = executor.spawn(async move {
                // 1. MTU Exchange
                client.exchange_mtu().await.unwrap();

                // 2. Read Blob Request starting at offset
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let val = client.read_blob(h(1), READ_BLOB_OFFSET, &mut rx_buf).await.unwrap();
                // Remaining bytes fits in MTU - 1
                assert_eq!(val, &long_val[READ_BLOB_OFFSET as usize..]);
            });

            executor.run_until_stalled();
            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_server_integration_read_blob_truncated() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            let long_val = b"0123456789012345678901234567890123456789"; // 40 bytes
            let name_attr = MockAttribute::new(Uuid::from_u16(0x2A00), long_val);
            db.insert(h(1), name_attr);

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(server_tx),
                BearerRx::new(server_rx),
                SMALL_TEST_MTU,
                db,
            );

            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                BearerRx::new(app_channel.receiver),
                CLIENT_PREFERRED_MTU,
            );

            // Server task
            let server_handle = executor.spawn(async move {
                // 1. MTU Exchange
                server.handle_request().await.unwrap();
                // 2. Read Blob Request
                server.handle_request().await.unwrap();
            });

            // Client task
            let client_handle = executor.spawn(async move {
                // 1. MTU Exchange (negotiates MTU of 23)
                client.exchange_mtu().await.unwrap();
                assert_eq!(client.mtu(), SMALL_TEST_MTU);

                // 2. Read Blob Request starting at offset
                let mut rx_buf = [MaybeUninit::uninit(); 64];
                let val = client.read_blob(h(1), READ_BLOB_OFFSET, &mut rx_buf).await.unwrap();
                // Remaining bytes is truncated to MTU - 1
                let expected_len = (SMALL_TEST_MTU - 1) as usize;
                assert_eq!(val.len(), expected_len);
                assert_eq!(
                    val,
                    &long_val[READ_BLOB_OFFSET as usize..READ_BLOB_OFFSET as usize + expected_len]
                );
            });

            executor.run_until_stalled();
            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_server_integration_read_by_type() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            db.insert(h(2), MockAttribute::new(Uuid::from_u16(0x2A00), b"Sunstone"));
            db.insert(h(4), MockAttribute::new(Uuid::from_u16(0x2A00), b"Sapphire"));
            db.insert(h(6), MockAttribute::new(Uuid::from_u16(0x2A01), b"Other")); // different UUID
            db.insert(h(8), MockAttribute::new(Uuid::from_u16(0x2A00), b"Blue")); // different value size!

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(server_tx),
                BearerRx::new(server_rx),
                SERVER_MTU,
                db,
            );

            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                BearerRx::new(app_channel.receiver),
                CLIENT_PREFERRED_MTU,
            );

            // Server task
            let server_handle = executor.spawn(async move {
                // 1. MTU Exchange
                server.handle_request().await.unwrap();
                // 2. Read By Type Request
                server.handle_request().await.unwrap();
            });

            // Client task
            let client_handle = executor.spawn(async move {
                // 1. MTU Exchange
                client.exchange_mtu().await.unwrap();

                // 2. Read By Type Request
                let mut rx_buf = [MaybeUninit::uninit(); CLIENT_PREFERRED_MTU as usize];
                let uuid = Uuid::from_u16(0x2A00);
                let results = client.read_by_type(h(1), h(10), &uuid, &mut rx_buf).await.unwrap();

                let mut iter = results.iter();
                let e1 = iter.next().unwrap().unwrap();
                assert_eq!(e1.handle, h(2));
                assert_eq!(e1.value, b"Sunstone");

                let e2 = iter.next().unwrap().unwrap();
                assert_eq!(e2.handle, h(4));
                assert_eq!(e2.value, b"Sapphire");

                assert!(iter.next().is_none());
            });

            executor.run_until_stalled();
            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_server_integration_read_by_group_type() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            // Primary service declarations (grouping type 0x2800)
            db.insert(h(1), MockAttribute::new_grouped(Uuid::from_u16(0x2800), b"\x01\x18", 5)); // Service 0x1801 (Generic Attribute), ends at handle 5
            db.insert(h(6), MockAttribute::new_grouped(Uuid::from_u16(0x2800), b"\x00\x18", 10)); // Service 0x1800 (Generic Access), ends at handle 10
            db.insert(h(11), MockAttribute::new(Uuid::from_u16(0x2A00), b"Device Name")); // Non-grouped attribute

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(server_tx),
                BearerRx::new(server_rx),
                SERVER_MTU,
                db,
            );

            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                BearerRx::new(app_channel.receiver),
                CLIENT_PREFERRED_MTU,
            );

            // Server task
            let server_handle = executor.spawn(async move {
                // 1. MTU Exchange
                server.handle_request().await.unwrap();
                // 2. Read By Group Type Request
                server.handle_request().await.unwrap();
            });

            // Client task
            let client_handle = executor.spawn(async move {
                // 1. MTU Exchange
                client.exchange_mtu().await.unwrap();

                // 2. Read By Group Type Request
                let mut rx_buf = [MaybeUninit::uninit(); CLIENT_PREFERRED_MTU as usize];
                let group_uuid = Uuid::from_u16(0x2800);
                let results =
                    client.read_by_group_type(h(1), h(20), &group_uuid, &mut rx_buf).await.unwrap();

                let mut iter = results.iter();
                let e1 = iter.next().unwrap().unwrap();
                assert_eq!(e1.handle, h(1));
                assert_eq!(e1.end_group_handle, h(5));
                assert_eq!(e1.value, b"\x01\x18");

                let e2 = iter.next().unwrap().unwrap();
                assert_eq!(e2.handle, h(6));
                assert_eq!(e2.end_group_handle, h(10));
                assert_eq!(e2.value, b"\x00\x18");

                assert!(iter.next().is_none());
            });

            executor.run_until_stalled();
            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_server_integration_write() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            db.insert(h(10), MockAttribute::new(Uuid::from_u16(0x2A00), b"InitialValue"));

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(server_tx),
                BearerRx::new(server_rx),
                SERVER_MTU,
                db,
            );

            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                BearerRx::new(app_channel.receiver),
                CLIENT_PREFERRED_MTU,
            );

            // Server task
            let server_handle = executor.spawn(async move {
                // 1. MTU Exchange
                server.handle_request().await.unwrap();
                // 2. Write Request
                server.handle_request().await.unwrap();
                // 3. Read Request to verify value
                server.handle_request().await.unwrap();
            });

            // Client task
            let client_handle = executor.spawn(async move {
                // 1. MTU Exchange
                client.exchange_mtu().await.unwrap();

                // 2. Write Request
                let mut rx_buf = [MaybeUninit::uninit(); CLIENT_PREFERRED_MTU as usize];
                client.write(h(10), b"Sunstone", &mut rx_buf).await.unwrap();

                // 3. Read Request to verify the written value
                let read_res = client.read(h(10), &mut rx_buf).await.unwrap();
                assert_eq!(read_res, b"Sunstone");
            });

            executor.run_until_stalled();
            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    #[test]
    fn test_client_server_integration_write_command() {
        BoundedExecutor::new(TestExecutor::new(), |executor| {
            let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);

            let mut db = MockDb::new();
            db.insert(h(10), MockAttribute::new(Uuid::from_u16(0x2A00), b"InitialValue"));

            let mut server = Server::new(
                PeerId::new(1).unwrap(),
                BearerTx::new(server_tx),
                BearerRx::new(server_rx),
                SERVER_MTU,
                db,
            );

            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                BearerRx::new(app_channel.receiver),
                CLIENT_PREFERRED_MTU,
            );

            // Server task
            let server_handle = executor.spawn(async move {
                // 1. MTU Exchange
                server.handle_request().await.unwrap();
                // 2. Write Command
                server.handle_request().await.unwrap();
                // 3. Read Request
                server.handle_request().await.unwrap();
            });

            // Client task
            let client_handle = executor.spawn(async move {
                // 1. MTU Exchange
                client.exchange_mtu().await.unwrap();

                // 2. Write Command
                client.write_command(h(10), b"SunstoneCmd").await.unwrap();

                // 3. Read Request to verify the written value
                let mut rx_buf = [MaybeUninit::uninit(); CLIENT_PREFERRED_MTU as usize];
                let read_res = client.read(h(10), &mut rx_buf).await.unwrap();
                assert_eq!(read_res, b"SunstoneCmd");
            });

            executor.run_until_stalled();
            assert!(server_handle.is_finished());
            assert!(client_handle.is_finished());
        });
    }

    mod proptests {
        use super::*;
        use crate::att::attribute::Attribute;
        use crate::att::attribute::testing::MockAttribute;
        use crate::att::client::{ClientError, DiscoveredInformation};
        use crate::att::database::Database;
        use crate::att::pdu::{
            ErrorCode, ErrorRsp, FindInformationReq, Header, Opcode, PacketBuilder, UuidFormat,
        };
        use core::mem::MaybeUninit;
        use proptest::prelude::*;
        use sapphire_peer_cache::PeerId;
        use sapphire_uuid::Uuid;
        use zerocopy::TryFromBytes;
        use zerocopy::byteorder::little_endian::U16;

        fn setup_db() -> MockDb {
            let mut db = MockDb::new();
            // Handle 1: UUID 16-bit
            db.insert(h(1), MockAttribute::new(Uuid::from_u16(0x2A00), b"Value1"));
            // Handle 2: UUID 128-bit
            let custom_uuid =
                Uuid::from_le_bytes([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
            db.insert(h(2), MockAttribute::new(custom_uuid, b"Value2"));
            // Handle 10: UUID 16-bit
            db.insert(h(10), MockAttribute::new(Uuid::from_u16(0x2A01), b"Value10"));
            // Handle 11: UUID 16-bit
            db.insert(h(11), MockAttribute::new(Uuid::from_u16(0x2A02), b"Value11"));
            // Handle 20: UUID 128-bit
            let custom_uuid2 = Uuid::from_le_bytes([
                10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
            ]);
            db.insert(h(20), MockAttribute::new(custom_uuid2, b"Value20"));
            db
        }

        fn setup_group_db() -> MockDb {
            let mut db = MockDb::new();
            // Primary service declarations (grouping type 0x2800)
            db.insert(h(1), MockAttribute::new_grouped(Uuid::from_u16(0x2800), b"\x01\x18", 5));
            db.insert(h(6), MockAttribute::new_grouped(Uuid::from_u16(0x2800), b"\x00\x18", 10));
            // Non-grouped attribute
            db.insert(h(11), MockAttribute::new(Uuid::from_u16(0x2A00), b"Device Name"));
            db
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(100))]

            #[test]
            fn test_find_information_invalid_handles_zero(
                handle in 1..=0xFFFFu16,
            ) {
                BoundedExecutor::new(TestExecutor::new(), |executor| {
                    let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);
                    let mut server = Server::new(
                        PeerId::new(1).unwrap(),
                        BearerTx::new(server_tx),
                        BearerRx::new(server_rx),
                        SERVER_MTU,
                        setup_db(),
                    );
                    let server_handle = executor.spawn(async move {
                        let _ = server.run().await;
                    });
                    let client_handle = executor.spawn(async move {
                        let mut rx_buf = [MaybeUninit::uninit(); 512];
                        let mut client_tx_bearer = BearerTx::new(app_channel.sender);
                        let mut client_rx_bearer = BearerRx::new(app_channel.receiver);

                        // Test starting handle = 0
                        let builder = PacketBuilder {
                            header: Header { opcode: Opcode::FindInformationReq },
                            payload: FindInformationReq {
                                starting_handle: U16::new(0),
                                ending_handle: U16::new(handle),
                            },
                        };
                        client_tx_bearer.send(builder.as_packet()).await.unwrap();
                        let packet = client_rx_bearer.next_packet(&mut rx_buf).await.unwrap();
                        assert_eq!(packet.header.opcode, Opcode::ErrorRsp);
                        let err = ErrorRsp::try_read_from_bytes(&packet.data[..]).unwrap();
                        assert_eq!(err.error_code, ErrorCode::InvalidHandle);

                        let mut rx_buf2 = [MaybeUninit::uninit(); 512];
                        // Test ending handle = 0
                        let builder2 = PacketBuilder {
                            header: Header { opcode: Opcode::FindInformationReq },
                            payload: FindInformationReq {
                                starting_handle: U16::new(handle),
                                ending_handle: U16::new(0),
                            },
                        };
                        client_tx_bearer.send(builder2.as_packet()).await.unwrap();
                        let packet2 = client_rx_bearer.next_packet(&mut rx_buf2).await.unwrap();
                        assert_eq!(packet2.header.opcode, Opcode::ErrorRsp);
                        let err2 = ErrorRsp::try_read_from_bytes(&packet2.data[..]).unwrap();
                        assert_eq!(err2.error_code, ErrorCode::InvalidHandle);
                    });

                    executor.run_until_stalled();
                    assert!(client_handle.is_finished());
                    assert!(server_handle.is_finished());
                });
            }

            #[test]
            fn test_find_information_end_handle_smaller_than_start_handle(
                (start, end) in (2..=0xFFFFu16).prop_flat_map(|s| {
                    (Just(AttributeHandle::new(s).unwrap()), (1..s).prop_map(|e| AttributeHandle::new(e).unwrap()))
                })
            ) {
                BoundedExecutor::new(TestExecutor::new(), |executor| {
                    let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);
                    let mut client = Client::new(BearerTx::new(app_channel.sender), BearerRx::new(app_channel.receiver), CLIENT_PREFERRED_MTU);
                    let mut server = Server::new(
                        PeerId::new(1).unwrap(),
                        BearerTx::new(server_tx),
                        BearerRx::new(server_rx),
                        SERVER_MTU,
                        setup_db(),
                    );
                    let server_handle = executor.spawn(async move {
                        let _ = server.run().await;
                    });
                    let client_handle = executor.spawn(async move {
                        let mut rx_buf = [MaybeUninit::uninit(); 512];
                        let result = client.find_information(start, end, &mut rx_buf).await;
                        assert_eq!(result, Err(ClientError::ErrorResponse(ErrorCode::InvalidHandle)));
                    });

                    executor.run_until_stalled();
                    assert!(client_handle.is_finished());
                    assert!(server_handle.is_finished());
                });
            }

            #[test]
            fn test_find_information_response_consistency(
                start in (1..=0xFFFFu16).prop_map(|v| AttributeHandle::new(v).unwrap()),
                end in (1..=0xFFFFu16).prop_map(|v| AttributeHandle::new(v).unwrap()),
            ) {
                BoundedExecutor::new(TestExecutor::new(), |executor| {
                    let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);
                    let mut client = Client::new(BearerTx::new(app_channel.sender), BearerRx::new(app_channel.receiver), CLIENT_PREFERRED_MTU);
                    let mut server = Server::new(
                        PeerId::new(1).unwrap(),
                        BearerTx::new(server_tx),
                        BearerRx::new(server_rx),
                        SERVER_MTU,
                        setup_db(),
                    );
                    let server_handle = executor.spawn(async move {
                        let _ = server.run().await;
                    });
                    let client_handle = executor.spawn(async move {
                        let mut rx_buf1 = [MaybeUninit::uninit(); 512];
                        let result1 = client.find_information(start, end, &mut rx_buf1).await;

                        let mut rx_buf2 = [MaybeUninit::uninit(); 512];
                        let result2 = client.find_information(start, end, &mut rx_buf2).await;

                        assert_eq!(result1, result2);
                    });

                    executor.run_until_stalled();
                    assert!(client_handle.is_finished());
                    assert!(server_handle.is_finished());
                });
            }

            #[test]
            fn test_find_information_valid_range_contents(
                (start, end) in (1..=0xFFFFu16).prop_flat_map(|s| {
                    (Just(AttributeHandle::new(s).unwrap()), (s..=0xFFFFu16).prop_map(|e| AttributeHandle::new(e).unwrap()))
                })
            ) {
                BoundedExecutor::new(TestExecutor::new(), |executor| {
                    let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);
                    let mut client = Client::new(BearerTx::new(app_channel.sender), BearerRx::new(app_channel.receiver), CLIENT_PREFERRED_MTU);
                    let mut server = Server::new(
                        PeerId::new(1).unwrap(),
                        BearerTx::new(server_tx),
                        BearerRx::new(server_rx),
                        SERVER_MTU,
                        setup_db(),
                    );
                    let server_handle = executor.spawn(async move {
                        let _ = server.run().await;
                    });
                    let db = setup_db();
                    let client_handle = executor.spawn(async move {
                        let mut rx_buf = [MaybeUninit::uninit(); 512];
                        let result = client.find_information(start, end, &mut rx_buf).await;

                        match result {
                            Ok(DiscoveredInformation::Uuid16(entries)) => {
                                assert!(!entries.is_empty());
                                for entry in entries {
                                    let h = entry.handle.get();
                                    assert!(h >= start.value() && h <= end.value());
                                    let handle = AttributeHandle::try_from(h).unwrap();
                                    let attr = db.find_attribute(handle).expect("attribute must exist in db");
                                    assert_eq!(UuidFormat::from(*attr.uuid()), UuidFormat::Uuid16);
                                }
                            }
                            Ok(DiscoveredInformation::Uuid128(entries)) => {
                                assert!(!entries.is_empty());
                                for entry in entries {
                                    let h = entry.handle.get();
                                    assert!(h >= start.value() && h <= end.value());
                                    let handle = AttributeHandle::try_from(h).unwrap();
                                    let attr = db.find_attribute(handle).expect("attribute must exist in db");
                                    assert_eq!(UuidFormat::from(*attr.uuid()), UuidFormat::Uuid128);
                                }
                            }
                            Err(ClientError::ErrorResponse(ErrorCode::AttributeNotFound)) => {
                                assert!(!db.has_attributes_in_range(start.value(), end.value()));
                            }
                            other => panic!("Unexpected result: {:?}", other),
                        }
                    });

                    executor.run_until_stalled();
                    assert!(client_handle.is_finished());
                    assert!(server_handle.is_finished());
                });
            }

            #[test]
            fn test_find_by_type_value_invalid_ranges(
                (start, end) in (2..=0xFFFFu16).prop_flat_map(|s| {
                    (Just(AttributeHandle::new(s).unwrap()), (1..s).prop_map(|e| AttributeHandle::new(e).unwrap()))
                }),
                target_type in 0..=0xFFFFu16,
                target_value in prop::collection::vec(any::<u8>(), 0..20),
            ) {
                let target_value: Vec<u8> = target_value;
                BoundedExecutor::new(TestExecutor::new(), |executor| {
                    let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);
                    let mut client = Client::new(BearerTx::new(app_channel.sender), BearerRx::new(app_channel.receiver), CLIENT_PREFERRED_MTU);
                    let mut server = Server::new(
                        PeerId::new(1).unwrap(),
                        BearerTx::new(server_tx),
                        BearerRx::new(server_rx),
                        SERVER_MTU,
                        setup_db(),
                    );
                    let server_handle = executor.spawn(async move {
                        let _ = server.run().await;
                    });
                    let client_handle = executor.spawn(async move {
                        client.exchange_mtu().await.unwrap();
                        let mut rx_buf = [MaybeUninit::uninit(); 512];
                        let result = client.find_by_type_value(start, end, target_type, &target_value, &mut rx_buf).await;
                        assert_eq!(result, Err(ClientError::ErrorResponse(ErrorCode::InvalidHandle)));
                    });

                    executor.run_until_stalled();
                    assert!(client_handle.is_finished());
                    assert!(server_handle.is_finished());
                });
            }

            #[test]
            fn test_find_by_type_value_response_consistency(
                start in (1..=0xFFFFu16).prop_map(|v| AttributeHandle::new(v).unwrap()),
                end in (1..=0xFFFFu16).prop_map(|v| AttributeHandle::new(v).unwrap()),
                target_type in 0..=0xFFFFu16,
                target_value in prop::collection::vec(any::<u8>(), 0..20),
            ) {
                let target_value: Vec<u8> = target_value;
                BoundedExecutor::new(TestExecutor::new(), |executor| {
                    let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);
                    let mut client = Client::new(BearerTx::new(app_channel.sender), BearerRx::new(app_channel.receiver), CLIENT_PREFERRED_MTU);
                    let mut server = Server::new(
                        PeerId::new(1).unwrap(),
                        BearerTx::new(server_tx),
                        BearerRx::new(server_rx),
                        SERVER_MTU,
                        setup_db(),
                    );
                    let server_handle = executor.spawn(async move {
                        let _ = server.run().await;
                    });
                    let client_handle = executor.spawn(async move {
                        client.exchange_mtu().await.unwrap();
                        let mut rx_buf1 = [MaybeUninit::uninit(); 512];
                        let result1 = client.find_by_type_value(start, end, target_type, &target_value, &mut rx_buf1).await;

                        let mut rx_buf2 = [MaybeUninit::uninit(); 512];
                        let result2 = client.find_by_type_value(start, end, target_type, &target_value, &mut rx_buf2).await;

                        assert_eq!(result1, result2);
                    });

                    executor.run_until_stalled();
                    assert!(client_handle.is_finished());
                    assert!(server_handle.is_finished());
                });
            }

            #[test]
            fn test_find_by_type_value_valid_range(
                (start, end) in (1..=0xFFFFu16).prop_flat_map(|s| {
                    (Just(AttributeHandle::new(s).unwrap()), (s..=0xFFFFu16).prop_map(|e| AttributeHandle::new(e).unwrap()))
                }),
                use_existing in proptest::bool::weighted(0.5),
                random_type in 0..=0xFFFFu16,
                random_value in prop::collection::vec(any::<u8>(), 0..20),
            ) {
                let random_value: Vec<u8> = random_value;
                BoundedExecutor::new(TestExecutor::new(), |executor| {
                    let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);
                    let mut client = Client::new(BearerTx::new(app_channel.sender), BearerRx::new(app_channel.receiver), CLIENT_PREFERRED_MTU);
                    let mut server = Server::new(
                        PeerId::new(1).unwrap(),
                        BearerTx::new(server_tx),
                        BearerRx::new(server_rx),
                        SERVER_MTU,
                        setup_db(),
                    );
                    let server_handle = executor.spawn(async move {
                        let _ = server.run().await;
                    });

                    let (target_type, target_value) = if use_existing {
                        (0x2A01u16, b"Value10".to_vec())
                    } else {
                        (random_type, random_value)
                    };

                    let db = setup_db();
                    let client_handle = executor.spawn(async move {
                        client.exchange_mtu().await.unwrap();
                        let mut rx_buf = [MaybeUninit::uninit(); 512];
                        let result = client
                            .find_by_type_value(start, end, target_type, &target_value, &mut rx_buf)
                            .await;

                        match result {
                            Ok(entries) => {
                                assert!(!entries.is_empty());
                                for entry in entries {
                                    let h = entry.attribute_handle.get();
                                    let group_end = entry.group_end_handle.get();
                                    assert!(h >= start.value() && h <= end.value());
                                    assert!(group_end >= h && group_end <= end.value());

                                    let handle = AttributeHandle::try_from(h).unwrap();
                                    let attr = db.find_attribute(handle).expect("attribute must exist in db");
                                    let matches_type = if let Ok(bytes16) = <[u8; 2]>::try_from(*attr.uuid()) {
                                        u16::from_le_bytes(bytes16) == target_type
                                    } else {
                                        false
                                    };
                                    assert!(matches_type);
                                }
                            }
                            Err(ClientError::ErrorResponse(ErrorCode::AttributeNotFound)) => {
                                // Verify that no attributes in the range match the type and value.
                                // Handle 1: type 0x2A00, value b"Value1"
                                // Handle 10: type 0x2A01, value b"Value10"
                                // Handle 11: type 0x2A02, value b"Value11"
                                let matches_1 = 1 >= start.value() && 1 <= end.value() && target_type == 0x2A00 && target_value == b"Value1";
                                let matches_10 = 10 >= start.value() && 10 <= end.value() && target_type == 0x2A01 && target_value == b"Value10";
                                let matches_11 = 11 >= start.value() && 11 <= end.value() && target_type == 0x2A02 && target_value == b"Value11";
                                assert!(!(matches_1 || matches_10 || matches_11));
                            }
                            other => panic!("Unexpected result: {:?}", other),
                        }
                    });

                    executor.run_until_stalled();
                    assert!(client_handle.is_finished());
                    assert!(server_handle.is_finished());
                });
            }

            #[test]
            fn test_read_by_type_invalid_ranges(
                (start, end) in (2..=0xFFFFu16).prop_flat_map(|s| {
                    (Just(AttributeHandle::new(s).unwrap()), (1..s).prop_map(|e| AttributeHandle::new(e).unwrap()))
                }),
                random_uuid_16 in 0..=0xFFFFu16,
            ) {
                BoundedExecutor::new(TestExecutor::new(), |executor| {
                    let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);
                    let mut client = Client::new(BearerTx::new(app_channel.sender), BearerRx::new(app_channel.receiver), CLIENT_PREFERRED_MTU);
                    let mut server = Server::new(
                        PeerId::new(1).unwrap(),
                        BearerTx::new(server_tx),
                        BearerRx::new(server_rx),
                        SERVER_MTU,
                        setup_db(),
                    );
                    let server_handle = executor.spawn(async move {
                        let _ = server.run().await;
                    });
                    let client_handle = executor.spawn(async move {
                        client.exchange_mtu().await.unwrap();
                        let mut rx_buf = [MaybeUninit::uninit(); 512];
                        let uuid = Uuid::from_u16(random_uuid_16);
                        let result = client.read_by_type(start, end, &uuid, &mut rx_buf).await;
                        assert_eq!(result.err(), Some(ClientError::ErrorResponse(ErrorCode::InvalidHandle)));
                    });

                    executor.run_until_stalled();
                    assert!(client_handle.is_finished());
                    assert!(server_handle.is_finished());
                });
            }

            #[test]
            fn test_read_by_type_response_consistency(
                start in (1..=0xFFFFu16).prop_map(|v| AttributeHandle::new(v).unwrap()),
                end in (1..=0xFFFFu16).prop_map(|v| AttributeHandle::new(v).unwrap()),
                random_uuid_16 in 0..=0xFFFFu16,
            ) {
                BoundedExecutor::new(TestExecutor::new(), |executor| {
                    let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);
                    let mut client = Client::new(BearerTx::new(app_channel.sender), BearerRx::new(app_channel.receiver), CLIENT_PREFERRED_MTU);
                    let mut server = Server::new(
                        PeerId::new(1).unwrap(),
                        BearerTx::new(server_tx),
                        BearerRx::new(server_rx),
                        SERVER_MTU,
                        setup_db(),
                    );
                    let server_handle = executor.spawn(async move {
                        let _ = server.run().await;
                    });
                    let client_handle = executor.spawn(async move {
                        client.exchange_mtu().await.unwrap();
                        let uuid = Uuid::from_u16(random_uuid_16);
                        let mut rx_buf1 = [MaybeUninit::uninit(); 512];
                        let result1 = client.read_by_type(start, end, &uuid, &mut rx_buf1).await;

                        let mut rx_buf2 = [MaybeUninit::uninit(); 512];
                        let result2 = client.read_by_type(start, end, &uuid, &mut rx_buf2).await;

                        let r1 = result1.as_ref().map(|res| res.iter().collect::<Vec<_>>());
                        let r2 = result2.as_ref().map(|res| res.iter().collect::<Vec<_>>());
                        assert_eq!(r1, r2);
                    });

                    executor.run_until_stalled();
                    assert!(client_handle.is_finished());
                    assert!(server_handle.is_finished());
                });
            }

            #[test]
            fn test_read_by_type_valid_range(
                (start, end) in (1..=0xFFFFu16).prop_flat_map(|s| {
                    (Just(AttributeHandle::new(s).unwrap()), (s..=0xFFFFu16).prop_map(|e| AttributeHandle::new(e).unwrap()))
                }),
                use_existing in proptest::bool::weighted(0.5),
                random_uuid_16 in 0..=0xFFFFu16,
            ) {
                BoundedExecutor::new(TestExecutor::new(), |executor| {
                    let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);
                    let mut client = Client::new(BearerTx::new(app_channel.sender), BearerRx::new(app_channel.receiver), CLIENT_PREFERRED_MTU);
                    let mut server = Server::new(
                        PeerId::new(1).unwrap(),
                        BearerTx::new(server_tx),
                        BearerRx::new(server_rx),
                        SERVER_MTU,
                        setup_db(),
                    );
                    let server_handle = executor.spawn(async move {
                        let _ = server.run().await;
                    });

                    let target_uuid = if use_existing {
                        Uuid::from_u16(0x2A01)
                    } else {
                        Uuid::from_u16(random_uuid_16)
                    };

                    let db = setup_db();
                    let client_handle = executor.spawn(async move {
                        client.exchange_mtu().await.unwrap();
                        let mut rx_buf = [MaybeUninit::uninit(); 512];
                        let result = client
                            .read_by_type(start, end, &target_uuid, &mut rx_buf)
                            .await;

                        match result {
                            Ok(results) => {
                                let mut entries = results.iter();
                                let mut count = 0;
                                while let Some(entry_res) = entries.next() {
                                    let entry = entry_res.unwrap();
                                    count += 1;
                                    let h = entry.handle.value();
                                    assert!(h >= start.value() && h <= end.value());

                                    let handle = AttributeHandle::try_from(h).unwrap();
                                    let attr = db.find_attribute(handle).expect("attribute must exist in db");
                                    assert_eq!(attr.uuid(), &target_uuid);

                                    // Verify value matches DB
                                    let mut db_val = [0u8; 64];
                                    let db_val_len = attr.read_chunk(PeerId::new(1).unwrap(), 0, &mut db_val).await.unwrap();
                                    assert_eq!(entry.value, &db_val[..db_val_len]);
                                }
                                assert!(count > 0);
                            }
                            Err(ClientError::ErrorResponse(ErrorCode::AttributeNotFound)) => {
                                // Verify that indeed no attributes in range [start, end] match target_uuid
                                for h_val in start.value()..=end.value() {
                                    if let Some(handle) = AttributeHandle::new(h_val) {
                                        if let Some(attr) = db.find_attribute(handle) {
                                            assert_ne!(attr.uuid(), &target_uuid);
                                        }
                                    }
                                }
                            }
                            other => panic!("Unexpected result: {:?}", other),
                        }
                    });

                    executor.run_until_stalled();
                    assert!(client_handle.is_finished());
                    assert!(server_handle.is_finished());
                });
            }

            #[test]
            fn test_read_by_group_type_invalid_ranges(
                (start, end) in (2..=0xFFFFu16).prop_flat_map(|s| {
                    (Just(AttributeHandle::new(s).unwrap()), (1..s).prop_map(|e| AttributeHandle::new(e).unwrap()))
                }),
                random_uuid_16 in 0..=0xFFFFu16,
            ) {
                BoundedExecutor::new(TestExecutor::new(), |executor| {
                    let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);
                    let mut client = Client::new(BearerTx::new(app_channel.sender), BearerRx::new(app_channel.receiver), CLIENT_PREFERRED_MTU);
                    let mut server = Server::new(
                        PeerId::new(1).unwrap(),
                        BearerTx::new(server_tx),
                        BearerRx::new(server_rx),
                        SERVER_MTU,
                        setup_group_db(),
                    );
                    let server_handle = executor.spawn(async move {
                        let _ = server.run().await;
                    });
                    let client_handle = executor.spawn(async move {
                        client.exchange_mtu().await.unwrap();
                        let mut rx_buf = [MaybeUninit::uninit(); 512];
                        let uuid = Uuid::from_u16(random_uuid_16);
                        let result = client.read_by_group_type(start, end, &uuid, &mut rx_buf).await;
                        assert_eq!(result.err(), Some(ClientError::ErrorResponse(ErrorCode::InvalidHandle)));
                    });

                    executor.run_until_stalled();
                    assert!(client_handle.is_finished());
                    assert!(server_handle.is_finished());
                });
            }

            #[test]
            fn test_read_by_group_type_response_consistency(
                start in (1..=0xFFFFu16).prop_map(|v| AttributeHandle::new(v).unwrap()),
                end in (1..=0xFFFFu16).prop_map(|v| AttributeHandle::new(v).unwrap()),
                random_uuid_16 in 0..=0xFFFFu16,
            ) {
                BoundedExecutor::new(TestExecutor::new(), |executor| {
                    let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);
                    let mut client = Client::new(BearerTx::new(app_channel.sender), BearerRx::new(app_channel.receiver), CLIENT_PREFERRED_MTU);
                    let mut server = Server::new(
                        PeerId::new(1).unwrap(),
                        BearerTx::new(server_tx),
                        BearerRx::new(server_rx),
                        SERVER_MTU,
                        setup_group_db(),
                    );
                    let server_handle = executor.spawn(async move {
                        let _ = server.run().await;
                    });
                    let client_handle = executor.spawn(async move {
                        client.exchange_mtu().await.unwrap();
                        let uuid = Uuid::from_u16(random_uuid_16);
                        let mut rx_buf1 = [MaybeUninit::uninit(); 512];
                        let result1 = client.read_by_group_type(start, end, &uuid, &mut rx_buf1).await;

                        let mut rx_buf2 = [MaybeUninit::uninit(); 512];
                        let result2 = client.read_by_group_type(start, end, &uuid, &mut rx_buf2).await;

                        let r1 = result1.map(|res| res.iter().map(|e| {
                            let e = e.unwrap();
                            (e.handle.value(), e.end_group_handle.value(), e.value.to_vec())
                        }).collect::<Vec<_>>());
                        let r2 = result2.map(|res| res.iter().map(|e| {
                            let e = e.unwrap();
                            (e.handle.value(), e.end_group_handle.value(), e.value.to_vec())
                        }).collect::<Vec<_>>());
                        assert_eq!(r1, r2);
                    });

                    executor.run_until_stalled();
                    assert!(client_handle.is_finished());
                    assert!(server_handle.is_finished());
                });
            }

            #[test]
            fn test_read_by_group_type_valid_range(
                (start, end) in (1..=0xFFFFu16).prop_flat_map(|s| {
                    (Just(AttributeHandle::new(s).unwrap()), (s..=0xFFFFu16).prop_map(|e| AttributeHandle::new(e).unwrap()))
                }),
                use_existing in proptest::bool::weighted(0.5),
                random_uuid_16 in 0..=0xFFFFu16,
            ) {
                BoundedExecutor::new(TestExecutor::new(), |executor| {
                    let (app_channel, server_tx, server_rx) = setup_mock_channel(executor);
                    let mut client = Client::new(BearerTx::new(app_channel.sender), BearerRx::new(app_channel.receiver), CLIENT_PREFERRED_MTU);
                    let mut server = Server::new(
                        PeerId::new(1).unwrap(),
                        BearerTx::new(server_tx),
                        BearerRx::new(server_rx),
                        SERVER_MTU,
                        setup_group_db(),
                    );
                    let server_handle = executor.spawn(async move {
                        let _ = server.run().await;
                    });

                    let target_uuid = if use_existing {
                        Uuid::from_u16(0x2800) // Primary Service UUID
                    } else {
                        Uuid::from_u16(random_uuid_16)
                    };

                    let db = setup_group_db();
                    let client_handle = executor.spawn(async move {
                        client.exchange_mtu().await.unwrap();
                        let mut rx_buf = [MaybeUninit::uninit(); 512];
                        let result = client
                            .read_by_group_type(start, end, &target_uuid, &mut rx_buf)
                            .await;

                        match result {
                            Ok(results) => {
                                let mut entries = results.iter();
                                let mut count = 0;
                                while let Some(entry) = entries.next() {
                                    let entry = entry.unwrap();
                                    count += 1;
                                    let h = entry.handle.value();
                                    assert!(h >= start.value() && h <= end.value());

                                    let handle = AttributeHandle::try_from(h).unwrap();
                                    let attr = db.find_attribute(handle).expect("attribute must exist in db");
                                    assert_eq!(attr.uuid(), &target_uuid);
                                    assert_eq!(entry.end_group_handle.value(), attr.group_end_handle().unwrap());

                                    let mut db_val = [0u8; 64];
                                    let db_val_len = attr.read_chunk(PeerId::new(1).unwrap(), 0, &mut db_val).await.unwrap();
                                    assert_eq!(entry.value, &db_val[..db_val_len]);
                                }
                                assert!(count > 0);
                            }
                            Err(ClientError::ErrorResponse(ErrorCode::AttributeNotFound)) => {
                                for h_val in start.value()..=end.value() {
                                    if let Some(handle) = AttributeHandle::new(h_val) {
                                        if let Some(attr) = db.find_attribute(handle) {
                                            assert_ne!(attr.uuid(), &target_uuid);
                                        }
                                    }
                                }
                            }
                            Err(ClientError::ErrorResponse(ErrorCode::UnsupportedGroupType)) => {
                                let mut has_non_grouping = false;
                                for h_val in start.value()..=end.value() {
                                    if let Some(handle) = AttributeHandle::new(h_val) {
                                        if let Some(attr) = db.find_attribute(handle) {
                                            if attr.uuid() == &target_uuid && attr.group_end_handle().is_none() {
                                                has_non_grouping = true;
                                                break;
                                            }
                                        }
                                    }
                                }
                                assert!(has_non_grouping);
                            }
                            other => panic!("Unexpected result: {:?}", other),
                        }
                    });

                    executor.run_until_stalled();
                    assert!(client_handle.is_finished());
                    assert!(server_handle.is_finished());
                });
            }
        }
    }
}
