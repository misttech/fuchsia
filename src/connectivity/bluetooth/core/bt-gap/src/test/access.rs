// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use assert_matches::assert_matches;
use fidl::endpoints;
use fidl_fuchsia_bluetooth_host::HostRequest;
use fidl_fuchsia_bluetooth_sys::{
    self as sys, AccessMarker, AccessSetConnectionPolicyRequest, ProcedureTokenMarker,
};
use fuchsia_bluetooth::types::pairing_options::{BondableMode, PairingOptions, SecurityLevel};
use fuchsia_bluetooth::types::{HostId, PeerId, Technology};
use fuchsia_sync::RwLock;
use futures::stream::{StreamExt, TryStreamExt};
use futures::{FutureExt, future};
use std::sync::Arc;

use crate::services::access;
use crate::{host_device, host_dispatcher};

#[fuchsia::test]
async fn initiate_pairing() {
    let dispatcher = host_dispatcher::test::make_simple_test_dispatcher();

    let (host_server, _, _gatt_server, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(42), &dispatcher)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access = access::run(dispatcher, server);

    // The parameters to send to access.Pair()
    let req_id = PeerId(128);
    let req_opts = PairingOptions {
        transport: Technology::LE,
        le_security_level: SecurityLevel::Authenticated,
        bondable: BondableMode::NonBondable,
    };

    let req_opts_ = req_opts.clone();

    let make_request = async move {
        let response = client.pair(&req_id.into(), &req_opts_.into()).await;
        assert_matches!(response, Ok(Ok(())));
        // This terminating will drop the access client, which causes the access stream to
        // terminate. This will cause run_access to terminate which drops the host dispatcher, which
        // closes the host channel and will cause run_host to terminate
        Ok(())
    };

    let run_host = async move {
        host_server.try_for_each(move |req| {
            assert_matches!(&req, HostRequest::Pair { id, options, responder: _ } if *id == req_id.into() && PairingOptions::from(options) == req_opts);
            if let HostRequest::Pair { id: _, options: _, responder } = req {
                assert_matches!(responder.send(Ok(())), Ok(()));
            }
            future::ok(())
        }).await.map_err(|e| e.into())
    };

    future::try_join3(make_request, run_host, run_access)
        .await
        .map(|_: ((), (), ())| ())
        .expect("successful execution")
}

