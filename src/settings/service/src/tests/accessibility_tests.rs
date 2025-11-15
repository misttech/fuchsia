// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::tests::test_failure_utils::create_empty_test_env;
use assert_matches::assert_matches;
use fidl::Error::ClientChannelClosed;
use fidl_fuchsia_settings::*;
use settings_test_common::storage::InMemoryStorageFactory;
use std::rc::Rc;
use zx::Status;

const ENV_NAME: &str = "settings_service_accessibility_test_environment";

// Creates an environment that will fail on a get request.
async fn create_empty_a11y_test_env(
    storage_factory: Rc<InMemoryStorageFactory>,
) -> AccessibilityProxy {
    create_empty_test_env(storage_factory, ENV_NAME)
        .await
        .connect_to_protocol::<AccessibilityMarker>()
        .unwrap()
}

#[fuchsia::test(allow_stalls = false)]
async fn test_channel_failure_watch() {
    let accessibility_proxy =
        create_empty_a11y_test_env(Rc::new(InMemoryStorageFactory::new())).await;
    let result = accessibility_proxy.watch().await;
    assert_matches!(result, Err(ClientChannelClosed { status: Status::NOT_FOUND, .. }));
}
