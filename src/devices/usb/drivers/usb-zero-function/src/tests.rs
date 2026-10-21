// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;
use fdf_component::PropertyValue;
use fidl::endpoints::{RequestStream, create_endpoints};

const TEST_EP_IN_ADDR: u8 = 0x81;
const TEST_EP_OUT_ADDR: u8 = 0x01;

const USB_TYPE_VENDOR_OUT: u8 = fusb_descriptor::EndpointDirection::Out.into_primitive()
    | fusb_descriptor::RequestType::Vendor.into_primitive()
    | fusb_descriptor::RequestRecipient::Device.into_primitive();
const USB_TYPE_VENDOR_IN: u8 = fusb_descriptor::EndpointDirection::In.into_primitive()
    | fusb_descriptor::RequestType::Vendor.into_primitive()
    | fusb_descriptor::RequestRecipient::Device.into_primitive();

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
            fusb_endpoint::EndpointRequest::CancelAll { responder } => {
                let mut completions: Vec<fusb_endpoint::Completion> = {
                    let mut state_lock = state.lock().unwrap();
                    state_lock
                        .requests
                        .drain(..)
                        .map(|req| fusb_endpoint::Completion {
                            request: Some(req),
                            status: Some(zx::sys::ZX_ERR_CANCELED),
                            transfer_size: Some(0),
                            ..Default::default()
                        })
                        .collect()
                };
                if completions.len() > 1 {
                    // Simulate Dwc3::EpServer::CancelAll completing the active request
                    // and remaining pending requests in two separate OnCompletion batches.
                    let rest = completions.split_off(1);
                    let _ = control_handle.send_on_completion(completions);
                    let _ = control_handle.send_on_completion(rest);
                } else if !completions.is_empty() {
                    let _ = control_handle.send_on_completion(completions);
                }
                let _ = responder.send(Ok(()));
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

    let (comp_in_tx, comp_in_rx) = mpsc::unbounded();
    let (comp_out_tx, comp_out_rx) = mpsc::unbounded();
    let (event_in_tx, mut event_in_rx) = mpsc::unbounded();
    let (event_out_tx, mut event_out_rx) = mpsc::unbounded();

    let scope = Arc::new(fasync::Scope::new_with_name("test"));

    scope.spawn_local(run_mock_endpoint(
        ep_in_server.into_stream(),
        state_in.clone(),
        comp_in_rx,
        event_in_tx,
        scope.clone(),
    ));
    scope.spawn_local(run_mock_endpoint(
        ep_out_server.into_stream(),
        state_out.clone(),
        comp_out_rx,
        event_out_tx,
        scope.clone(),
    ));

    let ep_in_proxy = ep_in_client.into_proxy();
    let ep_out_proxy = ep_out_client.into_proxy();

    let mut vmos_registered = false;
    let _tasks = run_loopback(
        ep_in_proxy,
        ep_out_proxy,
        &mut vmos_registered,
        u64::from(USB_MAX_PACKET_SIZE_HIGH_SPEED),
        Arc::new(Mutex::new(ScatterGatherConfig::default())),
    )
    .await
    .unwrap();

    // Await setup events: ep_out registered (1), ep_in registered (1),
    // and ep_out queued read request (1).
    assert_eq!(event_out_rx.next().await, Some(MockEvent::VmoRegistered));
    assert_eq!(event_out_rx.next().await, Some(MockEvent::RequestQueued));
    assert_eq!(event_in_rx.next().await, Some(MockEvent::VmoRegistered));

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

    // Verify read request was queued (request 0 maps to USB_ZERO_OUT_VMO_ID)
    let read_req = {
        let mut state = state_out.lock().unwrap();
        state.requests.remove(0)
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
    assert_eq!(event_in_rx.next().await, Some(MockEvent::RequestQueued));

    // Verify write request was queued on ep_in
    let write_req = {
        let mut state = state_in.lock().unwrap();
        state.requests.pop().unwrap()
    };

    // Complete write
    comp_in_tx
        .unbounded_send(vec![fusb_endpoint::Completion {
            request: Some(write_req),
            status: Some(zx::sys::ZX_OK),
            transfer_size: Some(test_data.len() as u64),
            ..Default::default()
        }])
        .unwrap();

    // Verify data in VMO IN
    let mut read_back = vec![0; test_data.len()];
    vmo_in.read(&mut read_back, 0).unwrap();
    assert_eq!(read_back, test_data);

    // Verify Zero-Length Packet (ZLP) through loopback
    let zlp_read_req = {
        let mut state = state_out.lock().unwrap();
        state.requests.remove(0)
    };
    comp_out_tx
        .unbounded_send(vec![fusb_endpoint::Completion {
            request: Some(zlp_read_req),
            status: Some(zx::sys::ZX_OK),
            transfer_size: Some(0),
            ..Default::default()
        }])
        .unwrap();

    assert_eq!(event_in_rx.next().await, Some(MockEvent::RequestQueued));
    let zlp_write_req = {
        let mut state = state_in.lock().unwrap();
        let req = state.requests.pop().unwrap();
        assert_eq!(req.data.as_deref(), Some(&[][..]));
        req
    };
    comp_in_tx
        .unbounded_send(vec![fusb_endpoint::Completion {
            request: Some(zlp_write_req),
            status: Some(zx::sys::ZX_OK),
            transfer_size: Some(0),
            ..Default::default()
        }])
        .unwrap();
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
            TestMode::SourceSink,
        );
        zero_function.handle_requests(iface_server.into_stream()).await;
    });

    let proxy = iface_client.into_proxy();

    // Test VendorRequest::SetStall (0x50)
    let setup_set_stall = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: VendorRequest::SetStall as u8,
        w_value: TEST_EP_IN_ADDR as u16,
        w_index: 0,
        w_length: 0,
    };
    let res = proxy.control(&setup_set_stall, &[]).await.unwrap();
    assert_eq!(res, Ok(vec![]));

    // Test VendorRequest::SetStall with invalid w_value (> 0xFF)
    let setup_invalid_w_value = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: VendorRequest::SetStall as u8,
        w_value: 0x100,
        w_index: 0,
        w_length: 0,
    };
    let res = proxy.control(&setup_invalid_w_value, &[]).await.unwrap();
    assert_eq!(res, Err(Status::INVALID_ARGS.into_raw()));

    // Test VendorRequest::SetStall with invalid direction bit (0xC0)
    let setup_invalid_dir = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_IN,
        b_request: VendorRequest::SetStall as u8,
        w_value: TEST_EP_IN_ADDR as u16,
        w_index: 0,
        w_length: 0,
    };
    let res = proxy.control(&setup_invalid_dir, &[]).await.unwrap();
    assert_eq!(res, Err(Status::INVALID_ARGS.into_raw()));

    // Test VendorRequest::ClearStall (0x51)
    let setup_clear_stall = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: VendorRequest::ClearStall as u8,
        w_value: TEST_EP_IN_ADDR as u16,
        w_index: 0,
        w_length: 0,
    };
    let res = proxy.control(&setup_clear_stall, &[]).await.unwrap();
    assert_eq!(res, Ok(vec![]));

    // Test VendorRequest::ConfigureEndpoint (0x52)
    let setup_config_ep = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: VendorRequest::ConfigureEndpoint as u8,
        w_value: TEST_EP_IN_ADDR as u16,
        w_index: 0,
        w_length: 0,
    };
    let res = proxy.control(&setup_config_ep, &[]).await.unwrap();
    assert_eq!(res, Ok(vec![]));

    // Test VendorRequest::DisableEndpoint (0x53)
    let setup_disable_ep = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: VendorRequest::DisableEndpoint as u8,
        w_value: TEST_EP_IN_ADDR as u16,
        w_index: 0,
        w_length: 0,
    };
    let res = proxy.control(&setup_disable_ep, &[]).await.unwrap();
    assert_eq!(res, Ok(vec![]));

    // Test VendorRequest::ConnectEndpoint (0x54)
    let setup_connect_ep = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: VendorRequest::ConnectEndpoint as u8,
        w_value: TEST_EP_IN_ADDR as u16,
        w_index: 0,
        w_length: 0,
    };
    let res = proxy.control(&setup_connect_ep, &[]).await.unwrap();
    assert_eq!(res, Ok(vec![]));

    // Test VendorRequest::Deconfigure (0x55)
    let setup_deconfig = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: VendorRequest::Deconfigure as u8,
        w_value: 0,
        w_index: 0,
        w_length: 0,
    };
    let res_deconfig = proxy.control(&setup_deconfig, &[]).await.unwrap();
    assert_eq!(res_deconfig, Ok(vec![]));

    // Test VendorRequest::WritePayload (0x56 - valid data)
    let setup_write = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
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
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: VendorRequest::WritePayload as u8,
        w_value: 0,
        w_index: 0,
        w_length: (USB_ZERO_WRITE_PAYLOAD.len() + 1) as u16,
    };
    let res_mismatch = proxy.control(&setup_write_mismatch, USB_ZERO_WRITE_PAYLOAD).await.unwrap();
    assert_eq!(res_mismatch, Err(Status::INVALID_ARGS.into_raw()));

    // Test VendorRequest::ReadPayload (0x57 - valid)
    let setup_read = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_IN,
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

    // Test VendorRequest::SetTestMode (0x58 - rejected for now, mode is fixed per configuration)
    let setup_set_mode_loopback = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: VendorRequest::SetTestMode as u8,
        w_value: TestMode::Loopback as u16,
        w_index: 0,
        w_length: 0,
    };
    let res_set_mode = proxy.control(&setup_set_mode_loopback, &[]).await.unwrap();
    assert_eq!(res_set_mode, Err(Status::NOT_SUPPORTED.into_raw()));

    // Test VendorRequest::ControlLoopbackOut (0x5b) and ControlLoopbackIn (0x5c)
    let payload = vec![1, 2, 3, 4, 5];
    let setup_control_loopback_out = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: VendorRequest::ControlLoopbackOut as u8,
        w_value: 0,
        w_index: 0,
        w_length: payload.len() as u16,
    };
    let res_cl_out = proxy.control(&setup_control_loopback_out, &payload).await.unwrap();
    assert_eq!(res_cl_out, Ok(vec![]));

    let setup_control_loopback_in = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_IN,
        b_request: VendorRequest::ControlLoopbackIn as u8,
        w_value: 0,
        w_index: 0,
        w_length: payload.len() as u16,
    };
    let res_cl_in = proxy.control(&setup_control_loopback_in, &[]).await.unwrap();
    assert_eq!(res_cl_in, Ok(payload));

    // Test VendorRequest::GetTestMode (0x59)
    let setup_get_test_mode = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_IN,
        b_request: VendorRequest::GetTestMode as u8,
        w_value: 0,
        w_index: 0,
        w_length: 1,
    };
    let res_get_mode = proxy.control(&setup_get_test_mode, &[]).await.unwrap();
    assert_eq!(res_get_mode, Ok(vec![TestMode::default() as u8]));

    // Test invalid vendor request (opcode 0x00 with bm_request_type = 0x40)
    let setup_invalid_vendor = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: 0x00,
        w_value: 0,
        w_index: 0,
        w_length: 0,
    };
    let res_invalid_vendor = proxy.control(&setup_invalid_vendor, &[]).await.unwrap();
    assert_eq!(res_invalid_vendor, Err(Status::NOT_SUPPORTED.into_raw()));

    // Test unsupported request
    let setup_unsupported = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: 0xff,
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
        assert_eq!(state.requests.len(), QUEUE_DEPTH);
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
        assert_eq!(state.requests.len(), QUEUE_DEPTH);
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

    let f_p = func_c.into_proxy();
    let ep_i = ep_in_c.into_proxy();
    let ep_o = ep_out_c.into_proxy();
    scope.spawn_local(async move {
        UsbZeroFunctionDevice::new(
            f_p,
            ep_i,
            TEST_EP_IN_ADDR,
            ep_o,
            TEST_EP_OUT_ADDR,
            0,
            TestMode::SourceSink,
        )
        .handle_requests(iface_s.into_stream())
        .await;
    });

    let proxy = iface_c.into_proxy();
    let setup = fusb_descriptor::UsbSetup {
        bm_request_type: 0x81,
        b_request: fusb_descriptor::StandardRequest::GetInterface.into_primitive(),
        w_value: 0,
        w_index: 0,
        w_length: 1,
    };

    // GetInterface initially returns alt 0
    assert_eq!(proxy.control(&setup, &[]).await.unwrap(), Ok(vec![0x00]));

    // Alternate setting 0 succeeds and resets endpoints
    assert_eq!(proxy.set_interface(0, 0).await.unwrap(), Ok(()));
    assert_eq!(proxy.control(&setup, &[]).await.unwrap(), Ok(vec![0x00]));

    // Configure device, alternate setting 0 still succeeds
    assert_eq!(proxy.set_configured(true, fusb_descriptor::UsbSpeed::High).await.unwrap(), Ok(()));
    assert_eq!(proxy.set_interface(0, 0).await.unwrap(), Ok(()));
    assert_eq!(proxy.control(&setup, &[]).await.unwrap(), Ok(vec![0x00]));

    // Alternate settings > 0 are rejected.
    assert_eq!(proxy.set_interface(0, 1).await.unwrap(), Err(Status::NOT_SUPPORTED.into_raw()));
    assert_eq!(proxy.set_interface(0, 2).await.unwrap(), Err(Status::NOT_SUPPORTED.into_raw()));
    assert_eq!(proxy.set_interface(1, 0).await.unwrap(), Err(Status::NOT_SUPPORTED.into_raw()));
}

