// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fidl::registry;
use crate::{Connector, ConversionError, Message, Receiver, WeakInstanceToken};
use fidl::endpoints::ClientEnd;
use fidl::handle::Channel;
use fidl_fuchsia_component_sandbox as fsandbox;
use fuchsia_async as fasync;
use futures::channel::mpsc;
use std::sync::Arc;
use vfs::directory::entry::DirectoryEntry;
use vfs::execution_scope::ExecutionScope;

impl Connector {
    pub(crate) fn send_channel(&self, channel: Channel) -> Result<(), ()> {
        self.send(Message { channel })
    }

    pub(crate) fn new_with_fidl_receiver(
        receiver_client: ClientEnd<fsandbox::ReceiverMarker>,
        scope: &fasync::Scope,
    ) -> Self {
        let (sender, receiver) = mpsc::unbounded();
        let receiver = Receiver::new(receiver);
        // Exits when ServerEnd<Receiver> is closed
        scope.spawn(receiver.handle_receiver(receiver_client.into_proxy()));
        Self::new_sendable(sender)
    }
}

impl crate::RemotableCapability for Connector {
    fn try_into_directory_entry(
        self,
        _scope: ExecutionScope,
        _token: WeakInstanceToken,
    ) -> Result<Arc<dyn DirectoryEntry>, ConversionError> {
        Ok(vfs::service::endpoint(move |_scope, server_end| {
            let _ = self.send_channel(server_end.into_zx_channel().into());
        }))
    }
}

impl From<Connector> for fsandbox::Connector {
    fn from(value: Connector) -> Self {
        fsandbox::Connector { token: registry::insert_token(value.into()) }
    }
}

impl crate::fidl::IntoFsandboxCapability for Connector {
    fn into_fsandbox_capability(self, _token: WeakInstanceToken) -> fsandbox::Capability {
        fsandbox::Capability::Connector(self.into())
    }
}
