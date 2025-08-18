// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use askama::Template;

use super::{Context, Contextual, filters};
use crate::ident_ext::IdentExt as _;
use crate::templates::prim::{NaturalPrimTemplate, WirePrimTemplate};
use crate::templates::reserved::escape;
use fidl_ir::{Bits, Type, TypeKind};

#[derive(Template)]
#[template(path = "bits.askama", whitespace = "preserve")]
pub struct BitsTemplate<'a> {
    bits: &'a Bits,
    context: Context<'a>,

    name: String,
    wire_name: String,
    natural_subtype: NaturalPrimTemplate<'a>,
    wire_subtype: WirePrimTemplate<'a>,
}

impl<'a> BitsTemplate<'a> {
    pub fn new(bits: &'a Bits, context: Context<'a>) -> Self {
        let base_name = bits.name.decl_name().camel();
        let wire_name = format!("Wire{base_name}");
        let Type { kind: TypeKind::Primitive { subtype }, .. } = &bits.ty else {
            panic!("invalid non-integral primitive subtype for bits");
        };

        Self {
            bits,
            context,

            name: escape(base_name),
            wire_name: escape(wire_name),
            natural_subtype: context.natural_prim(subtype),
            wire_subtype: context.wire_prim(subtype),
        }
    }
}

impl<'a> Contextual<'a> for BitsTemplate<'a> {
    fn context(&self) -> Context<'a> {
        self.context
    }
}
