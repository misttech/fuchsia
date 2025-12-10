// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use askama::Template;

use crate::templates::{Context, Contextual, Denylist, compat_camel};
use fidl_ir::Enum;
use fidlgen::rust::RustIdent as _;

use super::CompatTemplate;

#[derive(Template)]
#[template(path = "compat/enum.askama")]
pub struct EnumCompatTemplate<'a> {
    enm: &'a Enum,
    compat: &'a CompatTemplate<'a>,

    name: String,
    compat_name: String,
    denylist: Denylist,
}

impl Contextual for EnumCompatTemplate<'_> {
    fn context(&self) -> &Context {
        self.compat.context()
    }
}

impl<'a> EnumCompatTemplate<'a> {
    pub fn new(enm: &'a Enum, compat: &'a CompatTemplate<'a>) -> Self {
        Self {
            enm,
            compat,

            name: enm.name.decl_name().camel(),
            compat_name: compat_camel(enm.name.decl_name()),
            denylist: compat.rust_or_rust_next_denylist(&enm.name),
        }
    }
}
