// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use fidl::endpoints::{DiscoverableProtocolMarker, RequestStream};
use fidl_fuchsia_netemul as fnetemul;
use fuchsia_component::server::{ServiceFs, ServiceFsDir, ServiceObj};
use futures::StreamExt as _;
use netemul::{TestRealm, TestSandbox};
use netstack_testing_common::realms::{Netstack, Netstack3, TestSandboxExt as _, constants};
use netstack_testing_macros::netstack_test;

const MOCK_SERVICES_NAME: &str = "mock";

fn create_netstack_with_mock_endpoint<'s, RS>(
    sandbox: &'s TestSandbox,
    name: &'s str,
) -> (TestRealm<'s>, ServiceFs<ServiceObj<'s, RS>>)
where
    RS: RequestStream + 'static,
    RS::Protocol: DiscoverableProtocolMarker,
{
    let mut netstack: fnetemul::ChildDef =
        netstack_testing_common::realms::KnownServiceProvider::Netstack(Netstack3::VERSION).into();
    {
        let fnetemul::ChildUses::Capabilities(capabilities) =
            netstack.uses.as_mut().expect("empty uses");

        // Remove any existing ChildDep capability with the same name as the endpoint
        // we wish to add.
        let new_capability_name = RS::Protocol::PROTOCOL_NAME.to_string();
        capabilities.retain(|cap| {
            match cap {
                fnetemul::Capability::ChildDep(child_dep) => {
                    match &child_dep.capability {
                        Some(fnetemul::ExposedCapability::Protocol(name)) => {
                            name != &new_capability_name
                        }
                        // Keep deps without a capability or ExposedCapability
                        // that is not a Protocol.
                        _ => true,
                    }
                }
                // Keep all other capability variants.
                _ => true,
            }
        });

        // Add the new capability.
        capabilities.push(fnetemul::Capability::ChildDep(fnetemul::ChildDep {
            name: Some(MOCK_SERVICES_NAME.to_string()),
            capability: Some(fnetemul::ExposedCapability::Protocol(new_capability_name)),
            ..Default::default()
        }));
    }

    let (mock_dir, server_end) = fidl::endpoints::create_endpoints();
    let mut fs = ServiceFs::new();
    let _: &mut ServiceFsDir<'_, _> =
        fs.dir("svc").add_fidl_service_at(RS::Protocol::PROTOCOL_NAME, |rs: RS| rs);
    let _: &mut ServiceFs<_> = fs.serve_connection(server_end).expect("serve connection");

    let realm = sandbox
        .create_realm(
            name,
            [
                netstack,
                (&netstack_testing_common::realms::KnownServiceProvider::SecureStash).into(),
                fnetemul::ChildDef {
                    source: Some(fnetemul::ChildSource::Mock(mock_dir)),
                    name: Some(MOCK_SERVICES_NAME.to_string()),
                    ..Default::default()
                },
            ],
        )
        .expect("failed to create realm");

    // Connect to any service to get netstack launched.
    let _: fidl_fuchsia_net_interfaces::StateProxy = realm
        .connect_to_protocol::<fidl_fuchsia_net_interfaces::StateMarker>()
        .expect("connect to protocol");

    (realm, fs)
}

#[netstack_test]
async fn ns_sets_thread_profiles(name: &str) {
    let sandbox = netemul::TestSandbox::new().expect("failed to create sandbox");
    let (_realm, mut fs) = create_netstack_with_mock_endpoint::<
        fidl_fuchsia_scheduler_deprecated::ProfileProviderRequestStream,
    >(&sandbox, name);

    let profile_provider_request_stream = fs.next().await.expect("fs terminated unexpectedly");

    #[derive(Default, Debug)]
    struct ExpectProfiles {
        sysmon: bool,
        worker: bool,
    }

    impl ExpectProfiles {
        fn all_done(&self) -> bool {
            let Self { sysmon, worker } = self;
            *sysmon && *worker
        }

        fn update(&mut self, profile: &str) {
            let Self { sysmon, worker } = self;
            match profile {
                "fuchsia.netstack.go-worker" => {
                    *worker = true;
                }
                "fuchsia.netstack.go-sysmon" => {
                    assert!(!*sysmon, "sysmon observed more than once");
                    *sysmon = true;
                }
                other => panic!("unexpected profile {other}"),
            }
        }
    }

    let result = async_utils::fold::fold_while(
        profile_provider_request_stream,
        ExpectProfiles::default(),
        |mut expect, r| {
            let (thread, profile, responder) =
                r.expect("request failure").into_set_profile_by_role().expect("unexpected request");
            expect.update(profile.as_str());
            assert_eq!(
                thread.basic_info().expect("failed to get basic info").rights,
                zx::Rights::TRANSFER | zx::Rights::MANAGE_THREAD
            );
            responder.send(zx::sys::ZX_OK).expect("failed to respond");

            futures::future::ready(if expect.all_done() {
                async_utils::fold::FoldWhile::Done(())
            } else {
                async_utils::fold::FoldWhile::Continue(expect)
            })
        },
    )
    .await;
    result.short_circuited().expect("didn't observe all profiles installed");
}

#[netstack_test]
async fn serves_ota_health_check(name: &str) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let realm = sandbox.create_netstack_realm::<Netstack3, _>(name).expect("create netstack realm");
    let health_check = realm
        .connect_to_protocol::<fidl_fuchsia_update_verify::ComponentOtaHealthCheckMarker>()
        .expect("connect to protocol");

    let response = health_check.get_health_status().await.expect("call succeeded");
    assert_eq!(response, fidl_fuchsia_update_verify::HealthStatus::Healthy);
}

#[netstack_test]
async fn emits_logs(name: &str) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let realm = sandbox.create_netstack_realm::<Netstack3, _>(name).expect("create netstack realm");
    // Start the netstack.
    let _ = realm
        .connect_to_protocol::<fidl_fuchsia_net_interfaces::StateMarker>()
        .expect("connect to protocol");

    let netstack_moniker =
        netstack_testing_common::get_component_moniker(&realm, constants::netstack::COMPONENT_NAME)
            .await
            .expect("get netstack moniker");
    let mut stream = diagnostics_reader::ArchiveReader::logs()
        .select_all_for_component(netstack_moniker.as_str())
        .snapshot_then_subscribe()
        .expect("subscribe to netstack logs");
    let payload = stream
        .next()
        .await
        .expect("netstack should emit logs on startup")
        .expect("extract syslogs from archivist payload");
    assert!(payload.msg().is_some(), "syslog should contain message");
}
