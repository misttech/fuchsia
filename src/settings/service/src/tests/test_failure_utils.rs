// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::EnvironmentBuilder;
use settings_test_common::storage::InMemoryStorageFactory;
use std::rc::Rc;

pub(crate) async fn create_empty_test_env(
    storage_factory: Rc<InMemoryStorageFactory>,
    env_name: &'static str,
) -> fuchsia_component::server::ProtocolConnector {
    EnvironmentBuilder::new(storage_factory)
        .spawn_and_get_protocol_connector(env_name)
        .await
        .unwrap()
}
