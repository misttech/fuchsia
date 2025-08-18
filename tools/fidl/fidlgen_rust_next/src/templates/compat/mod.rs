// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod bits;
mod r#enum;
mod protocol;
mod reserved;
mod r#struct;
mod table;
mod union;

use askama::Template;

use crate::templates::{Context, Contextual, Denylist};
use fidl_ir::{Bits, CompoundIdent, DeclType, Enum, Protocol, Struct, Table, Union};

use self::bits::*;
use self::r#enum::*;
use self::protocol::*;
use self::reserved::*;
use self::r#struct::*;
use self::table::*;
use self::union::*;

#[derive(Template)]
#[template(path = "compat.askama")]
pub struct CompatTemplate<'a> {
    context: Context<'a>,

    crate_name: String,
}

impl<'a> CompatTemplate<'a> {
    pub fn new(context: Context<'a>) -> Self {
        Self { context, crate_name: format!("fidl_{}", context.library().name.replace('.', "_")) }
    }

    fn rust_or_rust_next_denylist(&self, ident: &CompoundIdent) -> Denylist {
        Denylist::for_ident(self.context().library(), ident, &["rust", "rust_next"])
    }

    fn bits(&self, bits: &'a Bits) -> BitsCompatTemplate<'_> {
        BitsCompatTemplate::new(bits, self)
    }

    fn enm(&self, enm: &'a Enum) -> EnumCompatTemplate<'_> {
        EnumCompatTemplate::new(enm, self)
    }

    fn protocol(&self, protocol: &'a Protocol) -> ProtocolCompatTemplate<'_> {
        ProtocolCompatTemplate::new(protocol, self)
    }

    fn strct(&self, strct: &'a Struct) -> StructCompatTemplate<'_> {
        StructCompatTemplate::new(strct, self)
    }

    fn table(&self, table: &'a Table) -> TableCompatTemplate<'_> {
        TableCompatTemplate::new(table, self)
    }

    fn union(&self, union: &'a Union) -> UnionCompatTemplate<'_> {
        UnionCompatTemplate::new(union, self)
    }
}

impl<'a> Contextual<'a> for CompatTemplate<'a> {
    fn context(&self) -> Context<'a> {
        self.context
    }
}

mod filters {
    use crate::ident_ext::IdentExt as _;
    use crate::templates::compat::escape_compat;
    use fidl_ir::Ident;

    pub use crate::templates::filters::*;

    pub fn escape_compat_snake(ident: &Ident) -> String {
        escape_compat(ident.snake(), ident)
    }

    pub fn escape_compat_camel(ident: &Ident) -> String {
        escape_compat(ident.camel(), ident)
    }

    pub fn compat_snake(ident: &Ident, _: &dyn askama::Values) -> askama::Result<String> {
        Ok(escape_compat_snake(ident))
    }

    pub fn compat_camel(ident: &Ident, _: &dyn askama::Values) -> askama::Result<String> {
        Ok(escape_compat_camel(ident))
    }
}
