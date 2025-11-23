// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::base::{Dependency, Entity, SettingType};
use crate::ingress::{fidl, registration};
use crate::job::source::Error;
use crate::job::{self, Job};
use crate::migration::MIGRATION_FILE_NAME;
use crate::tests::scaffold::workload::channel;
use crate::{Environment, EnvironmentBuilder};
use ::fidl::endpoints::create_proxy_and_stream;
use assert_matches::assert_matches;
use fidl_fuchsia_stash::StoreMarker;
use fuchsia_async as fasync;
use fuchsia_inspect::component;
use futures::{FutureExt, StreamExt};
use settings_common::inspect::config_logger::InspectConfigLogger;
use settings_light::build_light_default_settings;
use settings_test_common::storage::InMemoryStorageFactory;
use std::rc::Rc;

const ENV_NAME: &str = "settings_service_environment_test";

#[fuchsia::test(allow_stalls = false)]
async fn test_dependency_generation() {
    let entity = Entity::Handler(SettingType::Unknown);

    let registrant = registration::Registrant::new(
        "Registrar::Test".to_string(),
        registration::Registrar::Test(Box::new(move || {})),
        [Dependency::Entity(entity)].into(),
    );

    let Environment { entities, .. } =
        EnvironmentBuilder::new(Rc::new(InMemoryStorageFactory::new()))
            .registrants(vec![registrant])
            .spawn_nested(ENV_NAME)
            .await
            .expect("environment should be built");

    assert!(entities.contains(&entity));
}

#[fuchsia::test(allow_stalls = false)]
async fn test_job_sourcing() {
    // Create channel to send the current job state.
    let (job_state_tx, mut job_state_rx) = futures::channel::mpsc::unbounded::<channel::State>();

    // Create a new job stream with an Job that will signal when it is executed.
    let job_stream = async move {
        Ok(Job::new(job::work::Load::Independent(Box::new(channel::Workload::new(job_state_tx)))))
            as Result<Job, Error>
    }
    .into_stream();

    // Build a registrant with the stream.
    let registrant = registration::Registrant::new(
        "Registrar::TestWithSeeder".to_string(),
        registration::Registrar::TestWithSeeder(Box::new(move |seeder| {
            seeder.seed(job_stream);
        })),
        [].into(),
    );

    // Build environment with the registrant.
    let _ = EnvironmentBuilder::new(Rc::new(InMemoryStorageFactory::new()))
        .registrants(vec![registrant])
        .spawn_nested(ENV_NAME)
        .await
        .expect("environment should be built");

    // Ensure job is executed.
    assert_matches!(job_state_rx.next().await, Some(channel::State::Execute));
}

#[fuchsia::test]
async fn migration_error_does_not_cause_early_exit() {
    const UNKNOWN_ID: u64 = u64::MAX;
    let fs = tempfile::tempdir().expect("failed to create tempdir");
    std::fs::write(fs.path().join(MIGRATION_FILE_NAME), UNKNOWN_ID.to_string())
        .expect("failed to write migration file");
    let directory = fuchsia_fs::directory::open_in_namespace(
        fs.path().to_str().expect("tempdir path is not valid UTF-8"),
        fuchsia_fs::PERM_READABLE | fuchsia_fs::PERM_WRITABLE,
    )
    .expect("failed to open connection to tempdir");
    let (store_proxy, mut request_stream) = create_proxy_and_stream::<StoreMarker>();
    fasync::Task::local(async move {
        while let Some(request) = request_stream.next().await {
            match request.unwrap() {
                fidl_fuchsia_stash::StoreRequest::Identify { .. } => {}
                fidl_fuchsia_stash::StoreRequest::CreateAccessor { accessor_request, .. } => {
                    let mut stream = accessor_request.into_stream();
                    fasync::Task::local(async move {
                        if let Some(r) = stream.next().await {
                            panic!("unexpected call to store before migration id checked: {r:?}");
                        }
                    })
                    .detach();
                }
            }
        }
    })
    .detach();

    let config_logger =
        Rc::new(std::sync::Mutex::new(InspectConfigLogger::new(component::inspector().root())));

    let _ = EnvironmentBuilder::new(Rc::new(InMemoryStorageFactory::new()))
        .fidl_interfaces(&[fidl::Interface::Light])
        .store_proxy(store_proxy)
        .storage_dir(directory)
        .light_configuration(build_light_default_settings(config_logger))
        .spawn_nested(ENV_NAME)
        .await
        .expect("environment should be built");
}
