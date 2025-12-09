// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::CapabilityBound;
use fidl::handle::{self, HandleBased};

/// A capability that wraps a single Zircon handle.
#[derive(Debug)]
pub struct Handle(handle::NullableHandle);

impl Handle {
    /// Creates a new [Handle] containing a Zircon `handle`.
    pub fn new(handle: handle::NullableHandle) -> Self {
        Self(handle)
    }
}

impl From<handle::NullableHandle> for Handle {
    fn from(handle: handle::NullableHandle) -> Self {
        Self(handle)
    }
}

impl CapabilityBound for Handle {
    fn debug_typename() -> &'static str {
        "Handle"
    }
}

impl Handle {
    pub fn try_clone(&self) -> Result<Self, ()> {
        Ok(Self(self.0.duplicate_handle(fidl::Rights::SAME_RIGHTS).map_err(|_| ())?))
    }
}

impl From<Handle> for handle::NullableHandle {
    fn from(value: Handle) -> Self {
        value.0
    }
}

#[cfg(target_os = "fuchsia")]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::fidl::IntoFsandboxCapability;
    use crate::{Capability, WeakInstanceToken};
    use assert_matches::assert_matches;
    use fidl::handle::{AsHandleRef, HandleBased};
    use fidl_fuchsia_component_sandbox as fsandbox;

    // Tests converting the Handle to FIDL and back.
    #[fuchsia::test]
    async fn handle_into_fidl() {
        let event = zx::Event::create();
        let expected_koid = event.get_koid().unwrap();

        let handle = Handle::from(event.into_handle());

        // Convert the OneShotHandle to FIDL and back.
        let fidl_capability: fsandbox::Capability =
            handle.into_fsandbox_capability(WeakInstanceToken::new_invalid());
        assert_matches!(&fidl_capability, fsandbox::Capability::Handle(_));

        let any: Capability = fidl_capability.try_into().unwrap();
        let handle = assert_matches!(any, Capability::Handle(h) => h);

        // Get the handle.
        let handle: zx::NullableHandle = handle.into();

        // The handle should be for same Event that was in the original OneShotHandle.
        let got_koid = handle.get_koid().unwrap();
        assert_eq!(got_koid, expected_koid);
    }

    /// Tests that a Handle can be cloned by duplicating the handle.
    #[fuchsia::test]
    async fn try_clone() {
        let event = zx::Event::create();
        let expected_koid = event.get_koid().unwrap();

        let handle = Handle::from(event.into_handle());
        let handle = handle.try_clone().unwrap();
        let handle: zx::NullableHandle = handle.into();

        let got_koid = handle.get_koid().unwrap();
        assert_eq!(got_koid, expected_koid);

        let (ch, _) = zx::Channel::create();
        let handle = Handle::from(ch.into_handle());
        assert_matches!(handle.try_clone(), Err(()));
    }
}
