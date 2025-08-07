// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::HashMap;

use crate::ir::HandleSubtype;

pub struct Config {
    pub emit_compat: bool,
    pub emit_debug_impls: bool,
    pub resource_bindings: ResourceBindings,
}

pub struct HandleResourceBinding {
    wire_path_template: String,
    optional_wire_path_template: String,
    natural_path_template: String,
}

impl HandleResourceBinding {
    fn handle_subtype_name(subtype: HandleSubtype) -> &'static str {
        match subtype {
            HandleSubtype::None => "Handle",
            HandleSubtype::Process => "Process",
            HandleSubtype::Thread => "Thread",
            HandleSubtype::Vmo => "Vmo",
            HandleSubtype::Channel => "Channel",
            HandleSubtype::Event => "Event",
            HandleSubtype::Port => "Port",
            HandleSubtype::Interrupt => "Interrupt",
            HandleSubtype::PciDevice => "PciDevice",
            HandleSubtype::Log => "Log",
            HandleSubtype::Socket => "Socket",
            HandleSubtype::Resource => "Resource",
            HandleSubtype::EventPair => "EventPair",
            HandleSubtype::Job => "Job",
            HandleSubtype::Vmar => "Vmar",
            HandleSubtype::Fifo => "Fifo",
            HandleSubtype::Guest => "Guest",
            HandleSubtype::Vcpu => "Vcpu",
            HandleSubtype::Timer => "Timer",
            HandleSubtype::Iommu => "Iommu",
            HandleSubtype::Bti => "Bti",
            HandleSubtype::Profile => "Profile",
            HandleSubtype::Pmt => "Pmt",
            HandleSubtype::SuspendToken => "SuspendToken",
            HandleSubtype::Pager => "Pager",
            HandleSubtype::Exception => "Exception",
            HandleSubtype::Clock => "Clock",
            HandleSubtype::Stream => "Stream",
            HandleSubtype::Msi => "Msi",
            HandleSubtype::Iob => "Iob",
        }
    }

    pub fn wire_path(&self, subtype: HandleSubtype) -> String {
        self.wire_path_template.replace("{subtype}", Self::handle_subtype_name(subtype))
    }

    pub fn optional_wire_path(&self, subtype: HandleSubtype) -> String {
        self.optional_wire_path_template.replace("{subtype}", Self::handle_subtype_name(subtype))
    }

    pub fn natural_path(&self, subtype: HandleSubtype) -> String {
        self.natural_path_template.replace("{subtype}", Self::handle_subtype_name(subtype))
    }
}

pub struct ResourceBinding {
    pub wire_path: String,
    pub optional_wire_path: String,
    pub natural_path: String,
}

pub struct ResourceBindings {
    // Maps resource identifier (e.g. "zx/Handle") to binding types.
    handles: HashMap<String, HandleResourceBinding>,
    // The resource identifier to use for unrecognized handles.
    default_handle: String,
    // Maps protocol transport (e.g. "Channel") to binding types.
    endpoints: HashMap<String, ResourceBinding>,
    // The protocol transport to use for unrecognized transports.
    default_endpoint: String,
}

impl ResourceBindings {
    pub fn handle(&self, handle: &str) -> &HandleResourceBinding {
        self.handles.get(handle).or_else(|| self.handles.get(&self.default_handle)).unwrap()
    }

    pub fn endpoint(&self, transport: &str) -> &ResourceBinding {
        self.endpoints
            .get(transport)
            .or_else(|| self.endpoints.get(&self.default_endpoint))
            .unwrap()
    }
}

impl Default for ResourceBindings {
    fn default() -> Self {
        let mut handles = HashMap::new();
        handles.insert(
            "zx/Handle".to_string(),
            HandleResourceBinding {
                wire_path_template: "::fidl_next::fuchsia::Wire{subtype}".to_string(),
                optional_wire_path_template: "::fidl_next::fuchsia::WireOptional{subtype}"
                    .to_string(),
                natural_path_template: "::fidl_next::fuchsia::zx::{subtype}".to_string(),
            },
        );
        handles.insert(
            "fdf/handle".to_string(),
            HandleResourceBinding {
                wire_path_template: "::fdf_fidl::WireDriverChannel".to_string(),
                optional_wire_path_template: "::fdf_fidl::WireOptionalDriverChannel".to_string(),
                natural_path_template: "::fdf_fidl::DriverChannel".to_string(),
            },
        );

        let mut endpoints = HashMap::new();
        endpoints.insert(
            "Channel".to_string(),
            ResourceBinding {
                wire_path: "::fidl_next::fuchsia::WireChannel".to_string(),
                optional_wire_path: "::fidl_next::fuchsia::WireOptionalChannel".to_string(),
                natural_path: "::fidl_next::fuchsia::zx::Channel".to_string(),
            },
        );
        endpoints.insert(
            "Driver".to_string(),
            ResourceBinding {
                wire_path: "::fdf_fidl::WireDriverChannel".to_string(),
                optional_wire_path: "::fdf_fidl::WireOptionalDriverChannel".to_string(),
                natural_path: "::fdf_fidl::DriverChannel".to_string(),
            },
        );

        Self {
            handles,
            default_handle: "zx/Handle".to_string(),
            endpoints,
            default_endpoint: "Channel".to_string(),
        }
    }
}
