// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::fmt;

use super::{Context, Contextual, escape};
use crate::ident_ext::IdentExt;
use fidl_ir::{CompoundIdent, DeclType};
use fidl_ir_util::LibraryExt as _;

pub struct CompoundIdentifierTemplate<'a> {
    context: Context<'a>,
    id: &'a CompoundIdent,
    prefix: &'a str,
}

impl<'a> CompoundIdentifierTemplate<'a> {
    fn new(id: &'a CompoundIdent, prefix: &'a str, context: Context<'a>) -> Self {
        Self { context, id, prefix }
    }

    pub fn natural(id: &'a CompoundIdent, context: Context<'a>) -> Self {
        Self::new(id, "", context)
    }

    pub fn wire(id: &'a CompoundIdent, context: Context<'a>) -> Self {
        Self::new(id, "Wire", context)
    }

    pub fn wire_optional(id: &'a CompoundIdent, context: Context<'a>) -> Self {
        Self::new(id, "WireOptional", context)
    }
}

impl<'a> Contextual<'a> for CompoundIdentifierTemplate<'a> {
    fn context(&self) -> Context<'a> {
        self.context
    }
}

impl fmt::Display for CompoundIdentifierTemplate<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (lib, ty) = self.id.split();

        // Special case: zx::ObjType
        if lib == "zx" && ty.non_canonical() == "ObjType" {
            return write!(f, "::fidl_next::fuchsia::zx::ObjectType");
        }

        // Crate prefix
        if lib == self.library().name {
            write!(f, "crate::")?;
        } else if lib == "zx" {
            write!(f, "::fidl_next::fuchsia::zx::")?;
        } else {
            let escaped = lib.replace('.', "_");
            write!(f, "::fidl_next_{escaped}::")?;
        }

        // Type name
        let base_name = match self.library().get_decl_type(self.id).unwrap() {
            DeclType::Alias
            | DeclType::Bits
            | DeclType::Enum
            | DeclType::Struct
            | DeclType::Table
            | DeclType::Union
            | DeclType::Protocol => ty.camel(),
            DeclType::Const => ty.screaming_snake(),
            DeclType::ExperimentalResource
            | DeclType::NewType
            | DeclType::Overlay
            | DeclType::Service => {
                todo!()
            }
        };
        let name = escape(format!("{}{base_name}", self.prefix));

        write!(f, "{name}")?;

        Ok(())
    }
}
