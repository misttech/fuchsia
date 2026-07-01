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
    use sapphire_uuid::Uuid;

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

            let mut server = Server::new(
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
            let name_attr = MockAttribute::new(Uuid::from_u16(0x2A00)); // handle 1
            let custom_uuid =
                Uuid::from_le_bytes([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
            let custom_attr = MockAttribute::new(custom_uuid); // handle 2
            db.insert(h(1), name_attr);
            db.insert(h(2), custom_attr);

            let mut client = Client::new(
                BearerTx::new(app_channel.sender),
                BearerRx::new(app_channel.receiver),
                CLIENT_PREFERRED_MTU,
            );

            let mut server =
                Server::new(BearerTx::new(server_tx), BearerRx::new(server_rx), SERVER_MTU, db);

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
        use sapphire_uuid::Uuid;
        use zerocopy::TryFromBytes;
        use zerocopy::byteorder::little_endian::U16;

        fn setup_db() -> MockDb {
            let mut db = MockDb::new();
            // Handle 1: UUID 16-bit
            db.insert(h(1), MockAttribute::new(Uuid::from_u16(0x2A00)));
            // Handle 2: UUID 128-bit
            let custom_uuid =
                Uuid::from_le_bytes([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
            db.insert(h(2), MockAttribute::new(custom_uuid));
            // Handle 10: UUID 16-bit
            db.insert(h(10), MockAttribute::new(Uuid::from_u16(0x2A01)));
            // Handle 11: UUID 16-bit
            db.insert(h(11), MockAttribute::new(Uuid::from_u16(0x2A02)));
            // Handle 20: UUID 128-bit
            let custom_uuid2 = Uuid::from_le_bytes([
                10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
            ]);
            db.insert(h(20), MockAttribute::new(custom_uuid2));
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
                    let mut server = Server::new(BearerTx::new(server_tx), BearerRx::new(server_rx), SERVER_MTU, setup_db());
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
                    let mut server = Server::new(BearerTx::new(server_tx), BearerRx::new(server_rx), SERVER_MTU, setup_db());
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
                    let mut server = Server::new(BearerTx::new(server_tx), BearerRx::new(server_rx), SERVER_MTU, setup_db());
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
                    let mut server = Server::new(BearerTx::new(server_tx), BearerRx::new(server_rx), SERVER_MTU, setup_db());
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
        }
    }
}
