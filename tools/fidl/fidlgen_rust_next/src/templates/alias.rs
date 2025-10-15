// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::ops::Deref;

use askama::Template;

use super::{Context, Contextual};
use crate::templates::filters::escape_camel;
use crate::templates::natural_type::NaturalTypeTemplate;
use crate::templates::wire_type::WireTypeTemplate;
use fidl_ir::TypeAlias;
use fidl_ir_util::TypeShapeExt;

pub struct AliasTemplate<'a> {
    alias: &'a TypeAlias,
    context: Context<'a>,

    name: String,
    is_static: bool,
    natural_ty: NaturalTypeTemplate<'a>,
    wire_ty: WireTypeTemplate<'a>,
}

impl<'a> Contextual<'a> for AliasTemplate<'a> {
    fn context(&self) -> Context<'a> {
        self.context
    }
}

impl<'a> AliasTemplate<'a> {
    pub fn new(alias: &'a TypeAlias, context: Context<'a>) -> Self {
        Self {
            alias,
            context,

            name: escape_camel(alias.name.decl_name()),
            is_static: alias.ty.shape.is_static(),
            natural_ty: context.natural_type(&alias.ty),
            wire_ty: context.wire_type(&alias.ty),
        }
    }

    pub fn natural(self) -> NaturalAliasTemplate<'a> {
        NaturalAliasTemplate { template: self }
    }

    pub fn wire(self) -> WireAliasTemplate<'a> {
        WireAliasTemplate { template: self }
    }
}

#[derive(Template)]
#[template(path = "natural/alias.askama", whitespace = "preserve")]
pub struct NaturalAliasTemplate<'a> {
    template: AliasTemplate<'a>,
}

impl<'a> Deref for NaturalAliasTemplate<'a> {
    type Target = AliasTemplate<'a>;

    fn deref(&self) -> &Self::Target {
        &self.template
    }
}

#[derive(Template)]
#[template(path = "wire/alias.askama", whitespace = "preserve")]
pub struct WireAliasTemplate<'a> {
    template: AliasTemplate<'a>,
}

impl<'a> Deref for WireAliasTemplate<'a> {
    type Target = AliasTemplate<'a>;

    fn deref(&self) -> &Self::Target {
        &self.template
    }
}
