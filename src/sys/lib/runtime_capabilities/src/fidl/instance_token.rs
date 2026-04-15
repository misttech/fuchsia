// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::WeakInstanceToken;
use fidl::handle::{EventPair, Signals};
use fidl_fuchsia_component_sandbox as fsandbox;
use fuchsia_async as fasync;
use zx::Koid;

impl crate::RemotableCapability for WeakInstanceToken {}
impl crate::fidl::IntoFsandboxCapability for crate::WeakInstanceToken {
    fn into_fsandbox_capability(self, _token: WeakInstanceToken) -> fsandbox::Capability {
        panic!("unsupported");
    }
}

impl WeakInstanceToken {
    async fn serve(server: EventPair) {
        fasync::OnSignals::new(&server, Signals::OBJECT_PEER_CLOSED).await.ok();
    }

    pub fn register(self, koid: Koid, server: EventPair) {
        crate::fidl::registry::insert(self.into(), koid, WeakInstanceToken::serve(server));
    }
}