// Test that we can start discovery on a host then migrate that discovery session onto a different
// host when the original host is deactivated
#[fuchsia::test]
async fn discovery_over_adapter_change() {
    // Create mock host dispatcher
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    // Add Host #1 to dispatcher and make active
    let (host_server_1, host_1, _gatt_server_1, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();
    let host_info_1 = Arc::new(RwLock::new(host_1.info()));

    hd.set_active_host(HostId(1)).expect("can set active host");

    // Add Host #2 to dispatcher
    let (host_server_2, host_2, _gatt_server_2, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();
    let host_info_2 = Arc::new(RwLock::new(host_2.info()));

    // Create access server future
    let (access_client, access_server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access = access::run(hd.clone(), access_server);

    // Create access client future
    let (discovery_session, discovery_session_server) = endpoints::create_proxy();
    let run_client = async move {
        // Request discovery on active Host #1
        let response = access_client.start_discovery(discovery_session_server).await;
        assert_matches!(response, Ok(Ok(())));

        // Assert that Host #1 is now marked as discovering
        let _ = host_1
            .clone()
            .refresh_test_host_info()
            .await
            .expect("did not receive Host #1 info update");
        let is_discovering = host_1.info().discovering.clone();
        assert!(is_discovering);

        // Deactivate Host #1
        hd.rm_device(host_1.path()).await;

        // Assert that Host #2 is now marked as discovering
        let _ = host_2
            .clone()
            .refresh_test_host_info()
            .await
            .expect("did not receive Host #2 info update");
        let is_discovering = host_2.info().discovering.clone();
        assert!(is_discovering);

        // Drop discovery session, which contains an internal reference to the dispatcher state,
        // so that the other futures may terminate. Then, assert Host #2 stops discovering.
        drop(discovery_session);

        let _ = host_2
            .clone()
            .refresh_test_host_info()
            .await
            .expect("did not receive Host #2 info update");
        let is_discovering = host_2.info().discovering.clone();
        assert!(!is_discovering);

        Ok(())
    };

    future::try_join4(
        run_client,
        run_access,
        host_device::test::run_discovery_host_server(host_server_1, host_info_1),
        host_device::test::run_discovery_host_server(host_server_2, host_info_2),
    )
    .await
    .map(|_: ((), (), (), ())| ())
    .expect("successful execution")
}

#[fuchsia::test]
async fn make_active_host_discoverable() {
    let dispatcher = host_dispatcher::test::make_simple_test_dispatcher();

    let (host_server, _, _gatt_server, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(49), &dispatcher)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access = access::run(dispatcher, server);

    let make_request = async move {
        let (token, server) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
        let response = client.make_discoverable(server).await;
        assert_matches!(response, Ok(Ok(())));
        // Immediately dropping the token should trigger a SetDiscoverable request to turn it off.
        drop(token);
        Ok(())
    };

    let mut expected_enabled = true;
    let run_host = async move {
        host_server
            .try_for_each(move |req| {
                let HostRequest::SetDiscoverable { enabled, responder } = req else {
                    panic!("Expected HostRequest::SetDiscoverable");
                };
                assert_matches!(responder.send(Ok(())), Ok(()));
                assert_eq!(enabled, expected_enabled);
                // We first expect the Discoverable request with `enabled=true`. Then when the token
                // is dropped, we expect it to be false.
                expected_enabled = !expected_enabled;
                future::ok(())
            })
            .await
            .map_err(|e| e.into())
    };

    future::try_join3(make_request, run_host, run_access)
        .await
        .map(|_: ((), (), ())| ())
        .expect("successful execution");
}

#[fuchsia::test]
async fn toggle_connectable_of_active_host() {
    let dispatcher = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server, _, _gatt_server, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(49), &dispatcher)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let _run_access_task = fuchsia_async::Task::spawn(access::run(dispatcher, server));

    let (token, server) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server),
        ..Default::default()
    };
    let connection_policy_fut = client.set_connection_policy(policy);
    // Expect the Host server to receive a request to disable `connectable'.
    match host_server.next().await.expect("valid_request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable, got: {x:?}"),
    }

    // Expect the connection policy request to resolve successfully.
    let result = connection_policy_fut.await;
    assert_matches!(result, Ok(Ok(_)));

    // Immediately dropping the token should result in a request to re-enable connectable.
    drop(token);
    match host_server.next().await.expect("valid_request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable, got: {x:?}"),
    }
}

#[fuchsia::test]
async fn multiple_connection_policy_clients() {
    let dispatcher = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server, _, _gatt_server, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(49), &dispatcher)
            .await
            .unwrap();

    let (client1, server1) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task1 = fuchsia_async::Task::spawn(access::run(dispatcher.clone(), server1));
    let (client2, server2) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task2 = fuchsia_async::Task::spawn(access::run(dispatcher, server2));

    let (token1, server_token1) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy1 = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token1),
        ..Default::default()
    };
    let fut1 = client1.set_connection_policy(policy1);

    // Host should receive request to disable connectable.
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable, got: {x:?}"),
    }
    assert_matches!(fut1.await, Ok(Ok(_)));

    // Second client requests suppression.
    let (token2, server_token2) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy2 = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token2),
        ..Default::default()
    };
    let fut2 = client2.set_connection_policy(policy2);
    // Second request resolves successfully without issuing another SetConnectable to host.
    assert_matches!(fut2.await, Ok(Ok(_)));
    assert!(
        host_server.next().now_or_never().is_none(),
        "Unexpected message received on host_server after second client requested suppression"
    );

    // Drop first client's token and ensure Client 1's access task finishes processing.
    drop(token1);
    drop(client1);
    assert_matches!(run_access_task1.await, Ok(()));

    // Verify that dropping the first token did NOT re-enable connectable because second client
    // is still active.
    assert!(
        host_server.next().now_or_never().is_none(),
        "Unexpected message received on host_server after first client dropped token"
    );

    // Dropping second token should now cause connectable to be re-enabled.
    drop(token2);
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable, got: {x:?}"),
    }

    drop(client2);
    assert_matches!(run_access_task2.await, Ok(()));
}