#[fuchsia::test]
async fn test_endpoint_stall_state() {
    let (func_client, func_server) = create_endpoints::<fusb_function::UsbFunctionMarker>();
    let (ep_in_client, _ep_in_server) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let (ep_out_client, _ep_out_server) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let scope = Arc::new(fasync::Scope::new_with_name("test_stall"));
    scope.spawn_local(run_mock_function(func_server.into_stream()));

    let mut device = UsbZeroFunctionDevice::new(
        func_client.into_proxy(),
        ep_in_client.into_proxy(),
        TEST_EP_IN_ADDR,
        ep_out_client.into_proxy(),
        TEST_EP_OUT_ADDR,
        0,
        TestMode::SourceSink,
    );

    // Initial state: no stalled endpoints
    assert!(device.stalled_endpoints.is_empty());

    // Stall IN endpoint
    device.set_endpoint_stall(TEST_EP_IN_ADDR).await.unwrap();
    assert!(device.stalled_endpoints.contains(&TEST_EP_IN_ADDR));
    assert_eq!(device.stalled_endpoints.len(), 1);

    // Stall OUT endpoint
    device.set_endpoint_stall(TEST_EP_OUT_ADDR).await.unwrap();
    assert!(device.stalled_endpoints.contains(&TEST_EP_IN_ADDR));
    assert!(device.stalled_endpoints.contains(&TEST_EP_OUT_ADDR));
    assert_eq!(device.stalled_endpoints.len(), 2);

    // Stall EP0 (invalid)
    assert_eq!(device.set_endpoint_stall(0).await, Err(Status::INVALID_ARGS));
    assert_eq!(device.set_endpoint_stall(0x80).await, Err(Status::INVALID_ARGS));

    // Clear IN endpoint stall
    device.clear_endpoint_stall(TEST_EP_IN_ADDR).await.unwrap();
    assert!(!device.stalled_endpoints.contains(&TEST_EP_IN_ADDR));
    assert!(device.stalled_endpoints.contains(&TEST_EP_OUT_ADDR));
    assert_eq!(device.stalled_endpoints.len(), 1);

    // Clear EP0 stall (should be no-op and succeed)
    device.clear_endpoint_stall(0).await.unwrap();
    device.clear_endpoint_stall(0x80).await.unwrap();
    assert!(device.stalled_endpoints.contains(&TEST_EP_OUT_ADDR));
    assert_eq!(device.stalled_endpoints.len(), 1);

    // Clear OUT endpoint stall
    device.clear_endpoint_stall(TEST_EP_OUT_ADDR).await.unwrap();
    assert!(device.stalled_endpoints.is_empty());
}

