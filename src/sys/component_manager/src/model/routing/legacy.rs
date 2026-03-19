// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::model::component::{ComponentInstance, WeakComponentInstance};
use crate::model::routing;
use ::routing::RouteRequest;
use ::routing::component_instance::ComponentInstanceInterface;
use cm_rust::UseEventStreamDecl;
use fidl_fuchsia_io as fio;
use log::*;
use router_error::Explain;
use sandbox::{Capability, DirEntry};
use std::sync::Arc;
use vfs::directory::entry::{
    DirectoryEntry, DirectoryEntryAsync, EntryInfo, GetEntryInfo, OpenRequest,
};

pub trait RouteRequestExt {
    fn into_capability(self, target: &Arc<ComponentInstance>) -> Option<Capability>;
}

impl RouteRequestExt for RouteRequest {
    fn into_capability(self, target: &Arc<ComponentInstance>) -> Option<Capability> {
        let cap = match self {
            Self::UseService(_) => {
                panic!("Services should use bedrock instead");
            }
            Self::UseDirectory(_) => {
                panic!("Directories should use bedrock instead");
            }
            Self::UseStorage(_) => {
                panic!("Storage should use bedrock instead");
            }
            Self::UseEventStream(decl) => use_event_stream(decl, target),
            Self::UseProtocol(_) => {
                panic!("Protocols should use bedrock instead");
            }
            Self::ExposeProtocol(_) => {
                panic!("Protocols should use bedrock instead");
            }
            Self::ExposeService(_) => {
                panic!("Services should use bedrock instead");
            }
            Self::ExposeDirectory(_) => {
                panic!("Directories should use bedrock instead");
            }
            _ => return None,
        };
        Some(cap)
    }
}

fn use_event_stream(decl: UseEventStreamDecl, target: &Arc<ComponentInstance>) -> Capability {
    struct UseEventStream {
        component: WeakComponentInstance,
        decl: UseEventStreamDecl,
    }
    impl DirectoryEntry for UseEventStream {
        fn open_entry(self: Arc<Self>, request: OpenRequest<'_>) -> Result<(), zx::Status> {
            if !request.path().is_empty() {
                return Err(zx::Status::NOT_DIR);
            }
            request.spawn(self);
            Ok(())
        }
    }
    impl GetEntryInfo for UseEventStream {
        fn entry_info(&self) -> EntryInfo {
            EntryInfo::new(fio::INO_UNKNOWN, fio::DirentType::Service)
        }
    }
    impl DirectoryEntryAsync for UseEventStream {
        async fn open_entry_async(
            self: Arc<Self>,
            mut request: OpenRequest<'_>,
        ) -> Result<(), zx::Status> {
            let component = match self.component.upgrade() {
                Ok(component) => component,
                Err(e) => {
                    error!(
                        "failed to upgrade WeakComponentInstance routing use \
                                 decl `{:?}`: {:?}",
                        self.decl, e
                    );
                    return Err(e.as_zx_status());
                }
            };

            request.prepend_path(&self.decl.target_path.to_string().try_into()?);
            let route_request = RouteRequest::UseEventStream(self.decl.clone());
            routing::route_and_open_capability_with_reporting(&route_request, &component, request)
                .await
                .map_err(|e| e.as_zx_status())
        }
    }
    DirEntry::new(Arc::new(UseEventStream { component: target.as_weak(), decl })).into()
}
