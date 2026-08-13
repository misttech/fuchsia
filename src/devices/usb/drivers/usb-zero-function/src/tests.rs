// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use fidl::endpoints::{RequestStream, create_endpoints};

const TEST_EP_IN_ADDR: u8 = 1;
const TEST_EP_OUT_ADDR: u8 = 2;
use futures::channel::mpsc;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, PartialEq)]
enum MockEvent {
    VmoRegistered,
    RequestQueued,
}

#[derive(Default)]
struct MockEndpointState {
    requests: Vec<fusb_request::Request>,
    vmos: HashMap<u64, zx::Vmo>,
}

async fn run_mock_endpoint(
    mut stream: fusb_endpoint::EndpointRequestStream,
    state: Arc<Mutex<MockEndpointState>>,
    mut completion_rx: mpsc::UnboundedReceiver<Vec<fusb_endpoint::Completion>>,
    event_tx: mpsc::UnboundedSender<MockEvent>,
    scope: Arc<fasync::Scope>,
) {
    let control_handle = stream.control_handle();

    // Spawn task to handle completions
    let ch = control_handle.clone();
    scope.spawn_local(async move {
        while let Some(completion) = completion_rx.next().await {
            let _ = ch.send_on_completion(completion);
        }
    });

    while let Ok(Some(request)) = stream.try_next().await {
        match request {
            fusb_endpoint::EndpointRequest::RegisterVmos { vmo_ids, responder } => {
                let mut vmos = vec![];
                let mut state_lock = state.lock().unwrap();
                for info in vmo_ids {
                    let id = info.id.unwrap();
                    let size = info.size.unwrap();
                    let vmo = zx::Vmo::create(size).unwrap();
                    let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
                    state_lock.vmos.insert(id, vmo);
                    vmos.push(fusb_endpoint::VmoHandle {
                        id: Some(id),
                        vmo: Some(dup),
                        ..Default::default()
                    });
                }
                let _ = responder.send(vmos);
                let _ = event_tx.unbounded_send(MockEvent::VmoRegistered);
            }
            fusb_endpoint::EndpointRequest::QueueRequests { req, control_handle: _ } => {
                let mut state_lock = state.lock().unwrap();
                state_lock.requests.extend(req);
                let _ = event_tx.unbounded_send(MockEvent::RequestQueued);
            }
            fusb_endpoint::EndpointRequest::UnregisterVmos { vmo_ids, responder } => {
                let mut state_lock = state.lock().unwrap();
                for id in vmo_ids {
                    state_lock.vmos.remove(&id);
                }
                let _ = responder.send(&[], &[]);
            }
            _ => {}
        }
    }
}