#[fuchsia::test]
async fn test_standard_endpoint_halt() {
    let in_ep = TEST_EP_IN_ADDR as u16;
    let (iface_c, iface_s) = create_endpoints::<fusb_function::UsbFunctionInterfaceMarker>();
    let (func_c, func_s) = create_endpoints::<fusb_function::UsbFunctionMarker>();
    let (ep_in_c, _) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let (ep_out_c, _) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let scope = Arc::new(fasync::Scope::new_with_name("test_halt"));
    scope.spawn_local(run_mock_function(func_s.into_stream()));
    let mut dev = UsbZeroFunctionDevice::new(
        func_c.into_proxy(),
        ep_in_c.into_proxy(),
        TEST_EP_IN_ADDR,
        ep_out_c.into_proxy(),
        TEST_EP_OUT_ADDR,
        0, // interface_num
        TestMode::SourceSink,
    );

    // EP0 (Control Endpoint) stall management is handled by hardware / driver stack
    // and cannot be stalled via this vendor request. Return INVALID_ARGS for EP0.
    assert_eq!(dev.set_endpoint_stall(0).await, Err(Status::INVALID_ARGS));
    // Clearing stall on EP0 is a no-op because EP0 stall status automatically resets upon the next setup packet.
    assert_eq!(dev.clear_endpoint_stall(0).await, Ok(()));

    scope.spawn_local(async move {
        dev.handle_requests(iface_s.into_stream()).await;
    });

    let proxy = iface_c.into_proxy();
    macro_rules! control {
        ($setup:expr) => {
            proxy.control(&$setup, &[]).await.unwrap()
        };
    }

    let get_status = |bm, idx| fusb_descriptor::UsbSetup {
        bm_request_type: bm,
        b_request: fusb_descriptor::StandardRequest::GetStatus.into_primitive(),
        w_value: 0,
        w_index: idx,
        w_length: 2,
    };
    let set_halt = |bm, val, idx| fusb_descriptor::UsbSetup {
        bm_request_type: bm,
        b_request: fusb_descriptor::StandardRequest::SetFeature.into_primitive(),
        w_value: val,
        w_index: idx,
        w_length: 0,
    };
    let clear_halt = |bm, val, idx| fusb_descriptor::UsbSetup {
        bm_request_type: bm,
        b_request: fusb_descriptor::StandardRequest::ClearFeature.into_primitive(),
        w_value: val,
        w_index: idx,
        w_length: 0,
    };

    // GET_STATUS on device (0), interface (0), and unhalted endpoint (0)
    assert_eq!(
        control!(get_status(fusb_descriptor::STANDARD_DEVICE_REQUEST_IN, 0)),
        Ok(vec![0, 0])
    );
    assert_eq!(
        control!(get_status(fusb_descriptor::STANDARD_INTERFACE_REQUEST_IN, 0)),
        Ok(vec![0, 0])
    );
    assert_eq!(
        control!(get_status(fusb_descriptor::STANDARD_INTERFACE_REQUEST_IN, 1)),
        Err(Status::NOT_SUPPORTED.into_raw())
    );
    assert_eq!(
        control!(get_status(fusb_descriptor::STANDARD_ENDPOINT_REQUEST_IN, in_ep)),
        Ok(vec![0, 0])
    );

    // SET_FEATURE(ENDPOINT_HALT) -> GET_STATUS (1)
    assert_eq!(
        control!(set_halt(
            fusb_descriptor::STANDARD_ENDPOINT_REQUEST_OUT,
            fusb_descriptor::FeatureSelector::EndpointHalt.into_primitive() as u16,
            in_ep
        )),
        Ok(vec![])
    );
    assert_eq!(
        control!(get_status(fusb_descriptor::STANDARD_ENDPOINT_REQUEST_IN, in_ep)),
        Ok(vec![1, 0])
    );

    // CLEAR_FEATURE(ENDPOINT_HALT) -> GET_STATUS (0)
    assert_eq!(
        control!(clear_halt(
            fusb_descriptor::STANDARD_ENDPOINT_REQUEST_OUT,
            fusb_descriptor::FeatureSelector::EndpointHalt.into_primitive() as u16,
            in_ep
        )),
        Ok(vec![])
    );
    assert_eq!(
        control!(get_status(fusb_descriptor::STANDARD_ENDPOINT_REQUEST_IN, in_ep)),
        Ok(vec![0, 0])
    );

    // Negative validations: invalid feature, recipient, endpoint address, direction
    assert_eq!(
        control!(set_halt(fusb_descriptor::STANDARD_ENDPOINT_REQUEST_OUT, 1, in_ep)),
        Err(Status::NOT_SUPPORTED.into_raw())
    );
    assert_eq!(
        control!(set_halt(
            fusb_descriptor::STANDARD_DEVICE_REQUEST_OUT,
            fusb_descriptor::FeatureSelector::EndpointHalt.into_primitive() as u16,
            in_ep
        )),
        Err(Status::NOT_SUPPORTED.into_raw())
    );
    assert_eq!(
        control!(set_halt(
            fusb_descriptor::STANDARD_ENDPOINT_REQUEST_OUT,
            fusb_descriptor::FeatureSelector::EndpointHalt.into_primitive() as u16,
            99
        )),
        Err(Status::NOT_SUPPORTED.into_raw())
    );
    assert_eq!(
        control!(set_halt(
            fusb_descriptor::STANDARD_ENDPOINT_REQUEST_IN,
            fusb_descriptor::FeatureSelector::EndpointHalt.into_primitive() as u16,
            in_ep
        )),
        Err(Status::NOT_SUPPORTED.into_raw())
    );
}

fn parent_property<'a>(key: &'a str, value: impl Into<PropertyValue>) -> ParentProperty<'a> {
    ParentProperty { parent: "default", key, value: value.into() }
}

#[fuchsia::test]
fn test_get_usb_protocol_parsing() {
    assert_eq!(get_usb_protocol([parent_property(BIND_USB_PROTOCOL_KEY, 1u32)]), Some(1));
    assert_eq!(get_usb_protocol([parent_property(BIND_USB_PROTOCOL_KEY, 2u32)]), Some(2));
    assert_eq!(get_usb_protocol([]), None);
}

#[fuchsia::test]
async fn test_loopback_mode_and_set_interface() {
    let (iface_c, iface_s) = create_endpoints::<fusb_function::UsbFunctionInterfaceMarker>();
    let (func_c, func_s) = create_endpoints::<fusb_function::UsbFunctionMarker>();
    let (ep_in_c, ep_in_s) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let (ep_out_c, ep_out_s) = create_endpoints::<fusb_endpoint::EndpointMarker>();

    let scope = Arc::new(fasync::Scope::new_with_name("test_loopback_mode"));
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
        UsbZeroFunctionDevice::new(
            f_p,
            ep_i,
            TEST_EP_IN_ADDR,
            ep_o,
            TEST_EP_OUT_ADDR,
            0,
            TestMode::Loopback,
        )
        .handle_requests(iface_s.into_stream())
        .await;
    });

    let proxy = iface_c.into_proxy();
    let setup = fusb_descriptor::UsbSetup {
        bm_request_type: 0x81,
        b_request: fusb_descriptor::StandardRequest::GetInterface.into_primitive(),
        w_value: 0,
        w_index: 0,
        w_length: 1,
    };

    // GetInterface returns alt 0
    assert_eq!(proxy.control(&setup, &[]).await.unwrap(), Ok(vec![0x00]));

    // FIDL SetInterface(0, 0) succeeds
    assert_eq!(proxy.set_interface(0, 0).await.unwrap(), Ok(()));
    assert_eq!(proxy.control(&setup, &[]).await.unwrap(), Ok(vec![0x00]));

    // FIDL SetInterface(0, 1) fails (only alt 0 is supported)
    assert_eq!(proxy.set_interface(0, 1).await.unwrap(), Err(Status::NOT_SUPPORTED.into_raw()));

    // Configure device
    assert_eq!(proxy.set_configured(true, fusb_descriptor::UsbSpeed::High).await.unwrap(), Ok(()));
    assert_eq!(proxy.control(&setup, &[]).await.unwrap(), Ok(vec![0x00]));

    // SetInterface(0, 0) while configured succeeds and resets endpoints
    assert_eq!(proxy.set_interface(0, 0).await.unwrap(), Ok(()));
    assert_eq!(proxy.control(&setup, &[]).await.unwrap(), Ok(vec![0x00]));

    // SetInterface(0, 1) while configured fails
    assert_eq!(proxy.set_interface(0, 1).await.unwrap(), Err(Status::NOT_SUPPORTED.into_raw()));

    // Test Chapter 9 SET_INTERFACE control request (alt 0 succeeds)
    let setup_set_interface_0 = fusb_descriptor::UsbSetup {
        bm_request_type: 0x01,
        b_request: fusb_descriptor::StandardRequest::SetInterface.into_primitive(),
        w_value: 0,
        w_index: 0,
        w_length: 0,
    };
    assert_eq!(proxy.control(&setup_set_interface_0, &[]).await.unwrap(), Ok(vec![]));
    assert_eq!(proxy.control(&setup, &[]).await.unwrap(), Ok(vec![0x00]));

    // USB 2.0 §9.4.10: SET_INTERFACE resets halt state on all interface endpoints
    let set_halt = |ep_addr: u8| fusb_descriptor::UsbSetup {
        bm_request_type: 0x02, // OUT, Standard, Endpoint
        b_request: fusb_descriptor::StandardRequest::SetFeature.into_primitive(),
        w_value: fusb_descriptor::FeatureSelector::EndpointHalt.into_primitive() as u16,
        w_index: ep_addr as u16,
        w_length: 0,
    };
    let get_ep_status = |ep_addr: u8| fusb_descriptor::UsbSetup {
        bm_request_type: 0x82, // IN, Standard, Endpoint
        b_request: fusb_descriptor::StandardRequest::GetStatus.into_primitive(),
        w_value: 0,
        w_index: ep_addr as u16,
        w_length: 2,
    };

    // Stall endpoints and verify stalled
    assert_eq!(proxy.control(&set_halt(TEST_EP_IN_ADDR), &[]).await.unwrap(), Ok(vec![]));
    assert_eq!(proxy.control(&set_halt(TEST_EP_OUT_ADDR), &[]).await.unwrap(), Ok(vec![]));
    assert_eq!(
        proxy.control(&get_ep_status(TEST_EP_IN_ADDR), &[]).await.unwrap(),
        Ok(vec![0x01, 0x00])
    );
    assert_eq!(
        proxy.control(&get_ep_status(TEST_EP_OUT_ADDR), &[]).await.unwrap(),
        Ok(vec![0x01, 0x00])
    );

    // Chapter 9 SET_INTERFACE(0, 0) resets halt state on all interface endpoints
    assert_eq!(proxy.control(&setup_set_interface_0, &[]).await.unwrap(), Ok(vec![]));
    assert_eq!(
        proxy.control(&get_ep_status(TEST_EP_IN_ADDR), &[]).await.unwrap(),
        Ok(vec![0x00, 0x00])
    );
    assert_eq!(
        proxy.control(&get_ep_status(TEST_EP_OUT_ADDR), &[]).await.unwrap(),
        Ok(vec![0x00, 0x00])
    );

    // Re-stall and verify FIDL SetInterface(0, 0) also resets halt state
    assert_eq!(proxy.control(&set_halt(TEST_EP_IN_ADDR), &[]).await.unwrap(), Ok(vec![]));
    assert_eq!(proxy.control(&set_halt(TEST_EP_OUT_ADDR), &[]).await.unwrap(), Ok(vec![]));
    assert_eq!(proxy.set_interface(0, 0).await.unwrap(), Ok(()));
    assert_eq!(
        proxy.control(&get_ep_status(TEST_EP_IN_ADDR), &[]).await.unwrap(),
        Ok(vec![0x00, 0x00])
    );
    assert_eq!(
        proxy.control(&get_ep_status(TEST_EP_OUT_ADDR), &[]).await.unwrap(),
        Ok(vec![0x00, 0x00])
    );

    // Test Chapter 9 SET_INTERFACE control request (alt 1 fails)
    let setup_set_interface_1 = fusb_descriptor::UsbSetup {
        bm_request_type: 0x01,
        b_request: fusb_descriptor::StandardRequest::SetInterface.into_primitive(),
        w_value: 1,
        w_index: 0,
        w_length: 0,
    };
    assert_eq!(
        proxy.control(&setup_set_interface_1, &[]).await.unwrap(),
        Err(Status::NOT_SUPPORTED.into_raw())
    );

    // Verify integer truncation protection (> 255 does not truncate to 0)
    let setup_trunc_val = fusb_descriptor::UsbSetup {
        bm_request_type: 0x01,
        b_request: fusb_descriptor::StandardRequest::SetInterface.into_primitive(),
        w_value: 0x0100, // 256
        w_index: 0,
        w_length: 0,
    };
    assert_eq!(
        proxy.control(&setup_trunc_val, &[]).await.unwrap(),
        Err(Status::NOT_SUPPORTED.into_raw())
    );

    let setup_trunc_idx = fusb_descriptor::UsbSetup {
        bm_request_type: 0x01,
        b_request: fusb_descriptor::StandardRequest::SetInterface.into_primitive(),
        w_value: 0,
        w_index: 0x0100, // 256
        w_length: 0,
    };
    assert_eq!(
        proxy.control(&setup_trunc_idx, &[]).await.unwrap(),
        Err(Status::NOT_SUPPORTED.into_raw())
    );

    // Test VendorRequest::SetTestMode is rejected
    let setup_set_mode = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: VendorRequest::SetTestMode as u8,
        w_value: TestMode::SourceSink as u16,
        w_index: 0,
        w_length: 0,
    };
    assert_eq!(
        proxy.control(&setup_set_mode, &[]).await.unwrap(),
        Err(Status::NOT_SUPPORTED.into_raw())
    );

    // Test VendorRequest::GetTestMode returns Loopback
    let setup_get_mode = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_IN,
        b_request: VendorRequest::GetTestMode as u8,
        w_value: 0,
        w_index: 0,
        w_length: 1,
    };
    assert_eq!(
        proxy.control(&setup_get_mode, &[]).await.unwrap(),
        Ok(vec![TestMode::Loopback as u8])
    );
}