#[fuchsia::test]
async fn connection_policy_over_adapter_change() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server_1, host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    hd.set_active_host(HostId(1)).expect("can set active host");

    let (mut host_server_2, _host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), server));

    let (token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token),
        ..Default::default()
    };
    let fut = client.set_connection_policy(policy);

    // Host 1 should receive SetConnectable(false)
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 1, got: {x:?}"),
    }
    assert_matches!(fut.await, Ok(Ok(_)));

    // Spawn task to remove Host 1 so Host 2 becomes active
    let host_path_1 = host_1.path().to_string();
    let hd_clone = hd.clone();
    let rm_task = fuchsia_async::Task::spawn(async move {
        hd_clone.rm_device(&host_path_1).await;
    });

    // Host 2 should receive SetConnectable(false) during configure_newly_active_adapter
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 2, got: {x:?}"),
    }

    rm_task.await;

    // Dropping token should cause Host 2 to receive SetConnectable(true)
    drop(token);
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 2, got: {x:?}"),
    }

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn connection_policy_over_adapter_removal_and_late_addition() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server_1, host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), server));

    let (token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token),
        ..Default::default()
    };
    let fut = client.set_connection_policy(policy);

    // Host 1 should receive SetConnectable(false)
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 1, got: {x:?}"),
    }
    assert_matches!(fut.await, Ok(Ok(_)));

    // Remove Host 1 so 0 hosts remain.
    let host_path_1 = host_1.path().to_string();
    hd.rm_device(&host_path_1).await;

    // Add Host 2 (0 -> 1 host transition).
    let (mut host_server_2, _host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    // Host 2 should receive SetConnectable(false) during configure_newly_active_adapter
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 2, got: {x:?}"),
    }

    // Dropping token should cause Host 2 to receive SetConnectable(true)
    drop(token);
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 2, got: {x:?}"),
    }

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn connection_policy_host_error_does_not_deadlock_and_recovers() {
    let dispatcher = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server, _, _gatt_server, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &dispatcher)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(dispatcher.clone(), server));

    let (_token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token),
        ..Default::default()
    };
    let fut = client.set_connection_policy(policy);

    // Host receives SetConnectable(false) and replies with an error.
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Err(sys::Error::Failed)).expect("valid response");
        }
        x => panic!("Expected SetConnectable(false), got: {x:?}"),
    }

    // The client request must fail without deadlocking the dispatcher.
    assert_matches!(fut.await, Ok(Err(sys::Error::Failed)));

    // A subsequent connection policy request should be able to try again and succeed.
    let (token2, server_token2) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy2 = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token2),
        ..Default::default()
    };
    let fut2 = client.set_connection_policy(policy2);

    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(false), got: {x:?}"),
    }
    assert_matches!(fut2.await, Ok(Ok(_)));

    drop(token2);
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(true), got: {x:?}"),
    }

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn connection_policy_immediate_re_suppression_maintains_ordering() {
    let dispatcher = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server, _, _gatt_server, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &dispatcher)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(dispatcher.clone(), server));

    let (token1, server_token1) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy1 = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token1),
        ..Default::default()
    };
    let fut1 = client.set_connection_policy(policy1);

    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(false), got: {x:?}"),
    }
    assert_matches!(fut1.await, Ok(Ok(_)));

    // Client 1 drops token and Client 2 immediately requests suppression.
    drop(token1);
    let (token2, server_token2) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy2 = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token2),
        ..Default::default()
    };
    let fut2 = client.set_connection_policy(policy2);

    // Host should first receive SetConnectable(true) from the drop, followed by
    // SetConnectable(false) from the new request, in strict FIFO order.
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(true), got: {x:?}"),
    }

    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(false), got: {x:?}"),
    }
    assert_matches!(fut2.await, Ok(Ok(_)));

    drop(token2);
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(true), got: {x:?}"),
    }

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn connection_policy_migrates_pending_suppression() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server_1, host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    hd.set_active_host(HostId(1)).expect("can set active host");

    let (mut host_server_2, _host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), server));

    let (token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token),
        ..Default::default()
    };
    let fut = client.set_connection_policy(policy);

    // Host 1 receives SetConnectable(false), but does NOT respond yet (keeping state Pending).
    let responder_1 = match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => responder,
        x => panic!("Expected SetConnectable on host 1, got: {x:?}"),
    };

    // Remove Host 1 while suppression is pending so Host 2 becomes active.
    let host_path_1 = host_1.path().to_string();
    let hd_clone = hd.clone();
    let rm_task = fuchsia_async::Task::spawn(async move {
        hd_clone.rm_device(&host_path_1).await;
    });

    // Host 2 should receive SetConnectable(false) as the pending task is migrated.
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 2, got: {x:?}"),
    }

    rm_task.await;

    // Dropping or replying on the abandoned Host 1 responder should be harmless.
    drop(responder_1);

    // The client's original future should resolve successfully on Host 2!
    assert_matches!(fut.await, Ok(Ok(_)));

    // Dropping the token should cause Host 2 to receive SetConnectable(true).
    drop(token);
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 2, got: {x:?}"),
    }

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn multiple_discoverable_clients() {
    let dispatcher = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server, _, _gatt_server, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(49), &dispatcher)
            .await
            .unwrap();

    let (client1, server1) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task1 = fuchsia_async::Task::spawn(access::run(dispatcher.clone(), server1));
    let (client2, server2) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task2 = fuchsia_async::Task::spawn(access::run(dispatcher.clone(), server2));
    let (client3, server3) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task3 = fuchsia_async::Task::spawn(access::run(dispatcher, server3));

    let (token1, server_token1) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let fut1 = client1.make_discoverable(server_token1);

    // Host receives request to enable discoverable; hold responder so state stays Pending.
    let responder = match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => responder,
        x => panic!("Expected SetDiscoverable, got: {x:?}"),
    };

    // Second client requests discoverable while the first request is still Pending.
    let (token2, server_token2) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let mut fut2 = std::pin::pin!(client2.make_discoverable(server_token2));
    for _ in 0..5 {
        fuchsia_async::yield_now().await;
    }
    assert!(futures::poll!(&mut fut2).is_pending());
    assert!(
        host_server.next().now_or_never().is_none(),
        "Unexpected message received on host_server while first discoverable request is Pending"
    );

    // Responding on host resolves both Pending clients.
    responder.send(Ok(())).expect("valid response");
    assert_matches!(fut1.await, Ok(Ok(_)));
    assert_matches!(fut2.await, Ok(Ok(_)));

    // Third client requests discoverable while already in Discoverable state.
    let (token3, server_token3) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let fut3 = client3.make_discoverable(server_token3);
    // Third request resolves successfully without issuing another SetDiscoverable to host.
    assert_matches!(fut3.await, Ok(Ok(_)));
    assert!(
        host_server.next().now_or_never().is_none(),
        "Unexpected message received on host_server after third client requested discoverable"
    );

    // Drop first and second clients' tokens and ensure their access tasks finish processing.
    drop(token1);
    drop(client1);
    assert_matches!(run_access_task1.await, Ok(()));
    drop(token2);
    drop(client2);
    assert_matches!(run_access_task2.await, Ok(()));

    // Verify that dropping the first two tokens did NOT disable discoverable because third client
    // is still active.
    assert!(
        host_server.next().now_or_never().is_none(),
        "Unexpected message received on host_server while third client still holds token"
    );

    // Dropping third token should now cause discoverable to be disabled.
    drop(token3);
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable, got: {x:?}"),
    }

    drop(client3);
    assert_matches!(run_access_task3.await, Ok(()));
}

