// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use askama::Template;

use crate::templates::{Context, Contextual, Denylist, compat_camel};
use fidl_ir::Protocol;
use fidlgen::rust::RustIdent as _;

use super::CompatTemplate;

#[derive(Template)]
#[template(path = "compat/protocol.askama")]
pub struct ProtocolCompatTemplate<'a> {
    protocol: &'a Protocol,
    compat: &'a CompatTemplate<'a>,

    name: String,
    proxy_name: String,
    compat_name: String,
    compat_proxy_name: String,
    denylist: Denylist,
}

impl Contextual for ProtocolCompatTemplate<'_> {
    fn context(&self) -> &Context {
        self.compat.context()
    }
}

impl<'a> ProtocolCompatTemplate<'a> {
    pub fn new(protocol: &'a Protocol, compat: &'a CompatTemplate<'a>) -> Self {
        let base_name = protocol.name.decl_name().camel();
        let proxy_name = format!("{base_name}Proxy");
        let compat_base_name = compat_camel(protocol.name.decl_name());
        let compat_name = format!("{compat_base_name}Marker");
        let compat_proxy_name = format!("{compat_base_name}Proxy");

        Self {
            protocol,
            compat,

            name: base_name,
            proxy_name,
            compat_name,
            compat_proxy_name,
            denylist: compat.rust_or_rust_next_denylist(&protocol.name),
        }
    }
}