#[fuchsia::test]
async fn test_source_sink_cancellation() {
    let (ep_in_client, ep_in_server) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let (ep_out_client, ep_out_server) = create_endpoints::<fusb_endpoint::EndpointMarker>();

    let state_in =
        Arc::new(Mutex::new(MockEndpointState { requests: vec![], vmos: HashMap::new() }));
    let state_out =
        Arc::new(Mutex::new(MockEndpointState { requests: vec![], vmos: HashMap::new() }));

    let (comp_in_tx, comp_in_rx) = mpsc::unbounded();
    let (comp_out_tx, comp_out_rx) = mpsc::unbounded();
    let (event_in_tx, mut event_in_rx) = mpsc::unbounded();
    let (event_out_tx, mut event_out_rx) = mpsc::unbounded();

    let scope = Arc::new(fasync::Scope::new_with_name("test_ss_cancel"));

    scope.spawn_local(run_mock_endpoint(
        ep_in_server.into_stream(),
        state_in.clone(),
        comp_in_rx,
        event_in_tx,
        scope.clone(),
    ));
    scope.spawn_local(run_mock_endpoint(
        ep_out_server.into_stream(),
        state_out.clone(),
        comp_out_rx,
        event_out_tx,
        scope.clone(),
    ));

    let ep_in_proxy = ep_in_client.into_proxy();
    let ep_out_proxy = ep_out_client.into_proxy();

    let mut vmos_registered = false;
    let (sink_task, source_task) = run_source_sink(
        ep_in_proxy,
        ep_out_proxy,
        &mut vmos_registered,
        USB_MAX_PACKET_SIZE_HIGH_SPEED.into(),
    )
    .await
    .unwrap();

    assert_eq!(event_out_rx.next().await, Some(MockEvent::VmoRegistered));
    assert_eq!(event_out_rx.next().await, Some(MockEvent::RequestQueued));
    assert_eq!(event_in_rx.next().await, Some(MockEvent::VmoRegistered));
    assert_eq!(event_in_rx.next().await, Some(MockEvent::RequestQueued));

    // Verify cancellation in a mixed batch on OUT endpoint
    let (canceled_out_req, valid_out_req) = {
        let mut state = state_out.lock().unwrap();
        let c_req = state.requests.pop().unwrap();
        let v_req = state.requests.pop().unwrap();
        assert_eq!(state.requests.len(), QUEUE_DEPTH - 2);
        (c_req, v_req)
    };

    comp_out_tx
        .unbounded_send(vec![
            fusb_endpoint::Completion {
                request: Some(canceled_out_req),
                status: Some(zx::sys::ZX_ERR_CANCELED),
                transfer_size: Some(0),
                ..Default::default()
            },
            fusb_endpoint::Completion {
                request: Some(valid_out_req),
                status: Some(zx::sys::ZX_OK),
                transfer_size: Some(0),
                ..Default::default()
            },
        ])
        .unwrap();

    let event = event_out_rx.next().await;
    assert_eq!(event, Some(MockEvent::RequestQueued));
    {
        let state = state_out.lock().unwrap();
        // Canceled transfer dropped; only valid transfer re-queued.
        assert_eq!(state.requests.len(), QUEUE_DEPTH - 1);
    }

    // Verify IO_REFUSED re-queues transfer to maintain ring depth on OUT endpoint
    let io_refused_out_req = {
        let mut state = state_out.lock().unwrap();
        let req = state.requests.pop().unwrap();
        assert_eq!(state.requests.len(), QUEUE_DEPTH - 2);
        req
    };
    comp_out_tx
        .unbounded_send(vec![fusb_endpoint::Completion {
            request: Some(io_refused_out_req),
            status: Some(zx::sys::ZX_ERR_IO_REFUSED),
            transfer_size: Some(0),
            ..Default::default()
        }])
        .unwrap();

    let event = event_out_rx.next().await;
    assert_eq!(event, Some(MockEvent::RequestQueued));
    {
        let state = state_out.lock().unwrap();
        // IO_REFUSED transfer re-queued to maintain ring depth.
        assert_eq!(state.requests.len(), QUEUE_DEPTH - 1);
    }

    // Verify IO_NOT_PRESENT terminates pump task without re-queuing on OUT endpoint
    let io_not_present_out_req = {
        let mut state = state_out.lock().unwrap();
        let req = state.requests.pop().unwrap();
        assert_eq!(state.requests.len(), QUEUE_DEPTH - 2);
        req
    };
    comp_out_tx
        .unbounded_send(vec![fusb_endpoint::Completion {
            request: Some(io_not_present_out_req),
            status: Some(zx::sys::ZX_ERR_IO_NOT_PRESENT),
            transfer_size: Some(0),
            ..Default::default()
        }])
        .unwrap();

    sink_task.await;
    {
        let state = state_out.lock().unwrap();
        assert_eq!(state.requests.len(), QUEUE_DEPTH - 2);
    }

    // Verify cancellation in a mixed batch on IN endpoint
    let (canceled_in_req, valid_in_req) = {
        let mut state = state_in.lock().unwrap();
        let c_req = state.requests.pop().unwrap();
        let v_req = state.requests.pop().unwrap();
        assert_eq!(state.requests.len(), QUEUE_DEPTH - 2);
        (c_req, v_req)
    };

    comp_in_tx
        .unbounded_send(vec![
            fusb_endpoint::Completion {
                request: Some(canceled_in_req),
                status: Some(zx::sys::ZX_ERR_CANCELED),
                transfer_size: Some(0),
                ..Default::default()
            },
            fusb_endpoint::Completion {
                request: Some(valid_in_req),
                status: Some(zx::sys::ZX_OK),
                transfer_size: Some(512),
                ..Default::default()
            },
        ])
        .unwrap();

    let event = event_in_rx.next().await;
    assert_eq!(event, Some(MockEvent::RequestQueued));
    {
        let state = state_in.lock().unwrap();
        // Canceled transfer dropped; only valid transfer re-queued.
        assert_eq!(state.requests.len(), QUEUE_DEPTH - 1);
    }

    // Verify IO_REFUSED re-queues transfer to maintain ring depth on IN endpoint
    let io_refused_in_req = {
        let mut state = state_in.lock().unwrap();
        let req = state.requests.pop().unwrap();
        assert_eq!(state.requests.len(), QUEUE_DEPTH - 2);
        req
    };
    comp_in_tx
        .unbounded_send(vec![fusb_endpoint::Completion {
            request: Some(io_refused_in_req),
            status: Some(zx::sys::ZX_ERR_IO_REFUSED),
            transfer_size: Some(0),
            ..Default::default()
        }])
        .unwrap();

    let event = event_in_rx.next().await;
    assert_eq!(event, Some(MockEvent::RequestQueued));
    {
        let state = state_in.lock().unwrap();
        // IO_REFUSED transfer re-queued to maintain ring depth.
        assert_eq!(state.requests.len(), QUEUE_DEPTH - 1);
    }

    // Verify IO_NOT_PRESENT terminates pump task without re-queuing on IN endpoint
    let io_not_present_in_req = {
        let mut state = state_in.lock().unwrap();
        let req = state.requests.pop().unwrap();
        assert_eq!(state.requests.len(), QUEUE_DEPTH - 2);
        req
    };
    comp_in_tx
        .unbounded_send(vec![fusb_endpoint::Completion {
            request: Some(io_not_present_in_req),
            status: Some(zx::sys::ZX_ERR_IO_NOT_PRESENT),
            transfer_size: Some(0),
            ..Default::default()
        }])
        .unwrap();

    source_task.await;
    {
        let state = state_in.lock().unwrap();
        assert_eq!(state.requests.len(), QUEUE_DEPTH - 2);
    }
}