async fn run_mock_function(mut stream: fusb_function::UsbFunctionRequestStream) {
    while let Ok(Some(request)) = stream.try_next().await {
        match request {
            fusb_function::UsbFunctionRequest::ConfigureEndpoint { responder, .. } => {
                let _ = responder.send(Ok(()));
            }
            fusb_function::UsbFunctionRequest::DisableEndpoint { responder, .. } => {
                let _ = responder.send(Ok(()));
            }
            fusb_function::UsbFunctionRequest::EndpointSetStall { responder, .. } => {
                let _ = responder.send(Ok(()));
            }
            fusb_function::UsbFunctionRequest::EndpointClearStall { responder, .. } => {
                let _ = responder.send(Ok(()));
            }
            fusb_function::UsbFunctionRequest::ConnectToEndpoint { responder, .. } => {
                let _ = responder.send(Ok(()));
            }
            fusb_function::UsbFunctionRequest::Deconfigure { responder } => {
                let _ = responder.send(Ok(()));
            }
            _ => {}
        }
    }
}
#[fuchsia::test]
async fn test_loopback() {
    let (ep_in_client, ep_in_server) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let (ep_out_client, ep_out_server) = create_endpoints::<fusb_endpoint::EndpointMarker>();

    let state_in =
        Arc::new(Mutex::new(MockEndpointState { requests: vec![], vmos: HashMap::new() }));
    let state_out =
        Arc::new(Mutex::new(MockEndpointState { requests: vec![], vmos: HashMap::new() }));

    let (_comp_in_tx, comp_in_rx) = mpsc::unbounded();
    let (comp_out_tx, comp_out_rx) = mpsc::unbounded();
    let (event_tx, mut event_rx) = mpsc::unbounded();

    let scope = Arc::new(fasync::Scope::new_with_name("test"));

    scope.spawn_local(run_mock_endpoint(
        ep_in_server.into_stream(),
        state_in.clone(),
        comp_in_rx,
        event_tx.clone(),
        scope.clone(),
    ));
    scope.spawn_local(run_mock_endpoint(
        ep_out_server.into_stream(),
        state_out.clone(),
        comp_out_rx,
        event_tx,
        scope.clone(),
    ));

    let ep_in_proxy = ep_in_client.into_proxy();
    let ep_out_proxy = ep_out_client.into_proxy();

    let mut vmos_registered = false;
    let _tasks = run_loopback(ep_in_proxy, ep_out_proxy, &mut vmos_registered).await.unwrap();

    // Await setup events: ep_out registered (1), ep_in registered (1),
    // and ep_out queued read request (1).
    let mut vmo_reg_count = 0;
    let mut req_queue_count = 0;
    while vmo_reg_count < 2 || req_queue_count < 1 {
        match event_rx.next().await {
            Some(MockEvent::VmoRegistered) => vmo_reg_count += 1,
            Some(MockEvent::RequestQueued) => req_queue_count += 1,
            None => panic!("Event stream ended unexpectedly during setup"),
        }
    }

    // Verify OUT VMO was registered
    let vmo_out = {
        let state = state_out.lock().unwrap();
        state
            .vmos
            .get(&USB_ZERO_OUT_VMO_ID)
            .unwrap()
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
            .unwrap()
    };

    // Verify IN VMO was registered
    let vmo_in = {
        let state = state_in.lock().unwrap();
        state
            .vmos
            .get(&USB_ZERO_IN_VMO_ID)
            .unwrap()
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
            .unwrap()
    };

    // Verify read request was queued
    let read_req = {
        let mut state = state_out.lock().unwrap();
        state.requests.pop().unwrap()
    };

    // Fill VMO with some data
    let test_data = vec![1, 2, 3, 4, 5];
    vmo_out.write(&test_data, 0).unwrap();

    // Complete read
    comp_out_tx
        .unbounded_send(vec![fusb_endpoint::Completion {
            request: Some(read_req),
            status: Some(zx::sys::ZX_OK),
            transfer_size: Some(test_data.len() as u64),
            ..Default::default()
        }])
        .unwrap();

    // Wait for loopback to process and queue write request on ep_in
    loop {
        match event_rx.next().await {
            Some(MockEvent::RequestQueued) => break,
            Some(MockEvent::VmoRegistered) => {}
            None => panic!("Event stream ended unexpectedly waiting for write request"),
        }
    }

    // Verify write request was queued on ep_in
    let _write_req = {
        let mut state = state_in.lock().unwrap();
        state.requests.pop().unwrap()
    };

    // Verify data in VMO IN
    let mut read_back = vec![0; test_data.len()];
    vmo_in.read(&mut read_back, 0).unwrap();
    assert_eq!(read_back, test_data);
}