#[fuchsia::test]
async fn discoverable_over_adapter_change() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server_1, host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    hd.set_active_host(HostId(1)).expect("can set active host");

    let (mut host_server_2, _host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), server));

    let (token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let fut = client.make_discoverable(server_token);

    // Host 1 should receive SetDiscoverable(true)
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable on host 1, got: {x:?}"),
    }
    assert_matches!(fut.await, Ok(Ok(_)));

    // Spawn task to remove Host 1 so Host 2 becomes active
    let host_path_1 = host_1.path().to_string();
    let hd_clone = hd.clone();
    let rm_task = fuchsia_async::Task::spawn(async move {
        hd_clone.rm_device(&host_path_1).await;
    });

    // Host 2 should receive SetDiscoverable(true) during configure_newly_active_adapter
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable on host 2, got: {x:?}"),
    }

    rm_task.await;

    // Verify removed host 1 was NOT disabled when its discoverable session was migrated.
    assert!(
        host_server_1.next().now_or_never().is_none(),
        "Unexpected message received on host 1 after adapter change"
    );

    // Dropping token should cause Host 2 to receive SetDiscoverable(false)
    drop(token);
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable on host 2, got: {x:?}"),
    }

    // Host 1 should still have received no messages.
    assert!(
        host_server_1.next().now_or_never().is_none(),
        "Unexpected message received on host 1 after token drop"
    );

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn discoverable_over_adapter_removal_and_late_addition() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server_1, host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), server));

    let (token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let fut = client.make_discoverable(server_token);

    // Host 1 should receive SetDiscoverable(true)
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable on host 1, got: {x:?}"),
    }
    assert_matches!(fut.await, Ok(Ok(_)));

    // Remove Host 1 so 0 hosts remain.
    let host_path_1 = host_1.path().to_string();
    hd.rm_device(&host_path_1).await;

    // Add Host 2 (0 -> 1 host transition).
    let (mut host_server_2, _host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    // Host 2 should receive SetDiscoverable(true) during configure_newly_active_adapter
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable on host 2, got: {x:?}"),
    }

    // Dropping token should cause Host 2 to receive SetDiscoverable(false)
    drop(token);
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable on host 2, got: {x:?}"),
    }

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn discoverable_host_error_does_not_deadlock_and_recovers() {
    let dispatcher = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server, _, _gatt_server, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &dispatcher)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(dispatcher.clone(), server));

    let (_token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let fut = client.make_discoverable(server_token);

    // Host receives SetDiscoverable(true) and replies with an error.
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Err(sys::Error::Failed)).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable(true), got: {x:?}"),
    }

    // The client request must fail without deadlocking the dispatcher.
    assert_matches!(fut.await, Ok(Err(sys::Error::Failed)));

    // A subsequent make_discoverable request should be able to try again and succeed.
    let (token2, server_token2) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let fut2 = client.make_discoverable(server_token2);

    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable(true), got: {x:?}"),
    }
    assert_matches!(fut2.await, Ok(Ok(_)));

    drop(token2);
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable(false), got: {x:?}"),
    }

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn discoverable_immediate_re_discoverable_maintains_ordering() {
    let dispatcher = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server, _, _gatt_server, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &dispatcher)
            .await
            .unwrap();

    let (client1, server1) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task1 = fuchsia_async::Task::spawn(access::run(dispatcher.clone(), server1));
    let (client2, server2) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task2 = fuchsia_async::Task::spawn(access::run(dispatcher, server2));

    let (token1, server_token1) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let fut1 = client1.make_discoverable(server_token1);

    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable(true), got: {x:?}"),
    }
    assert_matches!(fut1.await, Ok(Ok(_)));

    // Client 1 drops token and finishes processing before Client 2 immediately requests
    // discoverable.
    drop(token1);
    drop(client1);
    assert_matches!(run_access_task1.await, Ok(()));

    let (token2, server_token2) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let fut2 = client2.make_discoverable(server_token2);

    // Host should first receive SetDiscoverable(false) from the drop, followed by
    // SetDiscoverable(true) from the new request, in strict FIFO order.
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable(false), got: {x:?}"),
    }

    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable(true), got: {x:?}"),
    }
    assert_matches!(fut2.await, Ok(Ok(_)));

    drop(token2);
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable(false), got: {x:?}"),
    }

    drop(client2);
    assert_matches!(run_access_task2.await, Ok(()));
}

