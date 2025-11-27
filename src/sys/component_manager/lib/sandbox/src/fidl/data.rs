// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::WeakInstanceToken;
use fidl_fuchsia_component_sandbox as fsandbox;

impl crate::RemotableCapability for crate::Data {}
impl crate::fidl::IntoFsandboxCapability for crate::Data {
    fn into_fsandbox_capability(self, _token: WeakInstanceToken) -> fsandbox::Capability {
        fsandbox::Capability::Data(match self {
            Self::Bytes(bytes) => fsandbox::Data::Bytes(bytes.to_vec()),
            Self::String(string) => fsandbox::Data::String(string.to_string()),
            Self::Int64(num) => fsandbox::Data::Int64(num),
            Self::Uint64(num) => fsandbox::Data::Uint64(num),
        })
    }
}
