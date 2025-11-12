// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::EnvironmentBuilder;
use crate::tests::fakes::recovery_policy_service::RecoveryPolicy;
use assert_matches::assert_matches;
use fidl_fuchsia_settings::FactoryResetMarker;
use futures::lock::Mutex;
use settings_test_common::fakes::service::ServiceRegistry;
use settings_test_common::storage::InMemoryStorageFactory;
use std::rc::Rc;

const ENV_NAME: &str = "settings_service_factory_test_environment";

// Tests that the FIDL calls for the reset setting result in appropriate
// commands sent to the service.
#[fuchsia::test(allow_stalls = false)]
async fn test_error_propagation() {
    let service_registry = ServiceRegistry::create();
    let recovery_policy_service_handler = RecoveryPolicy::create();
    service_registry
        .lock()
        .await
        .register_service(Rc::new(Mutex::new(recovery_policy_service_handler.clone())));

    // Bring up environment with restore agent and factory reset.
    let env = EnvironmentBuilder::new(Rc::new(InMemoryStorageFactory::new()))
        .service(Box::new(ServiceRegistry::serve(service_registry)))
        .spawn_and_get_protocol_connector(ENV_NAME)
        .await
        .expect("env should be available");

    // Connect to the proxy.
    let factory_reset_proxy = env
        .connect_to_protocol::<FactoryResetMarker>()
        .expect("factory reset service should be available");

    // Validate that an unavailable error is returned.
    assert_matches!(
        factory_reset_proxy.watch().await,
        Err(fidl::Error::ClientChannelClosed {
            status: zx::Status::NOT_FOUND,
            protocol_name: "fuchsia.settings.FactoryReset",
            ..
        })
    );
}
