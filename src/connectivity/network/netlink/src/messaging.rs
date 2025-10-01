// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A module for managing message passing between Netlink and its clients.

use futures::Stream;
use netlink_packet_core::{NetlinkMessage, NetlinkPayload, NetlinkSerializable};
use std::fmt::Debug;

use crate::multicast_groups::ModernGroup;
use crate::netlink_packet;
use crate::netlink_packet::errno::Errno;

/// A type capable of sending messages, `M`, from Netlink to a client.
pub trait Sender<M>: Clone + Send + Sync {
    /// Sends the given message to the client.
    ///
    /// If the message is a multicast, `group` will hold a `Some`; `None` for
    /// unicast messages.
    ///
    /// Implementors must ensure this call does not block.
    fn send(&mut self, message: NetlinkMessage<M>, group: Option<ModernGroup>);
}

/// A type capable of receiving messages, `M`, from a client to Netlink.
///
/// [`Stream`] already provides a sufficient interface for this purpose.
pub trait Receiver<M, C>: Stream<Item = NetlinkMessageWithCreds<M, C>> + Send
where
    M: Send + MessageWithPermission,
    C: Send,
{
}

/// Blanket implementation allows any [`Stream`] to be used as a [`Receiver`].
impl<M, C, S> Receiver<M, C> for S
where
    M: Send + MessageWithPermission,
    C: Send,
    S: Stream<Item = NetlinkMessageWithCreds<M, C>> + Send,
{
}

/// A permission that is required from the sender when processing a netlink
/// request.
pub enum Permission {
    /// GET messages in NETLINK_ROUTE.
    NetlinkRouteRead,

    /// SET/NEW/DELETE messages in NETLINK_ROUTE.
    NetlinkRouteWrite,
}

/// An object responsible to validating permissions for netlink clients.
pub trait AccessControl<C>: Clone {
    /// Returns true if a client with the specified credentials has the
    /// specified permission.
    fn grant_assess(&self, creds: &C, permission: Permission) -> Result<(), Errno>;
}

/// A message that may require special permissions from the sender to be
/// processed.
pub trait MessageWithPermission {
    /// Returns the permission that's required in order to process this message.
    fn permission(&self) -> Permission;
}

/// Encapsulates `NetlinkMessage` with the credentials of the sender.
#[derive(Clone, Debug)]
pub struct NetlinkMessageWithCreds<M, C>
where
    M: MessageWithPermission,
{
    message: NetlinkMessage<M>,
    creds: C,
}

impl<M, C> NetlinkMessageWithCreds<M, C>
where
    M: NetlinkSerializable + MessageWithPermission,
{
    /// Creates a new instance.
    pub fn new(message: NetlinkMessage<M>, creds: C) -> Self {
        Self { message, creds }
    }

    /// Validates permission using the specified `AccessControl` and returns
    /// the original message. If the permission is not granted then return an
    /// error with a message that should be sent back to the client.
    pub fn validate_creds_and_get_message<PS: AccessControl<C>>(
        self,
        access_control: &PS,
    ) -> Result<NetlinkMessage<M>, NetlinkMessage<M>> {
        let permission = match self.message.payload {
            NetlinkPayload::InnerMessage(ref msg) => msg.permission(),
            NetlinkPayload::Done(_)
            | NetlinkPayload::Error(_)
            | NetlinkPayload::Noop
            | NetlinkPayload::Overrun(_) => return Ok(self.message),
        };

        access_control
            .grant_assess(&self.creds, permission)
            .map_err(|errno| netlink_packet::new_error(Err(errno), self.message.header))?;
        Ok(self.message)
    }
}

/// A type capable of providing a concrete types used in Netlink.
pub trait NetlinkContext {
    /// The type used to represent process credentials.
    type Creds: Clone + Send + Debug;

    /// The type of [`Sender`] provided.
    type Sender<M: Clone + NetlinkSerializable + Send>: Sender<M>;

    /// The type of [`Receiver`] provided.
    type Receiver<M: Send + MessageWithPermission>: Receiver<M, Self::Creds>;

    /// The type of an object that validates access to netlink operations.
    type AccessControl<'a>: AccessControl<Self::Creds>;
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use crate::mpsc;
    use futures::{FutureExt as _, StreamExt as _};
    use netlink_packet_core::NetlinkSerializable;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) struct SentMessage<M> {
        pub message: NetlinkMessage<M>,
        pub group: Option<ModernGroup>,
    }

    impl<M> SentMessage<M> {
        pub(crate) fn unicast(message: NetlinkMessage<M>) -> Self {
            Self { message, group: None }
        }

        pub(crate) fn multicast(message: NetlinkMessage<M>, group: ModernGroup) -> Self {
            Self { message, group: Some(group) }
        }
    }

    #[derive(Clone, Debug)]
    pub(crate) struct FakeSender<M> {
        sender: futures::channel::mpsc::UnboundedSender<SentMessage<M>>,
    }

    impl<M: Clone + Send + NetlinkSerializable> Sender<M> for FakeSender<M> {
        fn send(&mut self, message: NetlinkMessage<M>, group: Option<ModernGroup>) {
            self.sender
                .unbounded_send(SentMessage { message, group })
                .expect("unable to send message");
        }
    }

    pub(crate) struct FakeSenderSink<M> {
        receiver: futures::channel::mpsc::UnboundedReceiver<SentMessage<M>>,
    }

    impl<M> FakeSenderSink<M> {
        pub(crate) fn take_messages(&mut self) -> Vec<SentMessage<M>> {
            let mut messages = Vec::new();
            while let Some(msg_opt) = self.receiver.next().now_or_never() {
                match msg_opt {
                    Some(msg) => messages.push(msg),
                    None => return messages, // Stream closed.
                };
            }
            // All receiver messages that were ready were added.
            messages
        }

        pub(crate) async fn next_message(&mut self) -> SentMessage<M> {
            self.receiver.next().await.expect("receiver unexpectedly closed")
        }
    }

    pub(crate) fn fake_sender_with_sink<M>() -> (FakeSender<M>, FakeSenderSink<M>) {
        let (sender, receiver) = futures::channel::mpsc::unbounded();
        (FakeSender { sender }, FakeSenderSink { receiver })
    }

    #[derive(Default, Debug, Clone)]
    pub(crate) struct FakeCreds {
        error: Option<Errno>,
    }

    impl FakeCreds {
        pub fn with_error(error: Errno) -> Self {
            FakeCreds { error: Some(error) }
        }
    }

    #[derive(Default, Clone)]
    pub(crate) struct FakeAccessControl {}

    impl AccessControl<FakeCreds> for FakeAccessControl {
        fn grant_assess(&self, creds: &FakeCreds, _perm: Permission) -> Result<(), Errno> {
            if let Some(ref error) = creds.error { Err(*error) } else { Ok(()) }
        }
    }

    pub(crate) struct TestNetlinkContext;

    impl NetlinkContext for TestNetlinkContext {
        type Creds = FakeCreds;
        type Sender<M: Clone + NetlinkSerializable + Send> = FakeSender<M>;
        type Receiver<M: Send + MessageWithPermission> =
            mpsc::Receiver<NetlinkMessageWithCreds<M, Self::Creds>>;
        type AccessControl<'a> = FakeAccessControl;
    }
}
