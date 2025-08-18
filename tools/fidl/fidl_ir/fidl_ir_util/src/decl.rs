// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::any::Any;

use fidl_ir::*;

/// A schema declaration.
pub trait Decl: Any {
    /// Returns the type of the declaration.
    fn decl_type(&self) -> DeclType;

    /// Returns the name of the declaration.
    fn name(&self) -> &CompoundIdent;

    /// Returns the attributes of the declaration.
    fn attributes(&self) -> &Attributes;

    /// Returns the naming context of the declaration, if any.
    fn naming_context(&self) -> Option<&[String]> {
        None
    }

    /// Returns the type shape of the declaration, if any.
    fn type_shape(&self) -> Option<&TypeShape> {
        None
    }
}

impl Decl for Bits {
    fn decl_type(&self) -> DeclType {
        DeclType::Bits
    }

    fn name(&self) -> &CompoundIdent {
        &self.name
    }

    fn attributes(&self) -> &Attributes {
        &self.attributes
    }

    fn naming_context(&self) -> Option<&[String]> {
        Some(&self.naming_context)
    }
}

impl Decl for Const {
    fn decl_type(&self) -> DeclType {
        DeclType::Const
    }

    fn name(&self) -> &CompoundIdent {
        &self.name
    }

    fn attributes(&self) -> &Attributes {
        &self.attributes
    }
}

impl Decl for Enum {
    fn decl_type(&self) -> DeclType {
        DeclType::Enum
    }

    fn name(&self) -> &CompoundIdent {
        &self.name
    }

    fn attributes(&self) -> &Attributes {
        &self.attributes
    }

    fn naming_context(&self) -> Option<&[String]> {
        Some(&self.naming_context)
    }
}

impl Decl for Protocol {
    fn decl_type(&self) -> DeclType {
        DeclType::Protocol
    }

    fn name(&self) -> &CompoundIdent {
        &self.name
    }

    fn attributes(&self) -> &Attributes {
        &self.attributes
    }
}

impl Decl for Service {
    fn decl_type(&self) -> DeclType {
        DeclType::Service
    }

    fn name(&self) -> &CompoundIdent {
        &self.name
    }

    fn attributes(&self) -> &Attributes {
        &self.attributes
    }
}

impl Decl for Struct {
    fn decl_type(&self) -> DeclType {
        DeclType::Struct
    }

    fn name(&self) -> &CompoundIdent {
        &self.name
    }

    fn attributes(&self) -> &Attributes {
        &self.attributes
    }

    fn naming_context(&self) -> Option<&[String]> {
        Some(&self.naming_context)
    }

    fn type_shape(&self) -> Option<&TypeShape> {
        Some(&self.shape)
    }
}

impl Decl for Table {
    fn decl_type(&self) -> DeclType {
        DeclType::Table
    }

    fn name(&self) -> &CompoundIdent {
        &self.name
    }

    fn attributes(&self) -> &Attributes {
        &self.attributes
    }

    fn naming_context(&self) -> Option<&[String]> {
        Some(&self.naming_context)
    }

    fn type_shape(&self) -> Option<&TypeShape> {
        Some(&self.shape)
    }
}

impl Decl for TypeAlias {
    fn decl_type(&self) -> DeclType {
        DeclType::Bits
    }

    fn name(&self) -> &CompoundIdent {
        &self.name
    }

    fn attributes(&self) -> &Attributes {
        &self.attributes
    }

    fn naming_context(&self) -> Option<&[String]> {
        None
    }
}

impl Decl for Union {
    fn decl_type(&self) -> DeclType {
        DeclType::Union
    }

    fn name(&self) -> &CompoundIdent {
        &self.name
    }

    fn attributes(&self) -> &Attributes {
        &self.attributes
    }

    fn naming_context(&self) -> Option<&[String]> {
        Some(&self.naming_context)
    }

    fn type_shape(&self) -> Option<&TypeShape> {
        Some(&self.shape)
    }
}
