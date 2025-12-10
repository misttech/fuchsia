// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use askama::Template;

use crate::templates::{Context, Contextual, Denylist};
use fidl_ir::Bits;
use fidlgen::rust::RustIdent as _;

use super::{CompatTemplate, compat_camel};

#[derive(Template)]
#[template(path = "compat/bits.askama")]
pub struct BitsCompatTemplate<'a> {
    bits: &'a Bits,
    compat: &'a CompatTemplate<'a>,

    name: String,
    compat_name: String,
    denylist: Denylist,
}

impl Contextual for BitsCompatTemplate<'_> {
    fn context(&self) -> &Context {
        self.compat.context()
    }
}

impl<'a> BitsCompatTemplate<'a> {
    pub fn new(bits: &'a Bits, compat: &'a CompatTemplate<'a>) -> Self {
        Self {
            bits,
            compat,

            name: bits.name.decl_name().camel(),
            compat_name: compat_camel(bits.name.decl_name()),
            denylist: compat.rust_or_rust_next_denylist(&bits.name),
        }
    }
}