#[fuchsia::test]
async fn test_loopback_cancellation() {
    let (ep_in_client, ep_in_server) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let (ep_out_client, ep_out_server) = create_endpoints::<fusb_endpoint::EndpointMarker>();

    let state_in =
        Arc::new(Mutex::new(MockEndpointState { requests: vec![], vmos: HashMap::new() }));
    let state_out =
        Arc::new(Mutex::new(MockEndpointState { requests: vec![], vmos: HashMap::new() }));

    let (_comp_in_tx, comp_in_rx) = mpsc::unbounded();
    let (comp_out_tx, comp_out_rx) = mpsc::unbounded();
    let (event_in_tx, _event_in_rx) = mpsc::unbounded();
    let (event_out_tx, mut event_out_rx) = mpsc::unbounded();

    let scope = Arc::new(fasync::Scope::new_with_name("test_loopback_cancellation"));

    scope.spawn_local(run_mock_endpoint(
        ep_in_server.into_stream(),
        state_in.clone(),
        comp_in_rx,
        event_in_tx,
        scope.clone(),
    ));
    scope.spawn_local(run_mock_endpoint(
        ep_out_server.into_stream(),
        state_out.clone(),
        comp_out_rx,
        event_out_tx,
        scope.clone(),
    ));

    let ep_in_proxy = ep_in_client.into_proxy();
    let ep_out_proxy = ep_out_client.into_proxy();

    let mut vmos_registered = false;
    let (read_task, _write_task, _requeue_rx) = run_loopback(
        ep_in_proxy,
        ep_out_proxy,
        &mut vmos_registered,
        u64::from(USB_MAX_PACKET_SIZE_HIGH_SPEED),
        Arc::new(Mutex::new(ScatterGatherConfig::default())),
    )
    .await
    .unwrap();

    assert_eq!(event_out_rx.next().await, Some(MockEvent::VmoRegistered));
    assert_eq!(event_out_rx.next().await, Some(MockEvent::RequestQueued));

    // Verify cancellation in a mixed batch on OUT endpoint
    let (canceled_out_req, valid_out_req) = {
        let mut state = state_out.lock().unwrap();
        let c_req = state.requests.pop().unwrap();
        let v_req = state.requests.pop().unwrap();
        assert_eq!(state.requests.len(), QUEUE_DEPTH - 2);
        (c_req, v_req)
    };

    comp_out_tx
        .unbounded_send(vec![
            fusb_endpoint::Completion {
                request: Some(canceled_out_req),
                status: Some(zx::sys::ZX_ERR_CANCELED),
                transfer_size: Some(0),
                ..Default::default()
            },
            fusb_endpoint::Completion {
                request: Some(valid_out_req),
                status: Some(zx::sys::ZX_OK),
                transfer_size: Some(0),
                ..Default::default()
            },
        ])
        .unwrap();

    let event = event_out_rx.next().await;
    assert_eq!(event, Some(MockEvent::RequestQueued));
    {
        let state = state_out.lock().unwrap();
        // Canceled transfer dropped; only valid transfer re-queued.
        assert_eq!(state.requests.len(), QUEUE_DEPTH - 1);
    }

    // Verify IO_REFUSED re-queues buffer to maintain ring depth (endpoint halt / abort)
    let io_refused_out_req = {
        let mut state = state_out.lock().unwrap();
        let req = state.requests.pop().unwrap();
        assert_eq!(state.requests.len(), QUEUE_DEPTH - 2);
        req
    };
    comp_out_tx
        .unbounded_send(vec![fusb_endpoint::Completion {
            request: Some(io_refused_out_req),
            status: Some(zx::sys::ZX_ERR_IO_REFUSED),
            transfer_size: Some(0),
            ..Default::default()
        }])
        .unwrap();

    let event = event_out_rx.next().await;
    assert_eq!(event, Some(MockEvent::RequestQueued));
    {
        let state = state_out.lock().unwrap();
        // IO_REFUSED transfer re-queued to maintain ring depth.
        assert_eq!(state.requests.len(), QUEUE_DEPTH - 1);
    }

    // Verify IO_NOT_PRESENT terminates loopback read task without re-queuing (disconnect)
    let io_not_present_out_req = {
        let mut state = state_out.lock().unwrap();
        let req = state.requests.pop().unwrap();
        assert_eq!(state.requests.len(), QUEUE_DEPTH - 2);
        req
    };
    comp_out_tx
        .unbounded_send(vec![fusb_endpoint::Completion {
            request: Some(io_not_present_out_req),
            status: Some(zx::sys::ZX_ERR_IO_NOT_PRESENT),
            transfer_size: Some(0),
            ..Default::default()
        }])
        .unwrap();

    read_task.await;
    {
        let state = state_out.lock().unwrap();
        assert_eq!(state.requests.len(), QUEUE_DEPTH - 2);
    }
}

#[fuchsia::test]
async fn test_transfer_size_exceeds_vmo_size() {
    let (ep_in_client, _ep_in_server) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let (ep_out_client, _ep_out_server) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let mut vmos_registered = false;
    let res = run_source_sink(
        ep_in_client.into_proxy(),
        ep_out_client.into_proxy(),
        &mut vmos_registered,
        DEFAULT_VMO_SIZE + 1,
    )
    .await;
    assert_eq!(res.err(), Some(Status::INVALID_ARGS));

    let (ep_in_client, _ep_in_server) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let (ep_out_client, _ep_out_server) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let mut vmos_registered = false;
    let res = run_loopback(
        ep_in_client.into_proxy(),
        ep_out_client.into_proxy(),
        &mut vmos_registered,
        DEFAULT_VMO_SIZE + 1,
        Arc::new(Mutex::new(ScatterGatherConfig::default())),
    )
    .await;
    assert_eq!(res.err(), Some(Status::INVALID_ARGS));
}

#[fuchsia::test]
async fn test_register_vmos_out_of_order_and_mismatch() {
    let (ep_client, ep_server) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let mut ep_stream = ep_server.into_stream();
    let scope = Arc::new(fasync::Scope::new());
    let s = scope.clone();
    s.spawn_local(async move {
        while let Ok(Some(req)) = ep_stream.try_next().await {
            match req {
                fusb_endpoint::EndpointRequest::UnregisterVmos { responder, .. } => {
                    let _ = responder.send(&[], &[]);
                }
                fusb_endpoint::EndpointRequest::RegisterVmos { vmo_ids, responder } => {
                    // Return VMOs in reverse order to test sorting
                    let mut vmos = vec![];
                    for info in vmo_ids.into_iter().rev() {
                        let id = info.id.unwrap();
                        let size = info.size.unwrap();
                        let vmo = zx::Vmo::create(size).unwrap();
                        let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
                        vmos.push(fusb_endpoint::VmoHandle {
                            id: Some(id),
                            vmo: Some(dup),
                            ..Default::default()
                        });
                    }
                    let _ = responder.send(vmos);
                }
                _ => {}
            }
        }
    });

    let ep_proxy = ep_client.into_proxy();
    let vmos = register_vmos(&ep_proxy, 100, 4, 4096).await.expect("register_vmos should sort");
    assert_eq!(vmos.len(), 4);

    // Test mismatched ID
    let (ep_client2, ep_server2) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let mut ep_stream2 = ep_server2.into_stream();
    let s2 = scope.clone();
    s2.spawn_local(async move {
        while let Ok(Some(req)) = ep_stream2.try_next().await {
            match req {
                fusb_endpoint::EndpointRequest::UnregisterVmos { responder, .. } => {
                    let _ = responder.send(&[], &[]);
                }
                fusb_endpoint::EndpointRequest::RegisterVmos { responder, .. } => {
                    let vmo = zx::Vmo::create(4096).unwrap();
                    let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
                    let vmos = vec![fusb_endpoint::VmoHandle {
                        id: Some(999), // Mismatched! Expected 100
                        vmo: Some(dup),
                        ..Default::default()
                    }];
                    let _ = responder.send(vmos);
                }
                _ => {}
            }
        }
    });

    let ep_proxy2 = ep_client2.into_proxy();
    let res = register_vmos(&ep_proxy2, 100, 1, 4096).await;
    assert_eq!(res.err(), Some(Status::INTERNAL));
}

