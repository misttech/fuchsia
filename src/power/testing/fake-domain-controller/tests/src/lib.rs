// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl::endpoints::DiscoverableProtocolMarker;
use fidl_fuchsia_power_cpu as fcpu;
use fuchsia_component_test::{Capability, ChildOptions, RealmBuilder, RealmInstance, Ref, Route};
use log::*;

const DOMAIN_ID0: u64 = 0;
const DOMAIN_ID1: u64 = 1;
const FREQUENCIES_HZ: [&'static [u64]; 2] =
    [&[2024000000, 1512000000, 1256000000, 1128000000], &[1024000000, 512000000]];

struct TestEnv {
    realm_instance: RealmInstance,
}

impl TestEnv {
    /// Connects to a protocol exposed by a component within the RealmInstance.
    pub fn connect_to_protocol<P: DiscoverableProtocolMarker>(&self) -> P::Proxy {
        self.realm_instance.root.connect_to_protocol_at_exposed_dir().unwrap()
    }
}

async fn create_test_env() -> TestEnv {
    info!("building the test env");

    let builder = RealmBuilder::new().await.unwrap();

    let component_ref = builder
        .add_child("fake-domain-controller", "#meta/fake-domain-controller.cm", ChildOptions::new())
        .await
        .expect("Failed to add child: fake-domain-controller");

    // Expose capabilities from fake-domain-controller.
    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol_by_name("fuchsia.power.cpu.DomainController"))
                .from(&component_ref)
                .to(Ref::parent()),
        )
        .await
        .unwrap();

    let realm_instance = builder.build().await.expect("Failed to build RealmInstance");
    TestEnv { realm_instance }
}

#[fuchsia::test]
async fn list_domains() {
    let env = create_test_env().await;
    let domain_controller = env.connect_to_protocol::<fcpu::DomainControllerMarker>();
    let domains = domain_controller.list_domains().await.unwrap();
    assert_eq!(2, domains.len());
    assert_eq!(
        fcpu::DomainInfo {
            id: Some(DOMAIN_ID0),
            core_ids: Some(vec![0, 1]),
            available_frequencies_hz: Some(FREQUENCIES_HZ[0].into()),
            name: Some("test-cluster0".to_string()),
            ..Default::default()
        },
        domains[0]
    );
    assert_eq!(
        fcpu::DomainInfo {
            id: Some(DOMAIN_ID1),
            core_ids: Some(vec![2, 3, 4, 5]),
            available_frequencies_hz: Some(FREQUENCIES_HZ[1].into()),
            name: Some("test-cluster1".to_string()),
            ..Default::default()
        },
        domains[1]
    );
}

#[fuchsia::test]
async fn get_max_frequency_returns_default() {
    let env = create_test_env().await;
    let domain_controller = env.connect_to_protocol::<fcpu::DomainControllerMarker>();
    let max_frequency_index =
        domain_controller.get_max_frequency(DOMAIN_ID0).await.unwrap().unwrap();
    assert_eq!(0, max_frequency_index);
}

#[fuchsia::test]
async fn set_max_frequency() {
    let env = create_test_env().await;
    let domain_controller = env.connect_to_protocol::<fcpu::DomainControllerMarker>();
    domain_controller.set_max_frequency(DOMAIN_ID0, 1).await.unwrap().unwrap();
    let max_frequency_index =
        domain_controller.get_max_frequency(DOMAIN_ID0).await.unwrap().unwrap();
    assert_eq!(1, max_frequency_index);
}

#[fuchsia::test]
async fn set_then_clear_max_frequency() {
    let env = create_test_env().await;
    let domain_controller = env.connect_to_protocol::<fcpu::DomainControllerMarker>();

    domain_controller.set_max_frequency(DOMAIN_ID1, 1).await.unwrap().unwrap();
    let new_max_frequency_index =
        domain_controller.get_max_frequency(DOMAIN_ID1).await.unwrap().unwrap();
    assert_eq!(1, new_max_frequency_index);

    domain_controller.clear_max_frequency(DOMAIN_ID1).await.unwrap().unwrap();
    let max_frequency_index =
        domain_controller.get_max_frequency(DOMAIN_ID1).await.unwrap().unwrap();
    assert_eq!(0, max_frequency_index);
}

#[fuchsia::test]
async fn get_max_frequency_returns_error_on_unknown_domain_id() {
    let env = create_test_env().await;
    let domain_controller = env.connect_to_protocol::<fcpu::DomainControllerMarker>();
    let err = domain_controller.get_max_frequency(2).await.unwrap().unwrap_err();
    assert_eq!(fcpu::DomainControllerError::InvalidArguments, err);
}