#[fuchsia::test]
async fn test_vendor_requests() {
    let (iface_client, iface_server) =
        create_endpoints::<fusb_function::UsbFunctionInterfaceMarker>();
    let (func_client, func_server) = create_endpoints::<fusb_function::UsbFunctionMarker>();
    let (ep_in_client, _ep_in_server) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let (ep_out_client, _ep_out_server) = create_endpoints::<fusb_endpoint::EndpointMarker>();

    let scope = Arc::new(fasync::Scope::new_with_name("test_vendor"));
    scope.spawn_local(run_mock_function(func_server.into_stream()));
    let func_client_proxy = func_client.into_proxy();
    let ep_in_proxy = ep_in_client.into_proxy();
    let ep_out_proxy = ep_out_client.into_proxy();
    scope.spawn_local(async move {
        let mut zero_function = UsbZeroFunctionDevice::new(
            func_client_proxy,
            ep_in_proxy,
            TEST_EP_IN_ADDR,
            ep_out_proxy,
            TEST_EP_OUT_ADDR,
            0,
        );
        zero_function.handle_requests(iface_server.into_stream()).await;
    });

    let proxy = iface_client.into_proxy();

    // Test VendorRequest::SetStall (0x50)
    let setup_set_stall = fusb_descriptor::UsbSetup {
        bm_request_type: 0x40,
        b_request: VendorRequest::SetStall as u8,
        w_value: TEST_EP_IN_ADDR as u16,
        w_index: 0,
        w_length: 0,
    };
    let res = proxy.control(&setup_set_stall, &[]).await.unwrap();
    assert_eq!(res, Ok(vec![]));

    // Test VendorRequest::SetStall with invalid w_value (> 0xFF)
    let setup_invalid_w_value = fusb_descriptor::UsbSetup {
        bm_request_type: 0x40,
        b_request: VendorRequest::SetStall as u8,
        w_value: 0x100,
        w_index: 0,
        w_length: 0,
    };
    let res = proxy.control(&setup_invalid_w_value, &[]).await.unwrap();
    assert_eq!(res, Err(Status::INVALID_ARGS.into_raw()));

    // Test VendorRequest::SetStall with invalid direction bit (0xC0)
    let setup_invalid_dir = fusb_descriptor::UsbSetup {
        bm_request_type: 0xC0,
        b_request: VendorRequest::SetStall as u8,
        w_value: TEST_EP_IN_ADDR as u16,
        w_index: 0,
        w_length: 0,
    };
    let res = proxy.control(&setup_invalid_dir, &[]).await.unwrap();
    assert_eq!(res, Err(Status::INVALID_ARGS.into_raw()));

    // Test VendorRequest::ClearStall (0x51)
    let setup_clear_stall = fusb_descriptor::UsbSetup {
        bm_request_type: 0x40,
        b_request: VendorRequest::ClearStall as u8,
        w_value: TEST_EP_IN_ADDR as u16,
        w_index: 0,
        w_length: 0,
    };
    let res = proxy.control(&setup_clear_stall, &[]).await.unwrap();
    assert_eq!(res, Ok(vec![]));

    // Test VendorRequest::ConfigureEndpoint (0x52)
    let setup_config_ep = fusb_descriptor::UsbSetup {
        bm_request_type: 0x40,
        b_request: VendorRequest::ConfigureEndpoint as u8,
        w_value: TEST_EP_IN_ADDR as u16,
        w_index: 0,
        w_length: 0,
    };
    let res = proxy.control(&setup_config_ep, &[]).await.unwrap();
    assert_eq!(res, Ok(vec![]));

    // Test VendorRequest::DisableEndpoint (0x53)
    let setup_disable_ep = fusb_descriptor::UsbSetup {
        bm_request_type: 0x40,
        b_request: VendorRequest::DisableEndpoint as u8,
        w_value: TEST_EP_IN_ADDR as u16,
        w_index: 0,
        w_length: 0,
    };
    let res = proxy.control(&setup_disable_ep, &[]).await.unwrap();
    assert_eq!(res, Ok(vec![]));

    // Test VendorRequest::ConnectEndpoint (0x54)
    let setup_connect_ep = fusb_descriptor::UsbSetup {
        bm_request_type: 0x40,
        b_request: VendorRequest::ConnectEndpoint as u8,
        w_value: TEST_EP_IN_ADDR as u16,
        w_index: 0,
        w_length: 0,
    };
    let res = proxy.control(&setup_connect_ep, &[]).await.unwrap();
    assert_eq!(res, Ok(vec![]));

    // Test VendorRequest::Deconfigure (0x55)
    let setup_deconfig = fusb_descriptor::UsbSetup {
        bm_request_type: 0x40,
        b_request: VendorRequest::Deconfigure as u8,
        w_value: 0,
        w_index: 0,
        w_length: 0,
    };
    let res_deconfig = proxy.control(&setup_deconfig, &[]).await.unwrap();
    assert_eq!(res_deconfig, Ok(vec![]));

    // Test VendorRequest::WritePayload (0x56 - valid data)
    let setup_write = fusb_descriptor::UsbSetup {
        bm_request_type: 0x40,
        b_request: VendorRequest::WritePayload as u8,
        w_value: 0,
        w_index: 0,
        w_length: USB_ZERO_WRITE_PAYLOAD.len() as u16,
    };
    let res_out = proxy.control(&setup_write, USB_ZERO_WRITE_PAYLOAD).await.unwrap();
    assert_eq!(res_out, Ok(vec![]));

    // Test VendorRequest::WritePayload (0x56 - invalid payload content)
    let res_err =
        proxy.control(&setup_write, &vec![0; USB_ZERO_WRITE_PAYLOAD.len()]).await.unwrap();
    assert_eq!(res_err, Err(Status::INVALID_ARGS.into_raw()));

    // Test VendorRequest::WritePayload (0x56 - w_length mismatch)
    let setup_write_mismatch = fusb_descriptor::UsbSetup {
        bm_request_type: 0x40,
        b_request: VendorRequest::WritePayload as u8,
        w_value: 0,
        w_index: 0,
        w_length: (USB_ZERO_WRITE_PAYLOAD.len() + 1) as u16,
    };
    let res_mismatch = proxy.control(&setup_write_mismatch, USB_ZERO_WRITE_PAYLOAD).await.unwrap();
    assert_eq!(res_mismatch, Err(Status::INVALID_ARGS.into_raw()));

    // Test VendorRequest::ReadPayload (0x57 - valid)
    let setup_read = fusb_descriptor::UsbSetup {
        bm_request_type: 0xC0,
        b_request: VendorRequest::ReadPayload as u8,
        w_value: 0,
        w_index: 0,
        w_length: USB_ZERO_READ_PAYLOAD.len() as u16,
    };
    let res = proxy.control(&setup_read, &[]).await.unwrap();
    assert_eq!(res, Ok(USB_ZERO_READ_PAYLOAD.to_vec()));

    // Test VendorRequest::ReadPayload (0x57 - invalid non-empty write payload)
    let res_read_nonempty = proxy.control(&setup_read, &[0x01]).await.unwrap();
    assert_eq!(res_read_nonempty, Err(Status::INVALID_ARGS.into_raw()));

    // Test VendorRequest::SetTestMode (0x58 - set Loopback mode)
    let setup_set_mode_loopback = fusb_descriptor::UsbSetup {
        bm_request_type: 0x40,
        b_request: VendorRequest::SetTestMode as u8,
        w_value: TestMode::Loopback as u16,
        w_index: 0,
        w_length: 0,
    };
    let res_set_mode = proxy.control(&setup_set_mode_loopback, &[]).await.unwrap();
    assert_eq!(res_set_mode, Ok(vec![]));

    // Test VendorRequest::SetTestMode (0x58 - invalid mode 99)
    let setup_set_mode_invalid = fusb_descriptor::UsbSetup {
        bm_request_type: 0x40,
        b_request: VendorRequest::SetTestMode as u8,
        w_value: 99,
        w_index: 0,
        w_length: 0,
    };
    let res_mode_invalid = proxy.control(&setup_set_mode_invalid, &[]).await.unwrap();
    assert_eq!(res_mode_invalid, Err(Status::INVALID_ARGS.into_raw()));

    // Test unsupported request
    let setup_unsupported = fusb_descriptor::UsbSetup {
        bm_request_type: 0x40,
        b_request: VendorRequest::SetTestMode as u8 + 1,
        w_value: 0,
        w_index: 0,
        w_length: 0,
    };
    let res_unsupported = proxy.control(&setup_unsupported, &[]).await.unwrap();
    assert_eq!(res_unsupported, Err(Status::NOT_SUPPORTED.into_raw()));
}