#[fuchsia::test]
fn test_test_mode_try_from() {
    assert_eq!(TestMode::try_from(0), Ok(TestMode::SourceSink));
    assert_eq!(TestMode::try_from(1), Ok(TestMode::Loopback));
    assert_eq!(TestMode::try_from(2), Err(Status::INVALID_ARGS));
    assert_eq!(TestMode::try_from(255), Err(Status::INVALID_ARGS));
}

#[fuchsia::test]
fn test_control_request_parsing_and_types() {
    let type_standard = fusb_descriptor::RequestType::Standard.into_primitive();
    let type_vendor = fusb_descriptor::RequestType::Vendor.into_primitive();
    assert_eq!(
        ControlRequest::parse(
            type_standard,
            fusb_descriptor::StandardRequest::GetStatus.into_primitive()
        ),
        Ok(ControlRequest::Standard(fusb_descriptor::StandardRequest::GetStatus))
    );
    assert_eq!(
        ControlRequest::parse(type_vendor, VendorRequest::SetStall as u8),
        Ok(ControlRequest::Vendor(VendorRequest::SetStall))
    );
    assert_eq!(
        ControlRequest::parse(type_vendor, VendorRequest::ClearStall as u8),
        Ok(ControlRequest::Vendor(VendorRequest::ClearStall))
    );
    assert_eq!(
        ControlRequest::parse(type_vendor, VendorRequest::ConfigureEndpoint as u8),
        Ok(ControlRequest::Vendor(VendorRequest::ConfigureEndpoint))
    );
    assert_eq!(
        ControlRequest::parse(type_vendor, VendorRequest::DisableEndpoint as u8),
        Ok(ControlRequest::Vendor(VendorRequest::DisableEndpoint))
    );
    assert_eq!(
        ControlRequest::parse(type_vendor, VendorRequest::ConnectEndpoint as u8),
        Ok(ControlRequest::Vendor(VendorRequest::ConnectEndpoint))
    );
    assert_eq!(
        ControlRequest::parse(type_vendor, VendorRequest::Deconfigure as u8),
        Ok(ControlRequest::Vendor(VendorRequest::Deconfigure))
    );
    assert_eq!(
        ControlRequest::parse(type_vendor, VendorRequest::WritePayload as u8),
        Ok(ControlRequest::Vendor(VendorRequest::WritePayload))
    );
    assert_eq!(
        ControlRequest::parse(type_vendor, VendorRequest::ReadPayload as u8),
        Ok(ControlRequest::Vendor(VendorRequest::ReadPayload))
    );
    assert_eq!(
        ControlRequest::parse(type_vendor, VendorRequest::SetTestMode as u8),
        Ok(ControlRequest::Vendor(VendorRequest::SetTestMode))
    );
    assert_eq!(
        ControlRequest::parse(type_vendor, VendorRequest::GetTestMode as u8),
        Ok(ControlRequest::Vendor(VendorRequest::GetTestMode))
    );
    assert_eq!(
        ControlRequest::parse(type_vendor, VendorRequest::ControlLoopbackOut as u8),
        Ok(ControlRequest::Vendor(VendorRequest::ControlLoopbackOut))
    );
    assert_eq!(VendorRequest::ControlLoopbackOut as u8, 0x5b);
    assert_eq!(
        ControlRequest::parse(type_vendor, VendorRequest::ControlLoopbackIn as u8),
        Ok(ControlRequest::Vendor(VendorRequest::ControlLoopbackIn))
    );
    assert_eq!(VendorRequest::ControlLoopbackIn as u8, 0x5c);
    assert_eq!(ControlRequest::parse(type_vendor, 0x99), Err(Status::NOT_SUPPORTED));
    assert_eq!(ControlRequest::parse(0x20, 0), Err(Status::NOT_SUPPORTED));
    assert_eq!(ControlRequest::parse(0x60, 0), Err(Status::NOT_SUPPORTED));
}

#[fuchsia::test]
fn test_get_usb_protocol_non_int_and_unknown_properties() {
    assert_eq!(
        get_usb_protocol([
            parent_property(BIND_USB_PROTOCOL_KEY, "not_an_int"),
            parent_property("some.other.key", 42u32),
        ]),
        None
    );
}

#[fuchsia::test]
fn test_max_packet_size_for_speed_all_speeds() {
    assert_eq!(
        UsbZeroFunctionDevice::max_packet_size_for_speed(fusb_descriptor::UsbSpeed::Full),
        USB_MAX_PACKET_SIZE_FULL_SPEED
    );
    assert_eq!(
        UsbZeroFunctionDevice::max_packet_size_for_speed(fusb_descriptor::UsbSpeed::High),
        USB_MAX_PACKET_SIZE_HIGH_SPEED
    );
    assert_eq!(
        UsbZeroFunctionDevice::max_packet_size_for_speed(fusb_descriptor::UsbSpeed::Super),
        USB_MAX_PACKET_SIZE_SUPER_SPEED
    );
    assert_eq!(
        UsbZeroFunctionDevice::max_packet_size_for_speed(fusb_descriptor::UsbSpeed::EnhancedSuper),
        USB_MAX_PACKET_SIZE_SUPER_SPEED
    );
    assert_eq!(
        UsbZeroFunctionDevice::max_packet_size_for_speed(fusb_descriptor::UsbSpeed::Low),
        USB_MAX_PACKET_SIZE_HIGH_SPEED
    );
}

#[fuchsia::test]
fn test_validate_vendor_out_request_errors() {
    let setup_in = fusb_descriptor::UsbSetup {
        bm_request_type: 0xC0,
        b_request: VendorRequest::SetStall as u8,
        w_value: 1,
        w_index: 0,
        w_length: 0,
    };
    assert_eq!(validate_vendor_out_request(&setup_in, &[]), Err(Status::INVALID_ARGS));

    let setup_nonzero_len = fusb_descriptor::UsbSetup {
        bm_request_type: 0x40,
        b_request: VendorRequest::SetStall as u8,
        w_value: 1,
        w_index: 0,
        w_length: 2,
    };
    assert_eq!(validate_vendor_out_request(&setup_nonzero_len, &[]), Err(Status::INVALID_ARGS));

    let setup_out = fusb_descriptor::UsbSetup {
        bm_request_type: 0x40,
        b_request: VendorRequest::SetStall as u8,
        w_value: 1,
        w_index: 0,
        w_length: 0,
    };
    assert_eq!(validate_vendor_out_request(&setup_out, &[0x01]), Err(Status::INVALID_ARGS));

    let setup_large_val = fusb_descriptor::UsbSetup {
        bm_request_type: 0x40,
        b_request: VendorRequest::SetStall as u8,
        w_value: 0x0100,
        w_index: 0,
        w_length: 0,
    };
    assert_eq!(validate_vendor_out_request(&setup_large_val, &[]), Err(Status::INVALID_ARGS));
}

