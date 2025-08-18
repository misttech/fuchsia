// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::fmt;

use super::{Context, Contextual};
use fidl_ir::{DeclType, EndpointRole, InternalSubtype, Type, TypeKind};
use fidl_ir_util::LibraryExt as _;

pub struct WireTypeTemplate<'a> {
    context: Context<'a>,
    ty: &'a Type,
    lifetime: &'a str,
}

impl<'a> WireTypeTemplate<'a> {
    pub fn new(ty: &'a Type, lifetime: &'a str, context: Context<'a>) -> Self {
        Self { context, ty, lifetime }
    }

    pub fn with_de(ty: &'a Type, context: Context<'a>) -> Self {
        Self::new(ty, "'de", context)
    }

    pub fn with_static(ty: &'a Type, context: Context<'a>) -> Self {
        Self::new(ty, "'static", context)
    }

    pub fn with_anonymous(ty: &'a Type, context: Context<'a>) -> Self {
        Self::new(ty, "'_", context)
    }
}

impl<'a> Contextual<'a> for WireTypeTemplate<'a> {
    fn context(&self) -> Context<'a> {
        self.context
    }
}

impl fmt::Display for WireTypeTemplate<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.ty.kind {
            TypeKind::Array { element_type, element_count } => {
                let wire_ty = Self::new(element_type, self.lifetime, self.context);
                write!(f, "[{wire_ty}; {element_count}]")?;
            }
            TypeKind::Vector { element_type, nullable, .. } => {
                let wire_ty = Self::new(element_type, self.lifetime, self.context);
                if *nullable {
                    write!(f, "::fidl_next::WireOptionalVector<{}, {wire_ty}>", self.lifetime)?;
                } else {
                    write!(f, "::fidl_next::WireVector<{}, {wire_ty}>", self.lifetime)?;
                }
            }
            TypeKind::String { nullable, .. } => {
                if *nullable {
                    write!(f, "::fidl_next::WireOptionalString<{}>", self.lifetime)?;
                } else {
                    write!(f, "::fidl_next::WireString<{}>", self.lifetime)?;
                }
            }
            TypeKind::Handle { nullable, subtype, resource_identifier, .. } => {
                if *nullable {
                    write!(
                        f,
                        "{}",
                        self.resource_bindings()
                            .handle(resource_identifier)
                            .optional_wire_path(*subtype)
                    )?;
                } else {
                    write!(
                        f,
                        "{}",
                        self.resource_bindings().handle(resource_identifier).wire_path(*subtype)
                    )?;
                }
            }
            TypeKind::Endpoint { nullable, role, protocol, protocol_transport } => {
                let role = match role {
                    EndpointRole::Client => "::fidl_next::ClientEnd",
                    EndpointRole::Server => "::fidl_next::ServerEnd",
                };
                let protocol_id = self.natural_id(protocol);
                if *nullable {
                    write!(
                        f,
                        "{role}<{protocol_id}, {}>",
                        self.resource_bindings().endpoint(protocol_transport).optional_wire_path,
                    )?;
                } else {
                    write!(
                        f,
                        "{role}<{protocol_id}, {}>",
                        self.resource_bindings().endpoint(protocol_transport).wire_path,
                    )?;
                }
            }
            TypeKind::Primitive { subtype } => {
                write!(f, "{}", self.wire_prim(subtype))?;
            }
            TypeKind::Identifier { identifier, nullable, .. } => {
                let wire_id = self.wire_id(identifier);

                match self.library().get_decl_type(identifier).unwrap() {
                    DeclType::Bits | DeclType::Enum => write!(f, "{wire_id}")?,
                    DeclType::Table => write!(f, "{wire_id}<{}>", self.lifetime)?,
                    DeclType::Struct => {
                        if *nullable {
                            write!(f, "::fidl_next::WireBox<{}, ", self.lifetime)?;
                        }

                        write!(f, "{wire_id}")?;

                        if let Some(shape) = self.library().get_type_shape(identifier) {
                            if shape.max_out_of_line != 0 {
                                write!(f, "<{}>", self.lifetime)?;
                            }
                        }

                        if *nullable {
                            write!(f, ">")?;
                        }
                    }
                    DeclType::Union => {
                        let id = if *nullable {
                            self.wire_optional_id(identifier)
                        } else {
                            self.wire_id(identifier)
                        };
                        if self.ty.shape.max_out_of_line != 0 {
                            write!(f, "{id}<{}>", self.lifetime)?;
                        } else {
                            write!(f, "{id}")?;
                        }
                    }
                    DeclType::ExperimentalResource => unreachable!(),
                    _ => unimplemented!(),
                }
            }
            TypeKind::Internal { subtype } => match subtype {
                InternalSubtype::FrameworkError => {
                    write!(f, "::fidl_next::WireFrameworkError")?;
                }
            },
        }

        Ok(())
    }
}