#[fuchsia::test]
async fn test_source_sink() {
    let (ep_in_client, ep_in_server) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let (ep_out_client, ep_out_server) = create_endpoints::<fusb_endpoint::EndpointMarker>();

    let state_in =
        Arc::new(Mutex::new(MockEndpointState { requests: vec![], vmos: HashMap::new() }));
    let state_out =
        Arc::new(Mutex::new(MockEndpointState { requests: vec![], vmos: HashMap::new() }));

    let (comp_in_tx, comp_in_rx) = mpsc::unbounded();
    let (comp_out_tx, comp_out_rx) = mpsc::unbounded();
    let (event_tx, mut event_rx) = mpsc::unbounded();

    let scope = Arc::new(fasync::Scope::new_with_name("test_ss"));

    scope.spawn_local(run_mock_endpoint(
        ep_in_server.into_stream(),
        state_in.clone(),
        comp_in_rx,
        event_tx.clone(),
        scope.clone(),
    ));
    scope.spawn_local(run_mock_endpoint(
        ep_out_server.into_stream(),
        state_out.clone(),
        comp_out_rx,
        event_tx,
        scope.clone(),
    ));

    let ep_in_proxy = ep_in_client.into_proxy();
    let ep_out_proxy = ep_out_client.into_proxy();

    let mut vmos_registered = false;
    let _tasks = run_source_sink(
        ep_in_proxy,
        ep_out_proxy,
        &mut vmos_registered,
        USB_MAX_PACKET_SIZE_HIGH_SPEED.into(),
    )
    .await
    .unwrap();

    // Await setup events: ep_out registered (1), ep_in registered (1),
    // ep_out queued initial request (1), ep_in queued initial request (1)
    for _ in 0..4 {
        let _ = event_rx.next().await;
    }

    // Verify OUT VMO was registered
    let read_req = {
        let mut state = state_out.lock().unwrap();
        assert!(state.vmos.contains_key(&USB_ZERO_OUT_VMO_ID));
        state.requests.pop().unwrap()
    };

    // Verify IN VMO was registered
    let write_req = {
        let mut state = state_in.lock().unwrap();
        assert!(state.vmos.contains_key(&USB_ZERO_IN_VMO_ID));
        state.requests.pop().unwrap()
    };

    // Send mock completion on OUT to assert read loop re-queues
    comp_out_tx
        .unbounded_send(vec![fusb_endpoint::Completion {
            request: Some(read_req),
            status: Some(zx::sys::ZX_OK),
            transfer_size: Some(0),
            ..Default::default()
        }])
        .unwrap();

    let event = event_rx.next().await;
    assert_eq!(event, Some(MockEvent::RequestQueued));
    {
        let state = state_out.lock().unwrap();
        assert_eq!(state.requests.len(), 1);
    }

    // Send mock completion on IN to assert write loop re-queues
    comp_in_tx
        .unbounded_send(vec![fusb_endpoint::Completion {
            request: Some(write_req),
            status: Some(zx::sys::ZX_OK),
            transfer_size: Some(512),
            ..Default::default()
        }])
        .unwrap();

    let event = event_rx.next().await;
    assert_eq!(event, Some(MockEvent::RequestQueued));
    {
        let state = state_in.lock().unwrap();
        assert_eq!(state.requests.len(), 1);
    }
}

