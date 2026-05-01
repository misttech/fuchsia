// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{CapabilityBound, Receiver};
use futures::channel::mpsc;
use std::fmt::Debug;
use std::sync::Arc;

/// Types that implement [`Connectable`] let the holder send channels
/// to them.
pub trait Connectable: Send + Sync + Debug {
    fn send(&self, channel: fidl::Channel) -> Result<(), ()>;
}

impl Connectable for mpsc::UnboundedSender<fidl::Channel> {
    fn send(&self, channel: fidl::Channel) -> Result<(), ()> {
        self.unbounded_send(channel).map_err(|_| ())
    }
}

/// A capability that transfers another capability to a [Receiver].
#[derive(Debug, Clone)]
pub struct Connector {
    inner: Arc<dyn Connectable>,
}

impl CapabilityBound for Connector {
    fn debug_typename() -> &'static str {
        "Connector"
    }
}

impl Connector {
    pub fn new() -> (Receiver, Self) {
        let (sender, receiver) = mpsc::unbounded();
        let receiver = Receiver::new(receiver);
        let this = Self::new_sendable(sender);
        (receiver, this)
    }

    pub fn new_sendable(connector: impl Connectable + 'static) -> Self {
        Self { inner: Arc::new(connector) }
    }

    pub fn send(&self, channel: fidl::Channel) -> Result<(), ()> {
        self.inner.send(channel)
    }
}

impl Connectable for Connector {
    fn send(&self, channel: fidl::Channel) -> Result<(), ()> {
        self.send(channel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::handle::{Channel, HandleBased, Rights};
    use fidl_fuchsia_component_sandbox as fsandbox;

    // NOTE: sending-and-receiving tests are written in `receiver.rs`.

    /// Tests that a Connector can be cloned by cloning its FIDL token.
    /// and capabilities sent to the original and clone arrive at the same Receiver.
    #[fuchsia::test]
    async fn fidl_clone() {
        let (receiver, sender) = Connector::new();

        // Send a channel through the Connector.
        let (ch1, _ch2) = Channel::create();
        sender.send(ch1).unwrap();

        // Convert the Sender to a FIDL token.
        let connector: fsandbox::Connector = sender.into();

        // Clone the Sender by cloning the token.
        let token_clone = fsandbox::Connector {
            token: connector.token.duplicate_handle(Rights::SAME_RIGHTS).unwrap(),
        };
        let connector_clone = match crate::Capability::try_from(fsandbox::Capability::Connector(
            token_clone,
        ))
        .unwrap()
        {
            crate::Capability::Connector(connector) => connector,
            capability @ _ => panic!("wrong type {capability:?}"),
        };

        // Send a channel through the cloned Sender.
        let (ch1, _ch2) = Channel::create();
        connector_clone.send(ch1).unwrap();

        // The Receiver should receive two channels, one from each connector.
        for _ in 0..2 {
            let _ch = receiver.receive().await.unwrap();
        }
    }
}