#[fuchsia::test]
async fn discoverable_migrates_pending_discoverable() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server_1, host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    hd.set_active_host(HostId(1)).expect("can set active host");

    let (mut host_server_2, _host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), server));

    let (token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let fut = client.make_discoverable(server_token);

    // Host 1 receives SetDiscoverable(true), but does NOT respond yet (keeping state Pending).
    let responder_1 = match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => responder,
        x => panic!("Expected SetDiscoverable on host 1, got: {x:?}"),
    };

    // Remove Host 1 while discoverable is pending so Host 2 becomes active.
    let host_path_1 = host_1.path().to_string();
    let hd_clone = hd.clone();
    let rm_task = fuchsia_async::Task::spawn(async move {
        hd_clone.rm_device(&host_path_1).await;
    });

    // Host 2 should receive SetDiscoverable(true) as the pending task is migrated.
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable on host 2, got: {x:?}"),
    }

    rm_task.await;

    // Dropping or replying on the abandoned Host 1 responder should be harmless.
    drop(responder_1);

    // The client's original future should resolve successfully on Host 2!
    assert_matches!(fut.await, Ok(Ok(_)));

    // Dropping the token should cause Host 2 to receive SetDiscoverable(false).
    drop(token);
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable on host 2, got: {x:?}"),
    }

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}
