// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, Result};
use assert_matches::assert_matches;
use fidl::endpoints::{Proxy, create_endpoints, create_proxy};
use fidl_fuchsia_power_broker::{
    self as fpb, BinaryPowerLevel, DependencyType, ElementSchema, LeaseStatus, LevelDependency,
    StatusMarker, TopologyMarker, TopologyProxy,
};
use fuchsia_async as fasync;
use fuchsia_component_test::{Capability, ChildOptions, RealmBuilder, RealmInstance, Ref, Route};
use futures_util::TryStreamExt;
use power_broker_client::BINARY_POWER_LEVELS;
use std::thread;
use std::time::Duration;
use zx::{self as zx, HandleBased};

async fn build_power_broker_realm() -> Result<RealmInstance, Error> {
    let builder = RealmBuilder::new().await?;
    let power_broker = builder
        .add_child("power_broker", "power-broker#meta/power-broker.cm", ChildOptions::new())
        .await?;
    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol::<TopologyMarker>())
                .from(&power_broker)
                .to(Ref::parent()),
        )
        .await?;
    let realm = builder.build().await?;
    Ok(realm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_power_broker::StatusProxy;
    use fpb::{
        ElementControlMarker, ElementRunnerMarker, ElementRunnerRequestStream, LessorMarker,
    };
    use futures_util::stream::TryNext;

    async fn handle_set_level(
        next: TryNext<'_, ElementRunnerRequestStream>,
    ) -> Result<(u8, fpb::ElementRunnerSetLevelResponder), Error> {
        if let Some(request) = next.await.unwrap() {
            match request {
                fpb::ElementRunnerRequest::SetLevel { level, responder } => {
                    return Ok((level, responder));
                }
                fidl_fuchsia_power_broker::ElementRunnerRequest::_UnknownMethod { .. } => {
                    return Err(Error::msg("ElementRunnerRequest::_UnknownMethod received"));
                }
            }
        } else {
            return Err(Error::msg("ElementRunnerRequest::_UnknownMethod received"));
        }
    }

    /// Verifies that the next ElementRunnerRequest matches the expected required level.
    /// Returns the ElementRunnerSetLevelResponder so that a response can be sent, confirming
    /// the current level.
    async fn assert_set_level_required_eq_and_return_responder(
        next: TryNext<'_, ElementRunnerRequestStream>,
        expect_required: u8,
    ) -> fpb::ElementRunnerSetLevelResponder {
        let (required, current) = handle_set_level(next).await.unwrap();
        assert_eq!(required, expect_required);
        current
    }

    /// Sends the ElementRunner::SetLevel response, then makes a Status::WatchPowerLevel call and
    /// asserts that it matches expect_level.
    async fn assert_send_response_updates_level_to(
        current: fpb::ElementRunnerSetLevelResponder,
        status: &StatusProxy,
        expect_level: u8,
    ) {
        current.send().expect("set_level resp failed");
        let current_level =
            status.watch_power_level().await.unwrap().expect("watch_power_level failed");
        assert_eq!(current_level, expect_level);
    }

    #[fuchsia::test]
    fn test_element_runner_invalid() -> Result<()> {
        let mut executor = fasync::TestExecutor::new();
        let realm = executor.run_singlethreaded(async { build_power_broker_realm().await })?;

        // Create a topology with only two elements and a single dependency:
        // P <- C
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;
        let (_, element_control_server) = create_proxy::<ElementControlMarker>();
        executor.run_singlethreaded(async {
            // Not supplying element_runner is invalid.
            let invalid_resp = topology
                .add_element(ElementSchema {
                    element_name: Some("P".into()),
                    initial_current_level: Some(BinaryPowerLevel::Off.into_primitive()),
                    valid_levels: Some(BINARY_POWER_LEVELS.to_vec()),
                    element_control: Some(element_control_server),
                    ..Default::default()
                })
                .await;
            assert_matches!(invalid_resp, Ok(Err(fpb::AddElementError::Invalid)));
        });
        Ok(())
    }

    #[fuchsia::test]
    fn test_direct() -> Result<()> {
        let mut executor = fasync::TestExecutor::new();
        let realm = executor.run_singlethreaded(async { build_power_broker_realm().await })?;

        // Create a topology with only two elements and a single dependency:
        // P <- C
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;
        let parent_token = zx::Event::create();
        let (parent_element_runner_client, parent_element_runner_server) =
            create_endpoints::<ElementRunnerMarker>();
        let mut parent_element_runner = parent_element_runner_server.into_stream();
        let (parent_element_control, parent_element_control_server) =
            create_proxy::<ElementControlMarker>();
        executor.run_singlethreaded(async {
            assert!(
                topology
                    .add_element(ElementSchema {
                        element_name: Some("P".into()),
                        initial_current_level: Some(BinaryPowerLevel::Off.into_primitive()),
                        valid_levels: Some(BINARY_POWER_LEVELS.to_vec()),
                        element_control: Some(parent_element_control_server),
                        element_runner: Some(parent_element_runner_client),
                        ..Default::default()
                    })
                    .await
                    .is_ok()
            );
            assert!(
                parent_element_control
                    .register_dependency_token(
                        parent_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
                        DependencyType::Assertive,
                    )
                    .await
                    .is_ok()
            );
        });
        let parent_status = {
            let (client, server) = create_proxy::<StatusMarker>();
            parent_element_control.open_status_channel(server)?;
            client
        };
        let (child_element_runner_client, child_element_runner_server) =
            create_endpoints::<ElementRunnerMarker>();
        let mut child_element_runner = child_element_runner_server.into_stream();
        let (child_lessor, lessor_server) = create_proxy::<LessorMarker>();
        let (child_element_control, child_element_control_server) =
            create_proxy::<ElementControlMarker>();
        executor.run_singlethreaded(async {
            assert!(
                topology
                    .add_element(ElementSchema {
                        element_name: Some("C".into()),
                        initial_current_level: Some(BinaryPowerLevel::Off.into_primitive()),
                        valid_levels: Some(BINARY_POWER_LEVELS.to_vec()),
                        dependencies: Some(vec![LevelDependency {
                            dependency_type: DependencyType::Assertive,
                            dependent_level: BinaryPowerLevel::On.into_primitive(),
                            requires_token: parent_token
                                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                .expect("dup failed"),
                            requires_level_by_preference: vec![
                                BinaryPowerLevel::On.into_primitive()
                            ],
                        }]),
                        lessor_channel: Some(lessor_server),
                        element_control: Some(child_element_control_server),
                        element_runner: Some(child_element_runner_client),
                        ..Default::default()
                    })
                    .await
                    .is_ok()
            );
        });
        let child_status = {
            let (client, server) = create_proxy::<StatusMarker>();
            child_element_control.open_status_channel(server)?;
            client
        };

        // Initial required level for P & C should be OFF.
        // Update P & C's current level to OFF with PowerBroker.
        executor.run_singlethreaded(async {
            let parent_current = assert_set_level_required_eq_and_return_responder(
                parent_element_runner.try_next(),
                BinaryPowerLevel::Off.into_primitive(),
            )
            .await;
            let child_current = assert_set_level_required_eq_and_return_responder(
                child_element_runner.try_next(),
                BinaryPowerLevel::Off.into_primitive(),
            )
            .await;
            parent_current.send().expect("set_level resp failed");
            child_current.send().expect("set_level resp failed");
            assert_eq!(
                parent_status.watch_power_level().await.unwrap(),
                Ok(BinaryPowerLevel::Off.into_primitive())
            );
            assert_eq!(
                child_status.watch_power_level().await.unwrap(),
                Ok(BinaryPowerLevel::Off.into_primitive())
            );
        });

        // Attempt to update with invalid level, this should fail.
        executor.run_singlethreaded(async {
            assert!(child_lessor.lease(100).await.unwrap().is_err());
        });

        // Acquire lease for C.
        // P's required level should become ON.
        // C's required level should remain OFF until P turns ON.
        // Lease should be pending.
        let lease = executor.run_singlethreaded(async {
            child_lessor
                .lease(BinaryPowerLevel::On.into_primitive())
                .await
                .unwrap()
                .expect("Lease response not ok")
                .into_proxy()
        });
        let parent_current = executor.run_singlethreaded(async {
            assert_set_level_required_eq_and_return_responder(
                parent_element_runner.try_next(),
                BinaryPowerLevel::On.into_primitive(),
            )
            .await
        });
        let mut child_element_runner_next = child_element_runner.try_next();
        assert!(executor.run_until_stalled(&mut child_element_runner_next).is_pending());
        executor.run_singlethreaded(async {
            assert_eq!(
                lease.watch_status(LeaseStatus::Unknown).await.unwrap(),
                LeaseStatus::Pending
            );
        });

        // Update P's current level to ON.
        // P's required level should remain ON.
        // C's required level should become ON.
        executor.run_singlethreaded(async {
            parent_current.send().expect("set_level resp failed");
            assert_eq!(
                parent_status.watch_power_level().await.unwrap(),
                Ok(BinaryPowerLevel::On.into_primitive())
            );
        });
        let mut parent_element_runner_next = parent_element_runner.try_next();
        assert!(executor.run_until_stalled(&mut parent_element_runner_next).is_pending());
        let child_current = executor.run_singlethreaded(async {
            assert_set_level_required_eq_and_return_responder(
                child_element_runner.try_next(),
                BinaryPowerLevel::On.into_primitive(),
            )
            .await
        });

        // Update C's current level to ON.
        // Lease should become satisfied.
        executor.run_singlethreaded(async {
            child_current.send().expect("set_level resp failed");
            assert_eq!(
                child_status.watch_power_level().await.unwrap(),
                Ok(BinaryPowerLevel::On.into_primitive())
            );
            assert_eq!(
                lease.watch_status(LeaseStatus::Unknown).await.unwrap(),
                LeaseStatus::Satisfied
            );
        });

        // Drop lease.
        // C's required level should become OFF.
        let child_current = executor.run_singlethreaded(async {
            drop(lease);
            assert_set_level_required_eq_and_return_responder(
                child_element_runner.try_next(),
                BinaryPowerLevel::Off.into_primitive(),
            )
            .await
        });

        // Update C's current level to OFF.
        // P's required level should become OFF.
        let parent_current = executor.run_singlethreaded(async {
            child_current.send().expect("set_level resp failed");
            assert_eq!(
                child_status.watch_power_level().await.unwrap(),
                Ok(BinaryPowerLevel::Off.into_primitive())
            );
            assert_set_level_required_eq_and_return_responder(
                parent_element_runner.try_next(),
                BinaryPowerLevel::Off.into_primitive(),
            )
            .await
        });

        // Update P's current level to OFF.
        executor.run_singlethreaded(async {
            parent_current.send().expect("set_level resp failed");
            assert_eq!(
                parent_status.watch_power_level().await.unwrap(),
                Ok(BinaryPowerLevel::Off.into_primitive())
            );
        });

        // Remove P's element. Status channel should be closed.
        executor.run_singlethreaded(async {
            drop(parent_element_control);
            let status_after_remove = parent_status.watch_power_level().await;
            assert_matches!(status_after_remove, Err(fidl::Error::ClientChannelClosed { .. }));
        });
        // Remove C's element. Status channel should be closed.
        executor.run_singlethreaded(async {
            drop(child_element_control);
            let status_after_remove = child_status.watch_power_level().await;
            assert_matches!(status_after_remove, Err(fidl::Error::ClientChannelClosed { .. }));
        });

        Ok(())
    }

    #[fuchsia::test]
    fn test_transitive() -> Result<()> {
        let mut executor = fasync::TestExecutor::new();
        let realm = executor.run_singlethreaded(async { build_power_broker_realm().await })?;

        // Create a four element topology with the following dependencies:
        // C depends on B, which in turn depends on A.
        // D has no dependencies or dependents.
        // A <- B <- C   D
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;
        let element_a_token = zx::Event::create();
        let (element_a_runner_client, element_a_runner_server) =
            create_endpoints::<ElementRunnerMarker>();
        let mut element_a_runner = element_a_runner_server.into_stream();
        let (element_a_element_control, element_control_server) =
            create_proxy::<ElementControlMarker>();
        executor.run_singlethreaded(async {
            assert!(
                topology
                    .add_element(ElementSchema {
                        element_name: Some("A".into()),
                        initial_current_level: Some(BinaryPowerLevel::Off.into_primitive()),
                        valid_levels: Some(BINARY_POWER_LEVELS.to_vec()),
                        element_control: Some(element_control_server),
                        element_runner: Some(element_a_runner_client),
                        ..Default::default()
                    })
                    .await
                    .is_ok()
            );
            assert!(
                element_a_element_control
                    .register_dependency_token(
                        element_a_token
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed"),
                        DependencyType::Assertive,
                    )
                    .await
                    .is_ok()
            );
        });
        let element_a_status = {
            let (client, server) = create_endpoints::<StatusMarker>();
            element_a_element_control.open_status_channel(server)?;
            client.into_proxy()
        };
        let element_b_token = zx::Event::create();
        let (element_b_runner_client, element_b_runner_server) =
            create_endpoints::<ElementRunnerMarker>();
        let mut element_b_runner = element_b_runner_server.into_stream();
        let (element_b_element_control, element_control_server) =
            create_proxy::<ElementControlMarker>();
        executor.run_singlethreaded(async {
            assert!(
                topology
                    .add_element(ElementSchema {
                        element_name: Some("B".into()),
                        initial_current_level: Some(BinaryPowerLevel::Off.into_primitive()),
                        valid_levels: Some(BINARY_POWER_LEVELS.to_vec()),
                        dependencies: Some(vec![LevelDependency {
                            dependency_type: DependencyType::Assertive,
                            dependent_level: BinaryPowerLevel::On.into_primitive(),
                            requires_token: element_a_token
                                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                .expect("dup failed"),
                            requires_level_by_preference: vec![
                                BinaryPowerLevel::On.into_primitive()
                            ],
                        }]),
                        element_control: Some(element_control_server),
                        element_runner: Some(element_b_runner_client),
                        ..Default::default()
                    })
                    .await
                    .is_ok()
            );
            assert!(
                element_b_element_control
                    .register_dependency_token(
                        element_b_token
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed"),
                        DependencyType::Assertive,
                    )
                    .await
                    .is_ok()
            );
        });
        let element_b_status: fpb::StatusProxy = {
            let (client, server) = create_endpoints::<StatusMarker>();
            element_b_element_control.open_status_channel(server)?;
            client.into_proxy()
        };
        let (element_c_runner_client, element_c_runner_server) =
            create_endpoints::<ElementRunnerMarker>();
        let mut element_c_runner = element_c_runner_server.into_stream();
        let (element_c_lessor, lessor_server) = create_proxy::<LessorMarker>();
        let (element_c_element_control, element_control_server) =
            create_proxy::<ElementControlMarker>();
        executor.run_singlethreaded(async {
            assert!(
                topology
                    .add_element(ElementSchema {
                        element_name: Some("C".into()),
                        initial_current_level: Some(BinaryPowerLevel::Off.into_primitive()),
                        valid_levels: Some(BINARY_POWER_LEVELS.to_vec()),
                        dependencies: Some(vec![LevelDependency {
                            dependency_type: DependencyType::Assertive,
                            dependent_level: BinaryPowerLevel::On.into_primitive(),
                            requires_token: element_b_token
                                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                .expect("dup failed"),
                            requires_level_by_preference: vec![
                                BinaryPowerLevel::On.into_primitive()
                            ],
                        }]),
                        lessor_channel: Some(lessor_server),
                        element_control: Some(element_control_server),
                        element_runner: Some(element_c_runner_client),
                        ..Default::default()
                    })
                    .await
                    .is_ok()
            );
        });
        let element_c_status: fpb::StatusProxy = {
            let (client, server) = create_endpoints::<StatusMarker>();
            element_c_element_control.open_status_channel(server)?;
            client.into_proxy()
        };
        let (element_d_runner_client, element_d_runner_server) =
            create_endpoints::<ElementRunnerMarker>();
        let mut element_d_runner = element_d_runner_server.into_stream();
        let (element_d_element_control, element_control_server) =
            create_proxy::<ElementControlMarker>();
        executor.run_singlethreaded(async {
            assert!(
                topology
                    .add_element(ElementSchema {
                        element_name: Some("D".into()),
                        initial_current_level: Some(BinaryPowerLevel::Off.into_primitive()),
                        valid_levels: Some(BINARY_POWER_LEVELS.to_vec()),
                        element_control: Some(element_control_server),
                        element_runner: Some(element_d_runner_client),
                        ..Default::default()
                    })
                    .await
                    .is_ok()
            );
        });
        let element_d_status: fpb::StatusProxy = {
            let (client, server) = create_endpoints::<StatusMarker>();
            element_d_element_control.open_status_channel(server)?;
            client.into_proxy()
        };

        // Initial required level for each element should be OFF.
        // Set managed elements' current level to OFF.
        for (status, runner) in [
            (&element_a_status, &mut element_a_runner),
            (&element_b_status, &mut element_b_runner),
            (&element_c_status, &mut element_c_runner),
            (&element_d_status, &mut element_d_runner),
        ] {
            executor.run_singlethreaded(async {
                let current = assert_set_level_required_eq_and_return_responder(
                    runner.try_next(),
                    BinaryPowerLevel::Off.into_primitive(),
                )
                .await;
                current.send().expect("set_level resp failed");
                let power_level =
                    status.watch_power_level().await.unwrap().expect("watch_power_level failed");
                assert_eq!(power_level, BinaryPowerLevel::Off.into_primitive());
            });
        }
        let element_a_runner_next = element_a_runner.try_next();
        let mut element_b_runner_next = element_b_runner.try_next();
        let mut element_c_runner_next = element_c_runner.try_next();
        let mut element_d_runner_next = element_d_runner.try_next();

        // Acquire lease for C.
        // A's required level should become ON.
        // B's required level should remain OFF because A is not yet ON.
        // C's required level should remain OFF because B is not yet ON.
        // D's required level should remain OFF.
        let lease = executor.run_singlethreaded(async {
            element_c_lessor
                .lease(BinaryPowerLevel::On.into_primitive())
                .await
                .unwrap()
                .expect("Lease response not ok")
                .into_proxy()
        });
        let element_a_current = executor.run_singlethreaded(async {
            assert_set_level_required_eq_and_return_responder(
                element_a_runner_next,
                BinaryPowerLevel::On.into_primitive(),
            )
            .await
        });
        let mut element_a_runner_next = element_a_runner.try_next();
        assert!(executor.run_until_stalled(&mut element_b_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut element_c_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut element_d_runner_next).is_pending());
        executor.run_singlethreaded(async {
            assert_eq!(
                lease.watch_status(LeaseStatus::Unknown).await.unwrap(),
                LeaseStatus::Pending
            );
        });

        // Update A's current level to ON.
        // A's required level should remain ON.
        // B's required level should become ON because A is now ON.
        // C's required level should remain OFF because B is not yet ON.
        // D's required level should remain OFF.
        executor.run_singlethreaded(async {
            assert_send_response_updates_level_to(
                element_a_current,
                &element_a_status,
                BinaryPowerLevel::On.into_primitive(),
            )
            .await;
        });
        assert!(executor.run_until_stalled(&mut element_a_runner_next).is_pending());
        let element_b_current = executor.run_singlethreaded(async {
            assert_set_level_required_eq_and_return_responder(
                element_b_runner_next,
                BinaryPowerLevel::On.into_primitive(),
            )
            .await
        });
        let mut element_b_runner_next = element_b_runner.try_next();
        assert!(executor.run_until_stalled(&mut element_c_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut element_d_runner_next).is_pending());

        // Update B's current level to ON.
        // A's required level should remain ON.
        // B's required level should remain ON.
        // C's required level should become ON because B is now ON.
        // D's required level should remain OFF.
        executor.run_singlethreaded(async {
            assert_send_response_updates_level_to(
                element_b_current,
                &element_b_status,
                BinaryPowerLevel::On.into_primitive(),
            )
            .await;
        });
        assert!(executor.run_until_stalled(&mut element_a_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut element_b_runner_next).is_pending());
        let element_c_current = executor.run_singlethreaded(async {
            assert_set_level_required_eq_and_return_responder(
                element_c_runner_next,
                BinaryPowerLevel::On.into_primitive(),
            )
            .await
        });
        let mut element_c_runner_next = element_c_runner.try_next();
        assert!(executor.run_until_stalled(&mut element_d_runner_next).is_pending());

        // Update C's current level to ON.
        // A's required level should remain ON.
        // B's required level should remain ON.
        // C's required level should remain ON.
        // D's required level should remain OFF.
        // Lease should become satisfied.
        executor.run_singlethreaded(async {
            assert_send_response_updates_level_to(
                element_c_current,
                &element_c_status,
                BinaryPowerLevel::On.into_primitive(),
            )
            .await;
        });
        assert!(executor.run_until_stalled(&mut element_a_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut element_b_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut element_c_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut element_d_runner_next).is_pending());
        executor.run_singlethreaded(async {
            assert_eq!(
                lease.watch_status(LeaseStatus::Unknown).await.unwrap(),
                LeaseStatus::Satisfied
            );
        });

        // Drop lease for C.
        // A's required level should remain ON.
        // B's required level should remain ON.
        // C's required level should become OFF because the lease was dropped.
        // D's required level should remain OFF.
        executor.run_singlethreaded(async {
            drop(lease);
        });
        assert!(executor.run_until_stalled(&mut element_a_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut element_b_runner_next).is_pending());
        let element_c_current = executor.run_singlethreaded(async {
            assert_set_level_required_eq_and_return_responder(
                element_c_runner_next,
                BinaryPowerLevel::Off.into_primitive(),
            )
            .await
        });
        let mut element_c_runner_next = element_c_runner.try_next();
        assert!(executor.run_until_stalled(&mut element_d_runner_next).is_pending());

        // Lower C's current level to OFF.
        // A's required level should remain ON.
        // B's required level should become OFF.
        // C's required level should remain OFF.
        // D's required level should remain OFF.
        executor.run_singlethreaded(async {
            assert_send_response_updates_level_to(
                element_c_current,
                &element_c_status,
                BinaryPowerLevel::Off.into_primitive(),
            )
            .await;
        });
        assert!(executor.run_until_stalled(&mut element_a_runner_next).is_pending());
        let element_b_current = executor.run_singlethreaded(async {
            assert_set_level_required_eq_and_return_responder(
                element_b_runner_next,
                BinaryPowerLevel::Off.into_primitive(),
            )
            .await
        });
        let mut element_b_runner_next = element_b_runner.try_next();
        assert!(executor.run_until_stalled(&mut element_c_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut element_d_runner_next).is_pending());

        // Lower B's current level to OFF.
        // A's required level should become OFF because B is no longer dependent.
        // B's required level should remain OFF.
        // C's required level should remain OFF.
        // D's required level should remain OFF.
        executor.run_singlethreaded(async {
            assert_send_response_updates_level_to(
                element_b_current,
                &element_b_status,
                BinaryPowerLevel::Off.into_primitive(),
            )
            .await;
        });
        executor.run_singlethreaded(async {
            let current = assert_set_level_required_eq_and_return_responder(
                element_a_runner_next,
                BinaryPowerLevel::Off.into_primitive(),
            )
            .await;
            assert_send_response_updates_level_to(
                current,
                &element_a_status,
                BinaryPowerLevel::Off.into_primitive(),
            )
            .await;
        });
        assert!(executor.run_until_stalled(&mut element_b_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut element_c_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut element_d_runner_next).is_pending());

        Ok(())
    }

    #[fuchsia::test]
    fn test_shared() -> Result<()> {
        // Create a topology of two child elements (C1 & C2) with a shared
        // parent (P) and grandparent (GP)
        // C1 \
        //     > P -> GP
        // C2 /
        // Child 1 requires Parent at 50 to support its own level of 5.
        // Parent requires Grandparent at 200 to support its own level of 50.
        // C1 -> P -> GP
        //  5 -> 50 -> 200
        // Child 2 requires Parent at 30 to support its own level of 3.
        // Parent requires Grandparent at 90 to support its own level of 30.
        // C2 -> P -> GP
        //  3 -> 30 -> 90
        // Grandparent has a default minimum level of 10.
        // All other elements have a default of 0.
        let mut executor = fasync::TestExecutor::new();
        let realm = executor.run_singlethreaded(async { build_power_broker_realm().await })?;
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;
        let grandparent_token = zx::Event::create();
        let (grandparent_runner_client, grandparent_runner_server) =
            create_endpoints::<ElementRunnerMarker>();
        let mut grandparent_runner = grandparent_runner_server.into_stream();
        let (grandparent_element_control, element_control_server) =
            create_proxy::<ElementControlMarker>();
        executor.run_singlethreaded(async {
            assert!(
                topology
                    .add_element(ElementSchema {
                        element_name: Some("GP".into()),
                        initial_current_level: Some(10),
                        valid_levels: Some(vec![10, 90, 200]),
                        element_control: Some(element_control_server),
                        element_runner: Some(grandparent_runner_client),
                        ..Default::default()
                    })
                    .await
                    .is_ok()
            );
            assert!(
                grandparent_element_control
                    .register_dependency_token(
                        grandparent_token
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed"),
                        DependencyType::Assertive,
                    )
                    .await
                    .is_ok()
            );
        });
        let grandparent_status: fpb::StatusProxy = {
            let (client, server) = create_endpoints::<StatusMarker>();
            grandparent_element_control.open_status_channel(server)?;
            client.into_proxy()
        };
        let parent_token = zx::Event::create();
        let (parent_runner_client, parent_runner_server) =
            create_endpoints::<ElementRunnerMarker>();
        let mut parent_runner = parent_runner_server.into_stream();
        let (parent_element_control, element_control_server) =
            create_proxy::<ElementControlMarker>();
        executor.run_singlethreaded(async {
            assert!(
                topology
                    .add_element(ElementSchema {
                        element_name: Some("P".into()),
                        initial_current_level: Some(0),
                        valid_levels: Some(vec![0, 30, 50]),
                        dependencies: Some(vec![
                            LevelDependency {
                                dependency_type: DependencyType::Assertive,
                                dependent_level: 50,
                                requires_token: grandparent_token
                                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                    .expect("dup failed"),
                                requires_level_by_preference: vec![200],
                            },
                            LevelDependency {
                                dependency_type: DependencyType::Assertive,
                                dependent_level: 30,
                                requires_token: grandparent_token
                                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                    .expect("dup failed"),
                                requires_level_by_preference: vec![90],
                            },
                        ]),
                        element_control: Some(element_control_server),
                        element_runner: Some(parent_runner_client),
                        ..Default::default()
                    })
                    .await
                    .is_ok()
            );
            assert!(
                parent_element_control
                    .register_dependency_token(
                        parent_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
                        DependencyType::Assertive,
                    )
                    .await
                    .is_ok()
            );
        });
        let parent_status: fpb::StatusProxy = {
            let (client, server) = create_endpoints::<StatusMarker>();
            parent_element_control.open_status_channel(server)?;
            client.into_proxy()
        };
        let (child1_runner_client, child1_runner_server) =
            create_endpoints::<ElementRunnerMarker>();
        let mut child1_runner = child1_runner_server.into_stream();
        let (child1_lessor, lessor_server) = create_proxy::<LessorMarker>();
        let (child1_element_control, element_control_server) =
            create_proxy::<ElementControlMarker>();
        executor.run_singlethreaded(async {
            assert!(
                topology
                    .add_element(ElementSchema {
                        element_name: Some("C1".into()),
                        initial_current_level: Some(0),
                        valid_levels: Some(vec![0, 5]),
                        dependencies: Some(vec![LevelDependency {
                            dependency_type: DependencyType::Assertive,
                            dependent_level: 5,
                            requires_token: parent_token
                                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                .expect("dup failed"),
                            requires_level_by_preference: vec![50],
                        }]),
                        lessor_channel: Some(lessor_server),
                        element_control: Some(element_control_server),
                        element_runner: Some(child1_runner_client),
                        ..Default::default()
                    })
                    .await
                    .is_ok()
            );
        });
        let child1_status: fpb::StatusProxy = {
            let (client, server) = create_endpoints::<StatusMarker>();
            child1_element_control.open_status_channel(server)?;
            client.into_proxy()
        };
        let (child2_runner_client, child2_runner_server) =
            create_endpoints::<ElementRunnerMarker>();
        let mut child2_runner = child2_runner_server.into_stream();
        let (child2_lessor, lessor_server) = create_proxy::<LessorMarker>();
        let (child2_element_control, element_control_server) =
            create_proxy::<ElementControlMarker>();
        executor.run_singlethreaded(async {
            assert!(
                topology
                    .add_element(ElementSchema {
                        element_name: Some("C2".into()),
                        initial_current_level: Some(0),
                        valid_levels: Some(vec![0, 3]),
                        dependencies: Some(vec![LevelDependency {
                            dependency_type: DependencyType::Assertive,
                            dependent_level: 3,
                            requires_token: parent_token
                                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                .expect("dup failed"),
                            requires_level_by_preference: vec![30],
                        }]),
                        lessor_channel: Some(lessor_server),
                        element_control: Some(element_control_server),
                        element_runner: Some(child2_runner_client),
                        ..Default::default()
                    })
                    .await
                    .is_ok()
            );
        });
        let child2_status: fpb::StatusProxy = {
            let (client, server) = create_endpoints::<StatusMarker>();
            child2_element_control.open_status_channel(server)?;
            client.into_proxy()
        };

        // GP should have a initial required level of 10
        // P, C1 and C2 should have initial required levels of 0.
        executor.run_singlethreaded(async {
            let grandparent_current = assert_set_level_required_eq_and_return_responder(
                grandparent_runner.try_next(),
                10,
            )
            .await;
            grandparent_current.send().expect("set_level resp failed");
            assert_eq!(grandparent_status.watch_power_level().await.unwrap(), Ok(10));

            let parent_current =
                assert_set_level_required_eq_and_return_responder(parent_runner.try_next(), 0)
                    .await;
            parent_current.send().expect("set_level resp failed");
            assert_eq!(parent_status.watch_power_level().await.unwrap(), Ok(0));

            let child1_current =
                assert_set_level_required_eq_and_return_responder(child1_runner.try_next(), 0)
                    .await;
            child1_current.send().expect("set_level resp failed");
            assert_eq!(child1_status.watch_power_level().await.unwrap(), Ok(0));

            let child2_current =
                assert_set_level_required_eq_and_return_responder(child2_runner.try_next(), 0)
                    .await;
            child2_current.send().expect("set_level resp failed");
            assert_eq!(child2_status.watch_power_level().await.unwrap(), Ok(0));
        });
        let grandparent_runner_next = grandparent_runner.try_next();
        let mut parent_runner_next = parent_runner.try_next();
        let mut child1_runner_next = child1_runner.try_next();
        let mut child2_runner_next = child2_runner.try_next();

        // Acquire lease for C1 @ 5.
        // GP's required level should become 200 because C1 @ 5 has a
        // dependency on P @ 50 and P @ 50 has a dependency on GP @ 200.
        // GP @ 200 has no dependencies so its level should be raised first.
        // P's required level should remain 0 because GP is not yet at 200.
        // C1's required level should remain 0 because P is not yet at 50.
        // C2's required level should remain 0.
        let lease_child_1 = executor.run_singlethreaded(async {
            child1_lessor.lease(5).await.unwrap().expect("Lease response not ok").into_proxy()
        });
        let grandparent_current = executor.run_singlethreaded(async {
            assert_set_level_required_eq_and_return_responder(grandparent_runner_next, 200).await
        });
        let mut grandparent_runner_next = grandparent_runner.try_next();
        assert!(executor.run_until_stalled(&mut parent_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child1_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child2_runner_next).is_pending());
        executor.run_singlethreaded(async {
            assert_eq!(
                lease_child_1.watch_status(LeaseStatus::Unknown).await.unwrap(),
                LeaseStatus::Pending
            );
        });

        // Raise GP's current level to 200.
        // GP's required level should remain 200.
        // P's required level should become 50 because GP is now at 200.
        // C1's required level should remain 0 because P is not yet at 50.
        // C2's required level should remain 0.
        executor.run_singlethreaded(async {
            grandparent_current.send().expect("set_level resp failed");
            assert_eq!(grandparent_status.watch_power_level().await.unwrap(), Ok(200));
        });
        let parent_current = executor.run_singlethreaded(async {
            assert_set_level_required_eq_and_return_responder(parent_runner_next, 50).await
        });
        let mut parent_runner_next = parent_runner.try_next();
        assert!(executor.run_until_stalled(&mut child1_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child2_runner_next).is_pending());
        executor.run_singlethreaded(async {
            assert_eq!(
                lease_child_1.watch_status(LeaseStatus::Unknown).await.unwrap(),
                LeaseStatus::Pending
            );
        });

        // Update P's current level to 50.
        // GP's required level should remain 200.
        // P's required level should remain 50.
        // C1's required level should become 5 because P is now at 50.
        // C2's required level should remain 0.
        executor.run_singlethreaded(async {
            parent_current.send().expect("set_level resp failed");
            assert_eq!(parent_status.watch_power_level().await.unwrap(), Ok(50));
        });
        let child1_current = executor.run_singlethreaded(async {
            assert_set_level_required_eq_and_return_responder(child1_runner_next, 5).await
        });
        let mut child1_runner_next = child1_runner.try_next();
        assert!(executor.run_until_stalled(&mut child2_runner_next).is_pending());

        // Update C1's current level to 5.
        // GP's required level should remain 200.
        // P's required level should remain 50.
        // C1's required level should remain 5.
        // C2's required level should remain 0.
        // C1's lease @ 5 is now satisfied.
        executor.run_singlethreaded(async {
            child1_current.send().expect("set_level resp failed");
            assert_eq!(child1_status.watch_power_level().await.unwrap(), Ok(5));
        });
        assert!(executor.run_until_stalled(&mut grandparent_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut parent_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child1_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child2_runner_next).is_pending());
        executor.run_singlethreaded(async {
            assert_eq!(
                lease_child_1.watch_status(LeaseStatus::Pending).await.unwrap(),
                LeaseStatus::Satisfied
            );
        });

        // Acquire lease for C2 @ 3.
        // Though C2 @ 3 has nominal requirements of P @ 30 and GP @ 90,
        // they are superseded by C1's requirements of 50 and 200.
        // GP's required level should remain 200.
        // P's required level should remain 50.
        // C1's required level should remain 5.
        // C2's required level should become 3 because its dependencies are already satisfied.
        // C1's lease @ 5 is still satisfied.
        let lease_child_2 = executor.run_singlethreaded(async {
            child2_lessor.lease(3).await.unwrap().expect("Lease response not ok").into_proxy()
        });
        assert!(executor.run_until_stalled(&mut grandparent_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut parent_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child1_runner_next).is_pending());
        let child2_current = executor.run_singlethreaded(async {
            assert_set_level_required_eq_and_return_responder(child2_runner_next, 3).await
        });
        let mut child2_runner_next = child2_runner.try_next();
        executor.run_singlethreaded(async {
            assert_eq!(
                lease_child_1.watch_status(LeaseStatus::Unknown).await.unwrap(),
                LeaseStatus::Satisfied
            );
        });

        // Update C2's current level to 3.
        // GP's required level should remain 200.
        // P's required level should remain 50.
        // C1's required level should remain 5.
        // C2's required level should remain 0.
        // C1's lease @ 5 is still satisfied.
        // C2's lease @ 3 is now satisfied.
        executor.run_singlethreaded(async {
            child2_current.send().expect("set_level resp failed");
            assert_eq!(child2_status.watch_power_level().await.unwrap(), Ok(3));
        });
        assert!(executor.run_until_stalled(&mut grandparent_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut parent_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child1_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child2_runner_next).is_pending());
        executor.run_singlethreaded(async {
            assert_eq!(
                lease_child_1.watch_status(LeaseStatus::Unknown).await.unwrap(),
                LeaseStatus::Satisfied
            );
            assert_eq!(
                lease_child_2.watch_status(LeaseStatus::Pending).await.unwrap(),
                LeaseStatus::Satisfied
            );
        });
        // Drop lease for C1.
        // GP's required level should remain 200.
        // P's required level should remain 50.
        // C1's required level should become 0 because its lease has been dropped.
        // C2's required level should remain 3.
        // C2's lease @ 3 is still satisfied.
        let child1_current = executor.run_singlethreaded(async {
            drop(lease_child_1);
            assert_set_level_required_eq_and_return_responder(child1_runner_next, 0).await
        });
        assert!(executor.run_until_stalled(&mut grandparent_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut parent_runner_next).is_pending());
        let mut child1_runner_next = child1_runner.try_next();
        assert!(executor.run_until_stalled(&mut child2_runner_next).is_pending());
        executor.run_singlethreaded(async {
            assert_eq!(
                lease_child_2.watch_status(LeaseStatus::Unknown).await.unwrap(),
                LeaseStatus::Satisfied
            );
        });

        // Lower C1's current level to 0.
        // GP's required level should remain 200.
        // P's required level should become 30.
        // C1's required level should remain 0.
        // C2's required level should remain 3.
        // C2's lease @ 3 is still satisfied.
        let parent_current = executor.run_singlethreaded(async {
            child1_current.send().expect("set_level resp failed");
            assert_eq!(child1_status.watch_power_level().await.unwrap(), Ok(0));
            assert_set_level_required_eq_and_return_responder(parent_runner_next, 30).await
        });
        let mut parent_runner_next = parent_runner.try_next();
        assert!(executor.run_until_stalled(&mut grandparent_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child1_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child2_runner_next).is_pending());
        executor.run_singlethreaded(async {
            assert_eq!(
                lease_child_2.watch_status(LeaseStatus::Unknown).await.unwrap(),
                LeaseStatus::Satisfied
            );
        });

        // Lower Parent's current level to 30.
        // GP's required level should become 90 because P has dropped to 30.
        // P's required level should remain 30.
        // C1's required level should remain 0.
        // C2's required level should remain 3.
        // C2's lease @ 3 is still satisfied.
        let grandparent_current = executor.run_singlethreaded(async {
            parent_current.send().expect("set_level resp failed");
            assert_eq!(parent_status.watch_power_level().await.unwrap(), Ok(30));
            assert_set_level_required_eq_and_return_responder(grandparent_runner_next, 90).await
        });
        let mut grandparent_runner_next = grandparent_runner.try_next();
        assert!(executor.run_until_stalled(&mut parent_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child1_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child2_runner_next).is_pending());
        executor.run_singlethreaded(async {
            assert_eq!(
                lease_child_2.watch_status(LeaseStatus::Unknown).await.unwrap(),
                LeaseStatus::Satisfied
            );
        });

        // Drop lease for Child 2.
        // GP's required level should remain 90 because P is still at 30.
        // P's required level should remain 30.
        // C1's required level should remain 0.
        // C2's required level should become 0 because its lease has been dropped.
        let child2_current = executor.run_singlethreaded(async {
            drop(lease_child_2);
            assert_set_level_required_eq_and_return_responder(child2_runner_next, 0).await
        });
        assert!(executor.run_until_stalled(&mut grandparent_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut parent_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child1_runner_next).is_pending());
        let mut child2_runner_next = child2_runner.try_next();

        // Lower GP's current level to 90.
        // GP's required level should remain 90 because P is still at 30.
        // P's required level should remain 30.
        // C1's required level should remain 0.
        // C2's required level should remain 0.
        executor.run_singlethreaded(async {
            grandparent_current.send().expect("set_level resp failed");
            assert_eq!(grandparent_status.watch_power_level().await.unwrap(), Ok(90));
        });
        assert!(executor.run_until_stalled(&mut grandparent_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut parent_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child1_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child2_runner_next).is_pending());

        // Lower C2's current level to 0.
        // GP's required level should remain 90 because P is still at 30.
        // P's required level should become 0.
        // C1's required level should remain 0.
        // C2's required level should remain 0.
        let parent_current = executor.run_singlethreaded(async {
            child2_current.send().expect("set_level resp failed");
            assert_eq!(child2_status.watch_power_level().await.unwrap(), Ok(0));
            assert_set_level_required_eq_and_return_responder(parent_runner_next, 0).await
        });
        let mut parent_runner_next = parent_runner.try_next();
        assert!(executor.run_until_stalled(&mut grandparent_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child1_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child2_runner_next).is_pending());

        // Lower Parent's current level to 0.
        // GP's required level should become its minimum level of 10 because P is now at 0.
        // P's required level should remain 0.
        // C1's required level should remain 0.
        // C2's required level should remain 0.
        let grandparent_current = executor.run_singlethreaded(async {
            parent_current.send().expect("set_level resp failed");
            assert_eq!(parent_status.watch_power_level().await.unwrap(), Ok(0));
            assert_set_level_required_eq_and_return_responder(grandparent_runner_next, 10).await
        });
        let mut grandparent_runner_next = grandparent_runner.try_next();
        assert!(executor.run_until_stalled(&mut parent_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child1_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child2_runner_next).is_pending());

        // Lower GP's current level to 10.
        // GP's required level should remain 10.
        // P's required level should remain 0.
        // C1's required level should remain 0.
        // C2's required level should remain 0.
        executor.run_singlethreaded(async {
            grandparent_current.send().expect("set_level resp failed");
            assert_eq!(grandparent_status.watch_power_level().await.unwrap(), Ok(10));
        });
        assert!(executor.run_until_stalled(&mut grandparent_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut parent_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child1_runner_next).is_pending());
        assert!(executor.run_until_stalled(&mut child2_runner_next).is_pending());

        Ok(())
    }

    #[fuchsia::test]
    async fn test_add_element_errors() -> Result<(), Error> {
        let realm = build_power_broker_realm().await?;
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;

        // Create a root element
        let earth_token = zx::Event::create();
        let (element_runner_client, _) = create_endpoints::<ElementRunnerMarker>();
        let (element_control, element_control_server) = create_proxy::<ElementControlMarker>();
        assert!(
            topology
                .add_element(ElementSchema {
                    element_name: Some("Earth".into()),
                    initial_current_level: Some(BinaryPowerLevel::Off.into_primitive()),
                    valid_levels: Some(BINARY_POWER_LEVELS.to_vec()),
                    element_control: Some(element_control_server),
                    element_runner: Some(element_runner_client),
                    ..Default::default()
                })
                .await
                .is_ok()
        );
        assert!(
            element_control
                .register_dependency_token(
                    earth_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
                    DependencyType::Assertive,
                )
                .await
                .is_ok()
        );

        // Using an unauthorized dependency token yields AddElementError::NotAuthorized.
        let (element_runner_client, _) = create_endpoints::<ElementRunnerMarker>();
        assert_matches!(
            topology
                .add_element(ElementSchema {
                    element_name: Some("Water".into()),
                    initial_current_level: Some(BinaryPowerLevel::Off.into_primitive()),
                    valid_levels: Some(BINARY_POWER_LEVELS.to_vec()),
                    dependencies: Some(vec![LevelDependency {
                        dependency_type: DependencyType::Assertive,
                        dependent_level: BinaryPowerLevel::On.into_primitive(),
                        requires_token: zx::Event::create(),
                        requires_level_by_preference: vec![BinaryPowerLevel::On.into_primitive()],
                    }]),
                    element_runner: Some(element_runner_client),
                    ..Default::default()
                })
                .await,
            Ok(Err(fpb::AddElementError::NotAuthorized))
        );

        // Using an invalid level in a dependency yields AddElementError::Invalid.
        let (element_runner_client, _) = create_endpoints::<ElementRunnerMarker>();
        assert_matches!(
            topology
                .add_element(ElementSchema {
                    element_name: Some("Air".into()),
                    initial_current_level: Some(BinaryPowerLevel::Off.into_primitive()),
                    valid_levels: Some(BINARY_POWER_LEVELS.to_vec()),
                    dependencies: Some(vec![LevelDependency {
                        dependency_type: DependencyType::Assertive,
                        dependent_level: BinaryPowerLevel::On.into_primitive(),
                        requires_token: earth_token
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed"),
                        requires_level_by_preference: vec![2],
                    }]),
                    element_runner: Some(element_runner_client),
                    ..Default::default()
                })
                .await,
            Ok(Err(fpb::AddElementError::Invalid))
        );

        // Using the correct dependency token succeeds.
        let (element_runner_client, _) = create_endpoints::<ElementRunnerMarker>();
        assert!(
            topology
                .add_element(ElementSchema {
                    element_name: Some("Fire".into()),
                    initial_current_level: Some(BinaryPowerLevel::Off.into_primitive()),
                    valid_levels: Some(BINARY_POWER_LEVELS.to_vec()),
                    dependencies: Some(vec![LevelDependency {
                        dependency_type: DependencyType::Assertive,
                        dependent_level: BinaryPowerLevel::On.into_primitive(),
                        requires_token: earth_token
                            .duplicate_handle(zx::Rights::SAME_RIGHTS)
                            .expect("dup failed"),
                        requires_level_by_preference: vec![BinaryPowerLevel::On.into_primitive()],
                    }]),
                    element_runner: Some(element_runner_client),
                    ..Default::default()
                })
                .await
                .is_ok()
        );

        Ok(())
    }

    #[fuchsia::test]
    async fn test_closing_element_control_closes_status() -> Result<(), Error> {
        let realm = build_power_broker_realm().await?;
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;

        let (element_runner_client, _) = create_endpoints::<ElementRunnerMarker>();
        let (element_control, element_control_server) = create_proxy::<ElementControlMarker>();
        assert!(
            topology
                .add_element(ElementSchema {
                    element_name: Some("Fire".into()),
                    initial_current_level: Some(BinaryPowerLevel::Off.into_primitive()),
                    valid_levels: Some(BINARY_POWER_LEVELS.to_vec()),
                    element_control: Some(element_control_server),
                    element_runner: Some(element_runner_client),
                    ..Default::default()
                })
                .await
                .is_ok()
        );

        // Confirm the element has been removed and Status channels have been
        // closed.
        let status_proxy = {
            let (client, server) = create_proxy::<StatusMarker>();
            element_control.open_status_channel(server)?;
            client
        };
        drop(element_control);
        status_proxy.as_channel().on_closed().await?;

        Ok(())
    }

    #[fuchsia::test]
    fn test_status_watch_power_level() -> Result<(), Error> {
        let mut executor = fasync::TestExecutor::new();
        let realm = executor.run_singlethreaded(async { build_power_broker_realm().await })?;

        // Create a topology with only one element:
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;
        let (element_runner_client, element_runner_server) =
            create_endpoints::<ElementRunnerMarker>();
        let mut element_runner = element_runner_server.into_stream();
        let (lessor, lessor_server) = create_proxy::<LessorMarker>();
        let (element_control, element_control_server) = create_proxy::<ElementControlMarker>();
        executor.run_singlethreaded(async {
            assert!(
                topology
                    .add_element(ElementSchema {
                        element_name: Some("E".into()),
                        initial_current_level: Some(0),
                        valid_levels: Some(vec![0, 1, 2]),
                        lessor_channel: Some(lessor_server),
                        element_control: Some(element_control_server),
                        element_runner: Some(element_runner_client),
                        ..Default::default()
                    })
                    .await
                    .is_ok()
            );
        });
        let status = {
            let (client, server) = create_proxy::<StatusMarker>();
            element_control.open_status_channel(server)?;
            client
        };

        executor.run_singlethreaded(async {
            // Initial power level should be 0.
            let responder =
                assert_set_level_required_eq_and_return_responder(element_runner.try_next(), 0)
                    .await;
            responder.send().unwrap();
            assert_eq!(status.watch_power_level().await.unwrap(), Ok(0));

            // Acquire a lease to change the required level to 1.
            let lease1 = lessor.lease(1).await.unwrap().unwrap().into_proxy();
            let responder =
                assert_set_level_required_eq_and_return_responder(element_runner.try_next(), 1)
                    .await;
            responder.send().unwrap();
            assert_eq!(status.watch_power_level().await.unwrap(), Ok(1));
            assert_eq!(
                lease1.watch_status(LeaseStatus::Unknown).await.unwrap(),
                LeaseStatus::Satisfied
            );

            // Acquire another lease to change the required level to 2.
            let lease2 = lessor.lease(2).await.unwrap().unwrap().into_proxy();
            let responder =
                assert_set_level_required_eq_and_return_responder(element_runner.try_next(), 2)
                    .await;
            responder.send().unwrap();
            // DON'T call watch_power_level here--we want to make sure it skips to the most recent
            assert_eq!(
                lease2.watch_status(LeaseStatus::Pending).await.unwrap(),
                LeaseStatus::Satisfied
            );
            // Drop lease2, level should go back to 1.
            drop(lease2);
            let responder =
                assert_set_level_required_eq_and_return_responder(element_runner.try_next(), 1)
                    .await;
            responder.send().unwrap();
            // Sleep here because otherwise we might call this before PB has updated from 2 to 1
            thread::sleep(Duration::from_millis(100));
            assert_eq!(status.watch_power_level().await.unwrap(), Ok(1));

            // Drop lease1, level should go back to 0.
            drop(lease1);
            let responder =
                assert_set_level_required_eq_and_return_responder(element_runner.try_next(), 0)
                    .await;
            responder.send().unwrap();
            assert_eq!(status.watch_power_level().await.unwrap(), Ok(0));
        });

        // Ensure there are no more current levels in the queue.
        assert!(executor.run_until_stalled(&mut status.watch_power_level()).is_pending());

        Ok(())
    }
}
