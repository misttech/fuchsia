// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::ops::Deref;

use askama::Template;

use super::{Context, Contextual, filters};
use crate::templates::filters::escape_camel;
use crate::templates::prim::{NaturalPrimTemplate, WirePrimTemplate};
use fidl_ir::{Bits, Type, TypeKind};

pub struct BitsTemplate<'a> {
    bits: &'a Bits,
    context: Context<'a>,

    name: String,
    natural_subtype: NaturalPrimTemplate<'a>,
    wire_subtype: WirePrimTemplate<'a>,
}

impl<'a> Contextual<'a> for BitsTemplate<'a> {
    fn context(&self) -> Context<'a> {
        self.context
    }
}

impl<'a> BitsTemplate<'a> {
    pub fn new(bits: &'a Bits, context: Context<'a>) -> Self {
        let Type { kind: TypeKind::Primitive { subtype }, .. } = &bits.ty else {
            panic!("invalid non-integral primitive subtype for bits");
        };

        Self {
            bits,
            context,

            name: escape_camel(bits.name.decl_name()),
            natural_subtype: context.natural_prim(subtype),
            wire_subtype: context.wire_prim(subtype),
        }
    }

    pub fn natural(self) -> NaturalBitsTemplate<'a> {
        NaturalBitsTemplate { template: self }
    }

    pub fn wire(self) -> WireBitsTemplate<'a> {
        WireBitsTemplate { template: self }
    }
}

#[derive(Template)]
#[template(path = "natural/bits.askama", whitespace = "preserve")]
pub struct NaturalBitsTemplate<'a> {
    template: BitsTemplate<'a>,
}

impl<'a> Deref for NaturalBitsTemplate<'a> {
    type Target = BitsTemplate<'a>;

    fn deref(&self) -> &Self::Target {
        &self.template
    }
}

#[derive(Template)]
#[template(path = "wire/bits.askama", whitespace = "preserve")]
pub struct WireBitsTemplate<'a> {
    template: BitsTemplate<'a>,
}

impl<'a> Deref for WireBitsTemplate<'a> {
    type Target = BitsTemplate<'a>;

    fn deref(&self) -> &Self::Target {
        &self.template
    }
}
