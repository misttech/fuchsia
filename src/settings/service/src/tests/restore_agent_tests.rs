// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::EnvironmentBuilder;
use crate::agent::AgentCreator;
use crate::base::SettingType;
use crate::handler::base::Request;
use crate::handler::setting_handler::{ControllerError, SettingHandlerResult};
use crate::tests::fakes::base::create_setting_handler;
use futures::lock::Mutex;
use settings_common::config::AgentType;
use settings_test_common::storage::InMemoryStorageFactory;
use std::rc::Rc;

const ENV_NAME: &str = "restore_agent_test_environment";

// Helper function for bringing up an environment with a single handler for a
// single SettingType and validating the environment initialization result.
async fn verify_restore_handling(
    response_generate: Box<dyn Fn() -> SettingHandlerResult>,
    success: bool,
) {
    let counter: Rc<Mutex<u64>> = Rc::new(Mutex::new(0));

    let counter_clone = counter.clone();
    assert_eq!(
        success,
        EnvironmentBuilder::new(Rc::new(InMemoryStorageFactory::new()))
            .handler(
                SettingType::Unknown,
                create_setting_handler(Box::new(move |request| {
                    let counter = counter_clone.clone();
                    if request == Request::Restore {
                        let result = (response_generate)();
                        Box::pin(async move {
                            let mut counter_lock = counter.lock().await;
                            *counter_lock += 1;
                            result
                        })
                    } else {
                        Box::pin(async { Ok(None) })
                    }
                })),
            )
            .agents(vec![AgentCreator::from_type(AgentType::Restore).unwrap()])
            .settings(&[SettingType::Unknown])
            .spawn_nested(ENV_NAME)
            .await
            .is_ok()
    );

    assert_eq!(*counter.lock().await, 1);
}

#[fuchsia::test(allow_stalls = false)]
async fn test_restore() {
    // Should succeed when the restore command is handled.
    verify_restore_handling(Box::new(|| Ok(None)), true).await;

    // Snould succeed when the restore command is explicitly not handled.
    verify_restore_handling(
        Box::new(|| {
            Err(ControllerError::UnimplementedRequest(SettingType::Unknown, Request::Restore))
        }),
        true,
    )
    .await;

    // Should succeed when the setting is not available.
    verify_restore_handling(
        Box::new(|| Err(ControllerError::UnhandledType(SettingType::Unknown))),
        true,
    )
    .await;

    // Snould fail when any other error is introduced.
    verify_restore_handling(
        Box::new(|| Err(ControllerError::UnexpectedError("foo".into()))),
        false,
    )
    .await;
}