#[fuchsia::test]
async fn set_max_frequency_error_on_unknown_domain_id() {
    let env = create_test_env().await;
    let domain_controller = env.connect_to_protocol::<fcpu::DomainControllerMarker>();
    let err = domain_controller.set_max_frequency(2, 1).await.unwrap().unwrap_err();
    assert_eq!(fcpu::DomainControllerError::InvalidArguments, err);
}

#[fuchsia::test]
async fn set_max_frequency_error_on_unknown_frequency() {
    let env = create_test_env().await;
    let domain_controller = env.connect_to_protocol::<fcpu::DomainControllerMarker>();
    let err = domain_controller.set_max_frequency(DOMAIN_ID0, 999).await.unwrap().unwrap_err();
    assert_eq!(fcpu::DomainControllerError::InvalidArguments, err);
}

#[fuchsia::test]
async fn clear_max_frequency_error_on_unknown_domain_id() {
    let env = create_test_env().await;
    let domain_controller = env.connect_to_protocol::<fcpu::DomainControllerMarker>();
    let err = domain_controller.clear_max_frequency(4).await.unwrap().unwrap_err();
    assert_eq!(fcpu::DomainControllerError::InvalidArguments, err);
}

#[fuchsia::test]
async fn get_min_frequency_returns_default() {
    let env = create_test_env().await;
    let domain_controller = env.connect_to_protocol::<fcpu::DomainControllerMarker>();
    let min_frequency_index =
        domain_controller.get_min_frequency(DOMAIN_ID0).await.unwrap().unwrap();
    assert_eq!((FREQUENCIES_HZ[0].len() - 1) as u64, min_frequency_index);
}

#[fuchsia::test]
async fn get_min_frequency_returns_error_on_unknown_domain_id() {
    let env = create_test_env().await;
    let domain_controller = env.connect_to_protocol::<fcpu::DomainControllerMarker>();
    let err = domain_controller.get_min_frequency(2).await.unwrap().unwrap_err();
    assert_eq!(fcpu::DomainControllerError::InvalidArguments, err);
}

#[fuchsia::test]
async fn lease_max_frequency_errors_on_invalid_arguments() {
    let env = create_test_env().await;
    let domain_controller = env.connect_to_protocol::<fcpu::DomainControllerMarker>();
    let err = domain_controller.lease_max_frequency(2, 1, "test").await.unwrap().unwrap_err();
    assert_eq!(fcpu::DomainControllerError::InvalidArguments, err);
    let err =
        domain_controller.lease_max_frequency(DOMAIN_ID0, 999, "test").await.unwrap().unwrap_err();
    assert_eq!(fcpu::DomainControllerError::InvalidArguments, err);
}

#[fuchsia::test]
async fn lease_min_frequency_errors_on_invalid_arguments() {
    let env = create_test_env().await;
    let domain_controller = env.connect_to_protocol::<fcpu::DomainControllerMarker>();
    let err = domain_controller.lease_min_frequency(2, 1, "test").await.unwrap().unwrap_err();
    assert_eq!(fcpu::DomainControllerError::InvalidArguments, err);
    let err =
        domain_controller.lease_min_frequency(DOMAIN_ID0, 999, "test").await.unwrap().unwrap_err();
    assert_eq!(fcpu::DomainControllerError::InvalidArguments, err);
}

#[fuchsia::test]
async fn lease_max_and_min_frequency_multi_client() {
    let env = create_test_env().await;
    let domain_controller = env.connect_to_protocol::<fcpu::DomainControllerMarker>();

    let lease_a =
        domain_controller.lease_max_frequency(DOMAIN_ID0, 1, "voter_a").await.unwrap().unwrap();
    let lease_b =
        domain_controller.lease_max_frequency(DOMAIN_ID0, 2, "voter_b").await.unwrap().unwrap();
    assert_eq!(2, domain_controller.get_max_frequency(DOMAIN_ID0).await.unwrap().unwrap());

    drop(lease_b);
    assert_eq!(1, domain_controller.get_max_frequency(DOMAIN_ID0).await.unwrap().unwrap());

    drop(lease_a);
    assert_eq!(0, domain_controller.get_max_frequency(DOMAIN_ID0).await.unwrap().unwrap());

    let min_a =
        domain_controller.lease_min_frequency(DOMAIN_ID0, 2, "min_voter_a").await.unwrap().unwrap();
    let min_b =
        domain_controller.lease_min_frequency(DOMAIN_ID0, 1, "min_voter_b").await.unwrap().unwrap();
    assert_eq!(1, domain_controller.get_min_frequency(DOMAIN_ID0).await.unwrap().unwrap());

    drop(min_b);
    assert_eq!(2, domain_controller.get_min_frequency(DOMAIN_ID0).await.unwrap().unwrap());

    drop(min_a);
    assert_eq!(
        (FREQUENCIES_HZ[0].len() - 1) as u64,
        domain_controller.get_min_frequency(DOMAIN_ID0).await.unwrap().unwrap()
    );
}