#[fuchsia::test]
async fn test_mapped_vmo_and_completion_helpers() {
    let vmo = zx::Vmo::create(4096).unwrap();
    let mapped_vmos =
        map_vmos(vec![vmo], 4096, zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE).unwrap();
    assert_eq!(mapped_vmos.len(), 1);
    let mapped = &mapped_vmos[0];
    assert_ne!(mapped.as_ptr(), std::ptr::null());
    assert_ne!(mapped.as_mut_ptr(), std::ptr::null_mut());
    assert_eq!(mapped.size(), 4096);

    // Empty list map_vmos
    let empty_mapped = map_vmos(vec![], 4096, zx::VmarFlags::PERM_READ).unwrap();
    assert!(empty_mapped.is_empty());

    // queue_requests_batch with empty list
    let (ep_client, _ep_server) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    assert_eq!(queue_requests_batch(&ep_client.into_proxy(), vec![]).is_ok(), true);

    // completion_vmo_id variations
    let empty_completion = fusb_endpoint::Completion::default();
    assert_eq!(completion_vmo_id(&empty_completion), None);

    let no_data_completion = fusb_endpoint::Completion {
        request: Some(fusb_request::Request::default()),
        ..Default::default()
    };
    assert_eq!(completion_vmo_id(&no_data_completion), None);

    let empty_data_completion = fusb_endpoint::Completion {
        request: Some(fusb_request::Request { data: Some(vec![]), ..Default::default() }),
        ..Default::default()
    };
    assert_eq!(completion_vmo_id(&empty_data_completion), None);

    let vmo_buffer_completion = fusb_endpoint::Completion {
        request: Some(fusb_request::Request {
            data: Some(vec![fusb_request::BufferRegion {
                buffer: Some(fusb_request::Buffer::unknown_variant_for_testing()),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(completion_vmo_id(&vmo_buffer_completion), None);
}

#[fuchsia::test]
async fn test_vendor_and_standard_control_request_edge_cases() {
    let (iface_c, iface_s) = create_endpoints::<fusb_function::UsbFunctionInterfaceMarker>();
    let (func_c, func_s) = create_endpoints::<fusb_function::UsbFunctionMarker>();
    let (ep_in_c, ep_in_s) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let (ep_out_c, ep_out_s) = create_endpoints::<fusb_endpoint::EndpointMarker>();

    let scope = Arc::new(fasync::Scope::new_with_name("test_control_edges"));
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

    let f_p = func_c.into_proxy();
    let ep_i = ep_in_c.into_proxy();
    let ep_o = ep_out_c.into_proxy();
    scope.spawn_local(async move {
        UsbZeroFunctionDevice::new(
            f_p,
            ep_i,
            TEST_EP_IN_ADDR,
            ep_o,
            TEST_EP_OUT_ADDR,
            0,
            TestMode::SourceSink,
        )
        .handle_requests(iface_s.into_stream())
        .await;
    });

    let proxy = iface_c.into_proxy();

    // 1. VendorRequest::SetStall on EP0 should fail with INVALID_ARGS
    let setup_stall_ep0 = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: VendorRequest::SetStall as u8,
        w_value: 0, // EP0
        w_index: 0,
        w_length: 0,
    };
    assert_eq!(
        proxy.control(&setup_stall_ep0, &[]).await.unwrap(),
        Err(Status::INVALID_ARGS.into_raw())
    );

    // 2. VendorRequest::ClearStall on EP0 should succeed (no-op)
    let setup_clear_ep0 = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: VendorRequest::ClearStall as u8,
        w_value: 0, // EP0
        w_index: 0,
        w_length: 0,
    };
    assert_eq!(proxy.control(&setup_clear_ep0, &[]).await.unwrap(), Ok(vec![]));

    // 3. VendorRequest::WritePayload with invalid payloads or directions
    let setup_write_payload_in = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_IN,
        b_request: VendorRequest::WritePayload as u8,
        w_value: 0,
        w_index: 0,
        w_length: USB_ZERO_WRITE_PAYLOAD.len() as u16,
    };
    assert_eq!(
        proxy.control(&setup_write_payload_in, USB_ZERO_WRITE_PAYLOAD).await.unwrap(),
        Err(Status::INVALID_ARGS.into_raw())
    );

    let setup_write_payload_bad = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: VendorRequest::WritePayload as u8,
        w_value: 0,
        w_index: 0,
        w_length: 4,
    };
    assert_eq!(
        proxy.control(&setup_write_payload_bad, &[0, 1, 2, 3]).await.unwrap(),
        Err(Status::INVALID_ARGS.into_raw())
    );

    // 4. VendorRequest::ReadPayload with invalid direction or small length
    let setup_read_payload_out = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: VendorRequest::ReadPayload as u8,
        w_value: 0,
        w_index: 0,
        w_length: 4,
    };
    assert_eq!(
        proxy.control(&setup_read_payload_out, &[]).await.unwrap(),
        Err(Status::INVALID_ARGS.into_raw())
    );

    let setup_read_payload_short = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_IN,
        b_request: VendorRequest::ReadPayload as u8,
        w_value: 0,
        w_index: 0,
        w_length: 2,
    };
    assert_eq!(
        proxy.control(&setup_read_payload_short, &[]).await.unwrap(),
        Err(Status::INVALID_ARGS.into_raw())
    );

    // 5. VendorRequest::GetTestMode invalid cases
    let setup_get_mode_bad_val = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_IN,
        b_request: VendorRequest::GetTestMode as u8,
        w_value: 1, // Must be 0
        w_index: 0,
        w_length: 1,
    };
    assert_eq!(
        proxy.control(&setup_get_mode_bad_val, &[]).await.unwrap(),
        Err(Status::INVALID_ARGS.into_raw())
    );

    let setup_get_mode_bad_len = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_IN,
        b_request: VendorRequest::GetTestMode as u8,
        w_value: 0,
        w_index: 0,
        w_length: 2, // Must be 1
    };
    assert_eq!(
        proxy.control(&setup_get_mode_bad_len, &[]).await.unwrap(),
        Err(Status::INVALID_ARGS.into_raw())
    );

    // 6. VendorRequest::ControlLoopback partial/truncation read
    let cl_payload = vec![10, 20, 30, 40, 50, 60];
    let setup_cl_out = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: VendorRequest::ControlLoopbackOut as u8,
        w_value: 0,
        w_index: 0,
        w_length: cl_payload.len() as u16,
    };
    assert_eq!(proxy.control(&setup_cl_out, &cl_payload).await.unwrap(), Ok(vec![]));

    let setup_cl_in_trunc = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_IN,
        b_request: VendorRequest::ControlLoopbackIn as u8,
        w_value: 0,
        w_index: 0,
        w_length: 3, // Only request first 3 bytes
    };
    assert_eq!(proxy.control(&setup_cl_in_trunc, &[]).await.unwrap(), Ok(vec![10, 20, 30]));

    // 7. VendorRequest::Deconfigure invalid args
    let setup_deconfig_in = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_IN,
        b_request: VendorRequest::Deconfigure as u8,
        w_value: 0,
        w_index: 0,
        w_length: 0,
    };
    assert_eq!(
        proxy.control(&setup_deconfig_in, &[]).await.unwrap(),
        Err(Status::INVALID_ARGS.into_raw())
    );

    let setup_deconfig_bad_len = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: VendorRequest::Deconfigure as u8,
        w_value: 0,
        w_index: 0,
        w_length: 4,
    };
    assert_eq!(
        proxy.control(&setup_deconfig_bad_len, &[]).await.unwrap(),
        Err(Status::INVALID_ARGS.into_raw())
    );

    // 8. Standard GetStatus invalid conditions
    // Out request instead of In
    let setup_status_out = fusb_descriptor::UsbSetup {
        bm_request_type: 0x00,
        b_request: fusb_descriptor::StandardRequest::GetStatus.into_primitive(),
        w_value: 0,
        w_index: 0,
        w_length: 2,
    };
    assert_eq!(
        proxy.control(&setup_status_out, &[]).await.unwrap(),
        Err(Status::NOT_SUPPORTED.into_raw())
    );

    // Length != 2
    let setup_status_len = fusb_descriptor::UsbSetup {
        bm_request_type: 0x80,
        b_request: fusb_descriptor::StandardRequest::GetStatus.into_primitive(),
        w_value: 0,
        w_index: 0,
        w_length: 1,
    };
    assert_eq!(
        proxy.control(&setup_status_len, &[]).await.unwrap(),
        Err(Status::NOT_SUPPORTED.into_raw())
    );

    // Unknown endpoint status
    let setup_status_bad_ep = fusb_descriptor::UsbSetup {
        bm_request_type: 0x82, // IN | ENDPOINT
        b_request: fusb_descriptor::StandardRequest::GetStatus.into_primitive(),
        w_value: 0,
        w_index: 0x05, // Unknown endpoint
        w_length: 2,
    };
    assert_eq!(
        proxy.control(&setup_status_bad_ep, &[]).await.unwrap(),
        Err(Status::NOT_SUPPORTED.into_raw())
    );

    // 9. Standard GetInterface invalid conditions
    let setup_get_iface_bad_idx = fusb_descriptor::UsbSetup {
        bm_request_type: 0x81,
        b_request: fusb_descriptor::StandardRequest::GetInterface.into_primitive(),
        w_value: 0,
        w_index: 99, // Wrong interface number
        w_length: 1,
    };
    assert_eq!(
        proxy.control(&setup_get_iface_bad_idx, &[]).await.unwrap(),
        Err(Status::NOT_SUPPORTED.into_raw())
    );

    // 10. Standard SetInterface invalid conditions
    let setup_set_iface_bad_idx = fusb_descriptor::UsbSetup {
        bm_request_type: 0x01,
        b_request: fusb_descriptor::StandardRequest::SetInterface.into_primitive(),
        w_value: 0,
        w_index: 99, // Wrong interface number
        w_length: 0,
    };
    assert_eq!(
        proxy.control(&setup_set_iface_bad_idx, &[]).await.unwrap(),
        Err(Status::NOT_SUPPORTED.into_raw())
    );

    // 11. Unhandled standard request (e.g. 0x09 SetConfiguration)
    let setup_set_config = fusb_descriptor::UsbSetup {
        bm_request_type: 0x00,
        b_request: 0x09,
        w_value: 1,
        w_index: 0,
        w_length: 0,
    };
    assert_eq!(
        proxy.control(&setup_set_config, &[]).await.unwrap(),
        Err(Status::NOT_SUPPORTED.into_raw())
    );
}

#[fuchsia::test]
async fn test_set_configured_superspeed() {
    let (iface_c, iface_s) = create_endpoints::<fusb_function::UsbFunctionInterfaceMarker>();
    let (func_c, func_s) = create_endpoints::<fusb_function::UsbFunctionMarker>();
    let (ep_in_c, ep_in_s) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let (ep_out_c, ep_out_s) = create_endpoints::<fusb_endpoint::EndpointMarker>();

    let scope = Arc::new(fasync::Scope::new_with_name("test_superspeed"));
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

    let f_p = func_c.into_proxy();
    let ep_i = ep_in_c.into_proxy();
    let ep_o = ep_out_c.into_proxy();
    scope.spawn_local(async move {
        UsbZeroFunctionDevice::new(
            f_p,
            ep_i,
            TEST_EP_IN_ADDR,
            ep_o,
            TEST_EP_OUT_ADDR,
            0,
            TestMode::SourceSink,
        )
        .handle_requests(iface_s.into_stream())
        .await;
    });

    let proxy = iface_c.into_proxy();

    // Test configuring with SuperSpeed (exercises SuperSpeed companion descriptor creation)
    assert_eq!(proxy.set_configured(true, fusb_descriptor::UsbSpeed::Super).await.unwrap(), Ok(()));

    // Deconfigure
    assert_eq!(
        proxy.set_configured(false, fusb_descriptor::UsbSpeed::Super).await.unwrap(),
        Ok(())
    );

    // Test configuring with EnhancedSuper
    assert_eq!(
        proxy.set_configured(true, fusb_descriptor::UsbSpeed::EnhancedSuper).await.unwrap(),
        Ok(())
    );

    // Deconfigure
    assert_eq!(proxy.set_configured(false, fusb_descriptor::UsbSpeed::Full).await.unwrap(), Ok(()));
}

