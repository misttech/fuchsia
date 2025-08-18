// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use askama::Template;

use super::{Context, Contextual};
use crate::ident_ext::IdentExt as _;
use crate::templates::reserved::escape;
use fidl_ir::{Const, TypeKind};

#[derive(Template)]
#[template(path = "const.askama", whitespace = "preserve")]
pub struct ConstTemplate<'a> {
    cnst: &'a Const,
    context: Context<'a>,

    name: String,
}

impl<'a> ConstTemplate<'a> {
    pub fn new(cnst: &'a Const, context: Context<'a>) -> Self {
        Self { cnst, context, name: escape(cnst.name.decl_name().screaming_snake()) }
    }
}

impl<'a> Contextual<'a> for ConstTemplate<'a> {
    fn context(&self) -> Context<'a> {
        self.context
    }
}
