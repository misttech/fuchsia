// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use askama::Template;
use fidl_ir::Bits;
use fidlgen::rust::RustIdent as _;

use crate::context::{Context, Contextual};

#[derive(Template)]
#[template(path = "bits.askama")]
pub struct BitsTemplate<'a> {
    bits: &'a Bits,
    context: &'a Context,
}

impl<'a> BitsTemplate<'a> {
    pub fn new(bits: &'a Bits, context: &'a Context) -> Self {
        Self { bits, context }
    }
}

impl Contextual for BitsTemplate<'_> {
    fn context(&self) -> &Context {
        self.context
    }
}
