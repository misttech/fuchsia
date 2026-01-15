// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::errors::UtilsError;
use fidl_fuchsia_component_sandbox::{
    self as fsandbox, AggregateSource, Capability, CapabilityId, DictionaryItem, DirConnector,
    DirConnectorRouterRouteResponse, RouteRequest,
};
use std::cell::Cell;
use std::collections::HashMap;

pub struct DictionaryUtil {
    capability_store: fsandbox::CapabilityStoreProxy,
    cap_id: Cell<CapabilityId>,
}

impl DictionaryUtil {
    pub fn new(capability_store: fsandbox::CapabilityStoreProxy) -> Self {
        Self { capability_store, cap_id: Cell::new(0) }
    }
}

impl DictionaryUtil {
    pub async fn import_dictionary(
        &self,
        dictionary: fsandbox::DictionaryRef,
    ) -> Result<CapabilityId, UtilsError> {
        let dest_id = self.next_cap_id();
        let capability = Capability::Dictionary(dictionary);
        self.capability_store.import(dest_id, capability).await??;
        Ok(dest_id)
    }

    pub async fn copy_export_dictionary(
        &self,
        dictionary_id: CapabilityId,
    ) -> Result<fsandbox::DictionaryRef, UtilsError> {
        let dest_id = self.next_cap_id();
        self.capability_store.dictionary_copy(dictionary_id, dest_id).await??;
        let exported = self.capability_store.export(dest_id).await??;
        if let Capability::Dictionary(d) = exported {
            Ok(d)
        } else {
            Err(UtilsError::UnexpectedCapabilityType(
                "Dictionary".to_string(),
                format!("{:?}", exported),
            ))
        }
    }

    pub async fn dictionary_dir_connector_route(
        &self,
        dictionary_id: CapabilityId,
        service_name: &str,
    ) -> Result<DirConnector, UtilsError> {
        let dest_id = self.next_cap_id();
        self.capability_store.dictionary_get(dictionary_id, service_name, dest_id).await??;
        let exported = self.capability_store.export(dest_id).await??;
        let Capability::DirConnectorRouter(router) = exported else {
            return Err(UtilsError::UnexpectedCapabilityType(
                "DirConnectorRouter".to_string(),
                format!("{:?}", exported),
            ));
        };

        let routed = router.into_proxy().route(RouteRequest { ..Default::default() }).await??;
        let DirConnectorRouterRouteResponse::DirConnector(connector) = routed else {
            return Err(UtilsError::UnexpectedCapabilityType(
                "DirConnector".to_string(),
                format!("{:?}", routed),
            ));
        };

        Ok(connector)
    }

    pub async fn create_aggregate_dictionary(
        &self,
        sources: HashMap<String, Vec<AggregateSource>>,
    ) -> Result<CapabilityId, UtilsError> {
        let dest_id = self.next_cap_id();
        self.capability_store.dictionary_create(dest_id).await??;

        for (service_name, sources) in sources.into_iter() {
            let aggregate = self.capability_store.create_service_aggregate(sources).await??;

            let imported = self.next_cap_id();
            self.capability_store.import(imported, Capability::DirConnector(aggregate)).await??;

            self.capability_store
                .dictionary_insert(dest_id, &DictionaryItem { key: service_name, value: imported })
                .await??;
        }

        Ok(dest_id)
    }

    fn next_cap_id(&self) -> CapabilityId {
        self.cap_id.set(self.cap_id.get() + 1);
        self.cap_id.get()
    }
}
