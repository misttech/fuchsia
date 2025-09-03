// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::tests::test_failure_utils::create_empty_test_env;
use assert_matches::assert_matches;
use fidl::Error::ClientChannelClosed;
use fidl_fuchsia_settings::{PrivacyMarker, PrivacyProxy};
use settings_test_common::storage::InMemoryStorageFactory;
use std::rc::Rc;
use zx::Status;

const ENV_NAME: &str = "settings_service_privacy_test_environment";

/// Creates an environment that will fail on a get request.
async fn create_privacy_test_env_with_failures() -> PrivacyProxy {
    let storage_factory = InMemoryStorageFactory::new();
    create_empty_test_env(Rc::new(storage_factory), ENV_NAME)
        .await
        .connect_to_protocol::<PrivacyMarker>()
        .unwrap()
}

#[fuchsia::test(allow_stalls = false)]
async fn test_privacy_not_available_when_not_configured() {
    let privacy_service = create_privacy_test_env_with_failures().await;
    let result = privacy_service.watch().await;
    assert_matches!(result, Err(ClientChannelClosed { status: Status::NOT_FOUND, .. }));
}
