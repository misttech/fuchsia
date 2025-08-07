// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use askama::Template;

use super::{filters, Context, Contextual};
use crate::id::IdExt as _;
use crate::ir::{CompId, Service, ServiceMember, TypeKind};
use crate::templates::reserved::escape;

#[derive(Template)]
#[template(path = "service.askama", whitespace = "preserve")]
pub struct ServiceTemplate<'a> {
    service: &'a Service,
    context: Context<'a>,

    non_canonical_name: &'a str,
    service_name: String,
    connector_name: String,
    handler_name: String,
}

impl<'a> ServiceTemplate<'a> {
    pub fn new(service: &'a Service, context: Context<'a>) -> Self {
        let base_name = service.name.decl_name().camel();
        let connector_name = format!("{base_name}Connector");
        let handler_name = format!("{base_name}Handler");

        Self {
            service,
            context,

            non_canonical_name: service.name.decl_name().non_canonical(),
            service_name: escape(base_name),
            connector_name: escape(connector_name),
            handler_name: escape(handler_name),
        }
    }

    fn service_name(&self) -> String {
        let (library, name) = self.service.name.split();
        format!("{}.{}", library, name.camel())
    }

    fn member_protocol<'m>(&self, member: &'m ServiceMember) -> &'m CompId {
        let TypeKind::Endpoint { protocol, .. } = &member.ty.kind else {
            panic!("service member type must be an endpoint");
        };

        protocol
    }

    fn member_transport(&self, member: &ServiceMember) -> &str {
        let TypeKind::Endpoint { protocol_transport, .. } = &member.ty.kind else {
            panic!("service member type must be an endpoint");
        };

        &self.resource_bindings().endpoint(protocol_transport).natural_path
    }
}

impl<'a> Contextual<'a> for ServiceTemplate<'a> {
    fn context(&self) -> Context<'a> {
        self.context
    }
}
