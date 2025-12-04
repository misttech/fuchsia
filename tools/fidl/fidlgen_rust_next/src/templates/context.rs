// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::Config;
use crate::config::ResourceBindings;
use core::any::Any;
use fidl_ir::{
    Attributes, CompoundIdent, Constant, IntType, Library, PrimSubtype, Protocol, Service, Type,
    TypeAlias,
};
use fidl_ir_util::LibraryExt as _;

use super::{
    CompoundIdentifierTemplate, ConstantTemplate, Denylist, DocStringTemplate, NaturalIntTemplate,
    NaturalPrimTemplate, NaturalTypeTemplate, WireIntTemplate, WirePrimTemplate, WireTypeTemplate,
    constraint_for,
};

#[derive(Clone, Copy)]
pub struct Context<'a> {
    library: &'a Library,
    config: &'a Config,
}

impl<'a> Context<'a> {
    pub fn new(library: &'a Library, config: &'a Config) -> Self {
        Self { library, config }
    }
}

pub trait Contextual<'a> {
    fn context(&self) -> Context<'a>;

    // Helpers

    fn library(&self) -> &'a Library {
        self.context().library
    }

    fn crate_prefix(&self) -> &'a String {
        &self.context().config.crate_prefix
    }

    fn resource_bindings(&self) -> &'a ResourceBindings {
        &self.context().config.resource_bindings
    }

    fn encode_trait_path(&self) -> &'a String {
        &self.context().config.encode_trait_path
    }

    fn decode_trait_path(&self) -> &'a String {
        &self.context().config.decode_trait_path
    }

    fn doc_string(&self, attributes: &'a Attributes) -> DocStringTemplate<'a> {
        DocStringTemplate::new(attributes)
    }

    fn emit_compat(&self) -> bool {
        self.context().config.emit_compat
    }

    fn emit_debug_impls(&self) -> bool {
        self.context().config.emit_debug_impls
    }

    fn non_type_id(&self, compound_ident: &'a CompoundIdent) -> CompoundIdentifierTemplate<'a> {
        CompoundIdentifierTemplate::non_type(compound_ident, self.context())
    }

    fn common_lib(&self) -> Option<&'a str> {
        self.context().config.common_lib.as_deref()
    }

    fn compat_crate_name(&self) -> String {
        format!("fidl_{}", self.context().library().name.replace('.', "_"))
    }

    fn natural_id(&self, compound_ident: &'a CompoundIdent) -> CompoundIdentifierTemplate<'a> {
        CompoundIdentifierTemplate::natural(compound_ident, self.context())
    }

    fn wire_id(&self, compound_ident: &'a CompoundIdent) -> CompoundIdentifierTemplate<'a> {
        CompoundIdentifierTemplate::wire(compound_ident, self.context())
    }

    fn wire_optional_id(
        &self,
        compound_ident: &'a CompoundIdent,
    ) -> CompoundIdentifierTemplate<'a> {
        CompoundIdentifierTemplate::wire_optional(compound_ident, self.context())
    }

    fn generic_id(&self, compound_ident: &'a CompoundIdent) -> CompoundIdentifierTemplate<'a> {
        CompoundIdentifierTemplate::generic(compound_ident, self.context())
    }

    fn natural_int(&self, int: &'a IntType) -> NaturalIntTemplate<'a> {
        NaturalIntTemplate(int)
    }

    fn natural_prim(&self, prim: &'a PrimSubtype) -> NaturalPrimTemplate<'a> {
        NaturalPrimTemplate(prim)
    }

    fn natural_type(&self, ty: &'a Type) -> NaturalTypeTemplate<'a> {
        NaturalTypeTemplate::new(ty, self.context())
    }

    fn wire_int(&self, int: &'a IntType) -> WireIntTemplate<'a> {
        WireIntTemplate(int)
    }

    fn wire_prim(&self, prim: &'a PrimSubtype) -> WirePrimTemplate<'a> {
        WirePrimTemplate(prim)
    }

    fn wire_type(&self, ty: &'a Type) -> WireTypeTemplate<'a> {
        WireTypeTemplate::with_de(ty, self.context())
    }

    fn static_wire_type(&self, ty: &'a Type) -> WireTypeTemplate<'a> {
        WireTypeTemplate::with_static(ty, self.context())
    }

    fn anonymous_wire_type(&self, ty: &'a Type) -> WireTypeTemplate<'a> {
        WireTypeTemplate::with_anonymous(ty, self.context())
    }

    fn constant(&self, constant: &'a Constant, ty: &'a Type) -> ConstantTemplate<'a> {
        ConstantTemplate::new(constant, ty, self.context())
    }

    fn rust_next_denylist(&self, ident: &CompoundIdent) -> Denylist {
        Denylist::for_ident(self.context().library, ident, &["rust_next"])
    }

    fn constraint(&self, ty: &'a Type) -> String {
        match constraint_for(ty) {
            Some(constraint) => constraint,
            None => "()".to_string(),
        }
    }

    fn validate(&self, ty: &'a Type, field_name: &str) -> String {
        if let Some(constraint) = constraint_for(ty) {
            format!("::fidl_next::Constrained::validate({field_name}, {constraint})?;",)
        } else {
            String::new()
        }
    }

    fn emit_given_commonness(&self, ident: &CompoundIdent) -> bool {
        if self.context().config.is_common {
            !has_resources(self.context().library(), ident)
        } else if self.context().config.common_lib.is_some() {
            has_resources(self.context().library(), ident)
        } else {
            true
        }
    }
}

impl<'a> Contextual<'a> for Context<'a> {
    fn context(&self) -> Context<'a> {
        *self
    }
}

fn has_resources(library: &Library, ident: &CompoundIdent) -> bool {
    let Some(decl) = library.get_local_decl(ident) else {
        return true;
    };

    if let Some(is_resource) = decl.is_resource() {
        is_resource
    } else if (decl as &dyn Any).is::<Service>() {
        true
    } else if let Some(protocol) = (decl as &dyn Any).downcast_ref::<Protocol>() {
        for method in &protocol.methods {
            if let Some(ty) = &method.maybe_request_payload
                && type_has_resources(library, ty)
            {
                return true;
            }

            if let Some(ty) = &method.maybe_response_payload
                && type_has_resources(library, ty)
            {
                return true;
            }

            if let Some(ty) = &method.maybe_response_err_type
                && type_has_resources(library, ty)
            {
                return true;
            }

            if let Some(ty) = &method.maybe_response_success_type
                && type_has_resources(library, ty)
            {
                return true;
            }
        }

        false
    } else if let Some(alias) = (decl as &dyn Any).downcast_ref::<TypeAlias>() {
        type_has_resources(library, &alias.ty)
    } else {
        unreachable!("Did not recognize Decl type");
    }
}

fn type_has_resources(library: &Library, ty: &Type) -> bool {
    match &ty.kind {
        fidl_ir::TypeKind::Array { element_type, .. }
        | fidl_ir::TypeKind::Vector { element_type, .. } => {
            type_has_resources(library, element_type)
        }
        fidl_ir::TypeKind::String { .. } => false,
        fidl_ir::TypeKind::Handle { .. } => true,
        fidl_ir::TypeKind::Endpoint { .. } => true,
        fidl_ir::TypeKind::Primitive { .. } => false,
        fidl_ir::TypeKind::Identifier { identifier, .. } => has_resources(library, identifier),
        fidl_ir::TypeKind::Internal { .. } => false,
    }
}