#[fuchsia::test]
async fn test_loopback_scatter_gather() {
    let (iface_c, iface_s) = create_endpoints::<fusb_function::UsbFunctionInterfaceMarker>();
    let (func_c, func_s) = create_endpoints::<fusb_function::UsbFunctionMarker>();
    let (ep_in_c, ep_in_s) = create_endpoints::<fusb_endpoint::EndpointMarker>();
    let (ep_out_c, ep_out_s) = create_endpoints::<fusb_endpoint::EndpointMarker>();

    let state_in =
        Arc::new(Mutex::new(MockEndpointState { requests: vec![], vmos: HashMap::new() }));
    let state_out =
        Arc::new(Mutex::new(MockEndpointState { requests: vec![], vmos: HashMap::new() }));

    let (_comp_in_tx, comp_in_rx) = mpsc::unbounded();
    let (comp_out_tx, comp_out_rx) = mpsc::unbounded();
    let (event_in_tx, mut event_in_rx) = mpsc::unbounded();
    let (event_out_tx, mut event_out_rx) = mpsc::unbounded();

    let scope = Arc::new(fasync::Scope::new_with_name("test_loopback_sg"));
    scope.spawn_local(run_mock_function(func_s.into_stream()));
    scope.spawn_local(run_mock_endpoint(
        ep_in_s.into_stream(),
        state_in.clone(),
        comp_in_rx,
        event_in_tx,
        scope.clone(),
    ));
    scope.spawn_local(run_mock_endpoint(
        ep_out_s.into_stream(),
        state_out.clone(),
        comp_out_rx,
        event_out_tx,
        scope.clone(),
    ));

    let (f_p, ep_i, ep_o) = (func_c.into_proxy(), ep_in_c.into_proxy(), ep_out_c.into_proxy());
    scope.spawn_local(async move {
        UsbZeroFunctionDevice::new(
            f_p,
            ep_i,
            TEST_EP_IN_ADDR,
            ep_o,
            TEST_EP_OUT_ADDR,
            0,
            TestMode::Loopback,
        )
        .handle_requests(iface_s.into_stream())
        .await;
    });

    let proxy = iface_c.into_proxy();
    assert_eq!(proxy.set_configured(true, fusb_descriptor::UsbSpeed::High).await.unwrap(), Ok(()));

    // Drain initial registration and queue events; keep initial OUT requests in state_out
    // so ep_out.cancel_all() cancels them and triggers re-queuing with the new SG layout.
    assert_eq!(event_out_rx.next().await, Some(MockEvent::VmoRegistered));
    assert_eq!(event_out_rx.next().await, Some(MockEvent::RequestQueued));
    assert_eq!(event_in_rx.next().await, Some(MockEvent::VmoRegistered));

    // Send VendorRequest::ConfigureScatterGather (0x5d) with w_value = 1 (in_short_zlp = true),
    // 2 OUT regions: (offset: 4096, size: 300), (offset: 12288, size: 724)
    // 3 IN regions: (offset: 8192, size: 256), (offset: 16384, size: 256), (offset: 24576, size: 512)
    let mut payload = Vec::new();
    payload.extend_from_slice(&2u16.to_le_bytes()); // num_out = 2
    payload.extend_from_slice(&3u16.to_le_bytes()); // num_in = 3
    for (offset, size) in [
        (4096u32, 300u32),
        (12288u32, 724u32),
        (8192u32, 256u32),
        (16384u32, 256u32),
        (24576u32, 512u32),
    ] {
        payload.extend_from_slice(&offset.to_le_bytes());
        payload.extend_from_slice(&size.to_le_bytes());
    }

    let setup_sg = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: VendorRequest::ConfigureScatterGather as u8,
        w_value: 1, // in_short_zlp = true
        w_index: 0,
        w_length: payload.len() as u16,
    };
    assert_eq!(proxy.control(&setup_sg, &payload).await.unwrap(), Ok(vec![]));

    // Wait for new SG OUT requests to be queued across both cancel_all() completion batches.
    assert_eq!(event_out_rx.next().await, Some(MockEvent::RequestQueued));
    assert_eq!(event_out_rx.next().await, Some(MockEvent::RequestQueued));

    let (read_req, vmo_out) = {
        let mut state = state_out.lock().unwrap();
        assert_eq!(state.requests.len(), QUEUE_DEPTH);
        let req = state.requests.remove(0);
        let regions = req.data.as_ref().unwrap();
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].offset, Some(4096));
        assert_eq!(regions[0].size, Some(300));
        assert_eq!(regions[1].offset, Some(12288));
        assert_eq!(regions[1].size, Some(724));
        let vmo = state
            .vmos
            .get(&USB_ZERO_OUT_VMO_ID)
            .unwrap()
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
            .unwrap();
        (req, vmo)
    };

    let vmo_in = {
        let state = state_in.lock().unwrap();
        state
            .vmos
            .get(&USB_ZERO_IN_VMO_ID)
            .unwrap()
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
            .unwrap()
    };

    // Populate 600 bytes across out_regions[0] (300 bytes at 4096) and out_regions[1] (300 bytes at 12288).
    let expected_payload: Vec<u8> = (0..600).map(|i| ((i * 7 + 3) & 0xff) as u8).collect();
    vmo_out.write(&expected_payload[0..300], 4096).unwrap();
    vmo_out.write(&expected_payload[300..600], 12288).unwrap();

    comp_out_tx
        .unbounded_send(vec![fusb_endpoint::Completion {
            request: Some(read_req),
            status: Some(zx::sys::ZX_OK),
            transfer_size: Some(600),
            ..Default::default()
        }])
        .unwrap();

    assert_eq!(event_in_rx.next().await, Some(MockEvent::RequestQueued));

    let write_req = {
        let mut state = state_in.lock().unwrap();
        state.requests.pop().unwrap()
    };

    assert_eq!(write_req.short, Some(true));
    let in_regions = write_req.data.as_ref().unwrap();
    assert_eq!(in_regions.len(), 3);
    assert_eq!(in_regions[0].offset, Some(8192));
    assert_eq!(in_regions[0].size, Some(256));
    assert_eq!(in_regions[1].offset, Some(16384));
    assert_eq!(in_regions[1].size, Some(256));
    assert_eq!(in_regions[2].offset, Some(24576));
    assert_eq!(in_regions[2].size, Some(88));

    let mut actual_payload = Vec::with_capacity(600);
    let mut part0 = vec![0u8; 256];
    let mut part1 = vec![0u8; 256];
    let mut part2 = vec![0u8; 88];
    vmo_in.read(&mut part0, 8192).unwrap();
    vmo_in.read(&mut part1, 16384).unwrap();
    vmo_in.read(&mut part2, 24576).unwrap();
    actual_payload.extend_from_slice(&part0);
    actual_payload.extend_from_slice(&part1);
    actual_payload.extend_from_slice(&part2);
    assert_eq!(actual_payload, expected_payload);

    // Wait for read_task to re-queue the completed OUT request.
    assert_eq!(event_out_rx.next().await, Some(MockEvent::RequestQueued));

    // Verify ZLP in SG mode queues an empty BufferRegion list rather than a size-0 region.
    let zlp_read_req = {
        let mut state = state_out.lock().unwrap();
        state.requests.remove(0)
    };
    comp_out_tx
        .unbounded_send(vec![fusb_endpoint::Completion {
            request: Some(zlp_read_req),
            status: Some(zx::sys::ZX_OK),
            transfer_size: Some(0),
            ..Default::default()
        }])
        .unwrap();
    assert_eq!(event_in_rx.next().await, Some(MockEvent::RequestQueued));
    assert_eq!(event_out_rx.next().await, Some(MockEvent::RequestQueued));
    {
        let mut state = state_in.lock().unwrap();
        let zlp_req = state.requests.pop().unwrap();
        assert_eq!(zlp_req.data.as_deref(), Some(&[][..]));
    }

    // Verify overlapping regions within the same request are rejected with INVALID_ARGS.
    let mut bad_payload = Vec::new();
    bad_payload.extend_from_slice(&2u16.to_le_bytes()); // num_out = 2
    bad_payload.extend_from_slice(&0u16.to_le_bytes()); // num_in = 0
    for (offset, size) in [(4096u32, 512u32), (4352u32, 512u32)] {
        bad_payload.extend_from_slice(&offset.to_le_bytes());
        bad_payload.extend_from_slice(&size.to_le_bytes());
    }
    let setup_bad_sg = fusb_descriptor::UsbSetup {
        bm_request_type: USB_TYPE_VENDOR_OUT,
        b_request: VendorRequest::ConfigureScatterGather as u8,
        w_value: 0,
        w_index: 0,
        w_length: bad_payload.len() as u16,
    };
    assert_eq!(
        proxy.control(&setup_bad_sg, &bad_payload).await.unwrap(),
        Err(Status::INVALID_ARGS.into_raw())
    );
}