#[fuchsia::test]
async fn test_set_and_get_interface() {
    let (iface_c, iface_s) = create_endpoints::<fusb_function::UsbFunctionInterfaceMarker>();
    let (func_c, func_s) = create_endpoints::<fusb_function::UsbFunctionMarker>();
    let (ep_in_c, ep_in_s) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let (ep_out_c, ep_out_s) = create_endpoints::<fusb_endpoint::EndpointMarker>();

    let scope = Arc::new(fasync::Scope::new_with_name("test_set_get_iface"));
    scope.spawn_local(run_mock_function(func_s.into_stream()));
    scope.spawn_local(run_mock_endpoint(
        ep_in_s.into_stream(),
        Default::default(),
        mpsc::unbounded().1,
        mpsc::unbounded().0,
        scope.clone(),
    ));
    scope.spawn_local(run_mock_endpoint(
        ep_out_s.into_stream(),
        Default::default(),
        mpsc::unbounded().1,
        mpsc::unbounded().0,
        scope.clone(),
    ));

    let (f_p, ep_i, ep_o) = (func_c.into_proxy(), ep_in_c.into_proxy(), ep_out_c.into_proxy());
    scope.spawn_local(async move {
        UsbZeroFunctionDevice::new(f_p, ep_i, TEST_EP_IN_ADDR, ep_o, TEST_EP_OUT_ADDR, 0)
            .handle_requests(iface_s.into_stream())
            .await;
    });

    let proxy = iface_c.into_proxy();
    let setup = fusb_descriptor::UsbSetup {
        bm_request_type: 0x81,
        b_request: USB_SETUP_REQ_GET_INTERFACE,
        w_value: 0,
        w_index: 0,
        w_length: 1,
    };

    assert_eq!(proxy.set_interface(0, 1).await.unwrap(), Ok(()));
    assert_eq!(proxy.control(&setup, &[]).await.unwrap(), Ok(vec![0x01]));
    assert_eq!(proxy.set_configured(true, fusb_descriptor::UsbSpeed::High).await.unwrap(), Ok(()));
    assert_eq!(proxy.set_interface(0, 0).await.unwrap(), Ok(()));
    assert_eq!(proxy.control(&setup, &[]).await.unwrap(), Ok(vec![0x00]));
    assert_eq!(proxy.set_interface(0, 1).await.unwrap(), Ok(()));
    assert_eq!(proxy.control(&setup, &[]).await.unwrap(), Ok(vec![0x01]));
    assert_eq!(proxy.set_interface(0, 2).await.unwrap(), Err(Status::NOT_SUPPORTED.into_raw()));
    assert_eq!(proxy.set_interface(1, 0).await.unwrap(), Err(Status::NOT_SUPPORTED.into_raw()));
}
