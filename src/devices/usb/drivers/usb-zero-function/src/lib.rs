// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdf_component::{Driver, DriverContext, DriverError, Node, driver_register};
use fidl_fuchsia_hardware_usb_descriptor as fusb_descriptor;
use fidl_fuchsia_hardware_usb_endpoint as fusb_endpoint;
use fidl_fuchsia_hardware_usb_function as fusb_function;
use fidl_fuchsia_hardware_usb_request as fusb_request;
use fuchsia_async as fasync;
use futures::channel::mpsc;
use futures::{StreamExt, TryStreamExt};
use log::{error, info, warn};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use zx::Status;

// USB Standard Constants
const USB_DESC_TYPE_INTERFACE: u8 = 0x04;
const USB_DESC_TYPE_ENDPOINT: u8 = 0x05;
const USB_CLASS_VENDOR: u8 = 0xff;

// USB Setup Request Types
const USB_TYPE_MASK: u8 = 0x60;
const USB_TYPE_STANDARD: u8 = 0x00;
const USB_TYPE_VENDOR: u8 = 0x40;

const USB_INTERFACE_DESC_SIZE: u8 = 9;
const USB_ENDPOINT_DESC_SIZE: u8 = 7;

const DEFAULT_VMO_SIZE: u64 = 4096;

const USB_SETUP_REQ_GET_STATUS: u8 = 0x00;
const USB_SETUP_REQ_CLEAR_FEATURE: u8 = 0x01;
const USB_SETUP_REQ_SET_FEATURE: u8 = 0x03;
const USB_SETUP_REQ_GET_INTERFACE: u8 = 0x0a;

const USB_MAX_PACKET_SIZE_FULL_SPEED: u16 = 64;
const USB_MAX_PACKET_SIZE_HIGH_SPEED: u16 = 512;
const USB_MAX_PACKET_SIZE_SUPER_SPEED: u16 = 1024;

// USB Zero Function Specific Constants
const USB_ZERO_NUM_INTERFACES: u8 = 1;
const USB_ZERO_NUM_ENDPOINTS: u8 = 2;
const USB_ZERO_DEFAULT_MAX_PACKET_SIZE: u16 = USB_MAX_PACKET_SIZE_HIGH_SPEED;

const USB_ZERO_OUT_VMO_ID: u64 = 1;
const USB_ZERO_IN_VMO_ID: u64 = 2;

const USB_ZERO_WRITE_PAYLOAD: &[u8] = &[0xDE, 0xAD, 0xBE, 0xEF];
const USB_ZERO_READ_PAYLOAD: &[u8] = &[0x12, 0x34, 0x56, 0x78];

#[derive(Copy, Clone, Debug, PartialEq, Eq, enumn::N)]
#[repr(u8)]
enum VendorRequest {
    SetStall = 0x50,
    ClearStall = 0x51,
    ConfigureEndpoint = 0x52,
    DisableEndpoint = 0x53,
    ConnectEndpoint = 0x54,
    Deconfigure = 0x55,
    WritePayload = 0x56,
    ReadPayload = 0x57,
    SetTestMode = 0x58,
    ControlLoopbackOut = 0x5b,
    ControlLoopbackIn = 0x5c,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ControlRequest {
    Vendor(VendorRequest),
    Standard(u8),
    Unsupported,
}

impl ControlRequest {
    fn parse(bm_request_type: u8, b_request: u8) -> Self {
        match bm_request_type & USB_TYPE_MASK {
            USB_TYPE_STANDARD => ControlRequest::Standard(b_request),
            USB_TYPE_VENDOR => VendorRequest::n(b_request)
                .map_or(ControlRequest::Unsupported, ControlRequest::Vendor),
            _ => ControlRequest::Unsupported,
        }
    }
}

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq, enumn::N)]
#[repr(u8)]
pub enum TestMode {
    #[default]
    SourceSink = 0,
    Loopback = 1,
}

impl TryFrom<u8> for TestMode {
    type Error = Status;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        TestMode::n(value).ok_or(Status::INVALID_ARGS)
    }
}
struct UsbZeroFunction {
    // Stored to keep the driver node handle alive per Fuchsia component driver lifecycle rules.
    _node: Node,
    // Stored to keep the driver execution scope and spawned background tasks alive per Fuchsia component driver lifecycle rules.
    _scope: Arc<fasync::Scope>,
}

struct UsbZeroFunctionDevice {
    function_client: fusb_function::UsbFunctionProxy,
    ep_in: fusb_endpoint::EndpointProxy,
    ep_in_addr: u8,
    ep_out: fusb_endpoint::EndpointProxy,
    ep_out_addr: u8,
    interface_num: u8,
    is_configured: Arc<AtomicBool>,
    vmos_registered: bool,
    endpoint_tasks: Option<(fasync::Task<()>, fasync::Task<()>)>,
    mode: TestMode,
    speed: Option<fusb_descriptor::UsbSpeed>,
}

driver_register!(UsbZeroFunction);

impl Driver for UsbZeroFunction {
    const NAME: &str = "usb-zero-function";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        let node = context.take_node()?;
        let scope = Arc::new(fasync::Scope::new_with_name("driver"));

        info!("Starting usb-zero-function");

        let function_client = context
            .incoming
            .service_marker(fusb_function::UsbFunctionServiceMarker)
            .connect()?
            .connect_to_device()
            .map_err(|e| {
                warn!("FIDL error: {:?}", e);
                Status::INTERNAL
            })?;

        // Allocate resources
        let (ep_in_client, ep_in_server) =
            fidl::endpoints::create_endpoints::<fusb_endpoint::EndpointMarker>();
        let (ep_out_client, ep_out_server) =
            fidl::endpoints::create_endpoints::<fusb_endpoint::EndpointMarker>();

        let endpoints = vec![
            fusb_function::EndpointResource {
                direction: fusb_descriptor::EndpointDirection::In,
                endpoint: ep_in_server,
                ep_info: fusb_endpoint::EndpointInfo::Bulk(
                    fusb_endpoint::BulkEndpointInfo::default(),
                ),
                max_packet_size: USB_ZERO_DEFAULT_MAX_PACKET_SIZE.into(),
            },
            fusb_function::EndpointResource {
                direction: fusb_descriptor::EndpointDirection::Out,
                endpoint: ep_out_server,
                ep_info: fusb_endpoint::EndpointInfo::Bulk(
                    fusb_endpoint::BulkEndpointInfo::default(),
                ),
                max_packet_size: USB_ZERO_DEFAULT_MAX_PACKET_SIZE.into(),
            },
        ];

        let alloc_result = function_client
            .alloc_resources(USB_ZERO_NUM_INTERFACES, endpoints, &[])
            .await
            .map_err(|e| {
                warn!("FIDL error: {:?}", e);
                Status::INTERNAL
            })?
            .map_err(Status::err_from_raw)?;

        let (interfaces, endpoints, _) = alloc_result;
        if interfaces.len() != 1 || endpoints.len() != 2 {
            error!("Invalid resource lengths from AllocResources");
            return Err(Status::NO_RESOURCES.into());
        }
        let interface_num = interfaces[0];
        let ep_in_addr = endpoints[0];
        let ep_out_addr = endpoints[1];

        if (ep_in_addr & 0x80) == 0 || (ep_out_addr & 0x80) != 0 {
            error!("Invalid endpoint direction bits assigned");
            return Err(Status::NO_RESOURCES.into());
        }

        // Construct descriptors
        let default_max_packet_size_bytes = USB_ZERO_DEFAULT_MAX_PACKET_SIZE.to_le_bytes();
        let mut desc = vec![
            // Interface Descriptor
            USB_INTERFACE_DESC_SIZE, // bLength
            USB_DESC_TYPE_INTERFACE, // bDescriptorType (Interface)
            interface_num,           // bInterfaceNumber
            0x00,                    // bAlternateSetting
            USB_ZERO_NUM_ENDPOINTS,  // bNumEndpoints
            USB_CLASS_VENDOR,        // bInterfaceClass (Vendor Specific)
            0,                       // bInterfaceSubClass
            0,                       // bInterfaceProtocol
            0,                       // iInterface
            // Endpoint Descriptor (IN)
            USB_ENDPOINT_DESC_SIZE,                               // bLength
            USB_DESC_TYPE_ENDPOINT,                               // bDescriptorType (Endpoint)
            ep_in_addr,                                           // bEndpointAddress
            fusb_descriptor::EndpointType::Bulk.into_primitive(), // bmAttributes (Bulk)
            default_max_packet_size_bytes[0],
            default_max_packet_size_bytes[1], // wMaxPacketSize (little endian)
            0,                                // bInterval
            // Endpoint Descriptor (OUT)
            USB_ENDPOINT_DESC_SIZE,                               // bLength
            USB_DESC_TYPE_ENDPOINT,                               // bDescriptorType (Endpoint)
            ep_out_addr,                                          // bEndpointAddress
            fusb_descriptor::EndpointType::Bulk.into_primitive(), // bmAttributes (Bulk)
            default_max_packet_size_bytes[0],
            default_max_packet_size_bytes[1], // wMaxPacketSize (little endian)
            0,                                // bInterval
        ];
        // Alternate Setting 1 (Loopback) Interface and Endpoint Descriptors
        let ep_desc_start = USB_INTERFACE_DESC_SIZE as usize;
        let ep_desc_end = desc.len();
        desc.extend_from_slice(&[
            USB_INTERFACE_DESC_SIZE,
            USB_DESC_TYPE_INTERFACE,
            interface_num,
            0x01,
            USB_ZERO_NUM_ENDPOINTS,
            USB_CLASS_VENDOR,
            0,
            0,
            0,
        ]);
        desc.extend_from_within(ep_desc_start..ep_desc_end);

        let (iface_client, iface_server) =
            fidl::endpoints::create_endpoints::<fusb_function::UsbFunctionInterfaceMarker>();

        function_client
            .configure(&desc, iface_client)
            .await
            .map_err(|e| {
                warn!("FIDL error: {:?}", e);
                Status::INTERNAL
            })?
            .map_err(Status::err_from_raw)?;

        let ep_in = ep_in_client.into_proxy();
        let ep_out = ep_out_client.into_proxy();

        let function_client_clone = function_client.clone();
        let ep_in_clone = ep_in.clone();
        let ep_out_clone = ep_out.clone();
        scope.spawn_local(async move {
            let mut device = UsbZeroFunctionDevice::new(
                function_client_clone,
                ep_in_clone,
                ep_in_addr,
                ep_out_clone,
                ep_out_addr,
                interface_num,
            );
            device.handle_requests(iface_server.into_stream()).await;
        });

        Ok(UsbZeroFunction { _node: node, _scope: scope })
    }

    async fn stop(&self) {}
}

fn validate_vendor_out_request(
    setup: &fusb_descriptor::UsbSetup,
    write: &[u8],
) -> Result<u8, Status> {
    if (setup.bm_request_type & 0x80) != 0 || setup.w_length != 0 || !write.is_empty() {
        return Err(Status::INVALID_ARGS);
    }
    u8::try_from(setup.w_value).map_err(|_| Status::INVALID_ARGS)
}

async fn configure_ep(
    function_client: &fusb_function::UsbFunctionProxy,
    ep_addr: u8,
    ep_config: &fusb_function::EndpointConfiguration,
) -> Result<(), Status> {
    function_client
        .configure_endpoint(ep_addr, ep_config)
        .await
        .map_err(|e| {
            warn!("FIDL error: {:?}", e);
            Status::INTERNAL
        })?
        .map_err(Status::err_from_raw)
}

impl UsbZeroFunctionDevice {
    fn new(
        function_client: fusb_function::UsbFunctionProxy,
        ep_in: fusb_endpoint::EndpointProxy,
        ep_in_addr: u8,
        ep_out: fusb_endpoint::EndpointProxy,
        ep_out_addr: u8,
        interface_num: u8,
    ) -> Self {
        Self {
            function_client,
            ep_in,
            ep_in_addr,
            ep_out,
            ep_out_addr,
            interface_num,
            is_configured: Arc::new(AtomicBool::new(false)),
            vmos_registered: false,
            endpoint_tasks: None,
            mode: TestMode::default(),
            speed: None,
        }
    }

    async fn cleanup_endpoints(&mut self) {
        self.endpoint_tasks = None;
        if self.vmos_registered {
            let _ = self.ep_in.unregister_vmos(&[USB_ZERO_IN_VMO_ID]).await;
            let _ = self.ep_out.unregister_vmos(&[USB_ZERO_OUT_VMO_ID]).await;
            self.vmos_registered = false;
        }
        self.is_configured.store(false, Ordering::Relaxed);
        let _ = self.function_client.disable_endpoint(self.ep_in_addr).await;
        let _ = self.function_client.disable_endpoint(self.ep_out_addr).await;
    }

    fn max_packet_size_for_speed(speed: fusb_descriptor::UsbSpeed) -> u16 {
        match speed {
            fusb_descriptor::UsbSpeed::Full => USB_MAX_PACKET_SIZE_FULL_SPEED,
            fusb_descriptor::UsbSpeed::High => USB_MAX_PACKET_SIZE_HIGH_SPEED,
            fusb_descriptor::UsbSpeed::Super | fusb_descriptor::UsbSpeed::EnhancedSuper => {
                USB_MAX_PACKET_SIZE_SUPER_SPEED
            }
            _ => USB_MAX_PACKET_SIZE_HIGH_SPEED,
        }
    }

    /// Configures and activates the IN and OUT bulk endpoints.
    async fn activate_endpoints(&self, speed: fusb_descriptor::UsbSpeed) -> Result<(), Status> {
        let w_max_packet_size = Self::max_packet_size_for_speed(speed);
        let super_speed_companion = match speed {
            fusb_descriptor::UsbSpeed::Super | fusb_descriptor::UsbSpeed::EnhancedSuper => {
                Some(fusb_function::SuperSpeedEndpointCompanionDescriptor {
                    b_max_burst: 0,
                    bm_attributes: 0,
                    w_bytes_per_interval: 0,
                })
            }
            _ => None,
        };
        let ep_config = fusb_function::EndpointConfiguration {
            descriptor: Some(fusb_function::EndpointDescriptor {
                bm_attributes: fusb_descriptor::EndpointType::Bulk.into_primitive(),
                w_max_packet_size,
                b_interval: 0,
            }),
            super_speed_companion,
            ..Default::default()
        };
        configure_ep(&self.function_client, self.ep_in_addr, &ep_config).await?;
        if let Err(e) = configure_ep(&self.function_client, self.ep_out_addr, &ep_config).await {
            let _ = self.function_client.disable_endpoint(self.ep_in_addr).await;
            return Err(e);
        }
        Ok(())
    }

    async fn handle_set_configured(
        &mut self,
        configured: bool,
        speed: fusb_descriptor::UsbSpeed,
    ) -> Result<Option<(fasync::Task<()>, fasync::Task<()>)>, Status> {
        self.cleanup_endpoints().await;
        self.speed = if configured { Some(speed) } else { None };
        if configured {
            self.activate_endpoints(speed).await?;
            let transfer_size = Self::max_packet_size_for_speed(speed).into();
            let res = match self.mode {
                TestMode::SourceSink => {
                    run_source_sink(
                        self.ep_in.clone(),
                        self.ep_out.clone(),
                        &mut self.vmos_registered,
                        transfer_size,
                    )
                    .await
                }
                TestMode::Loopback => {
                    run_loopback(self.ep_in.clone(), self.ep_out.clone(), &mut self.vmos_registered)
                        .await
                }
            };
            match res {
                Ok((r_task, w_task)) => Ok(Some((r_task, w_task))),
                Err(e) => {
                    self.cleanup_endpoints().await;
                    Err(e)
                }
            }
        } else {
            Ok(None)
        }
    }
    async fn handle_vendor_request(
        &mut self,
        vendor_req: VendorRequest,
        setup: &fusb_descriptor::UsbSetup,
        write: &[u8],
    ) -> Result<Vec<u8>, Status> {
        // Vendor control requests must have Vendor type (0x40 in bm_request_type)
        if (setup.bm_request_type & 0x60) != 0x40 {
            return Err(Status::INVALID_ARGS);
        }
        match vendor_req {
            VendorRequest::SetStall => {
                let ep_addr = validate_vendor_out_request(setup, write)?;
                self.function_client
                    .endpoint_set_stall(ep_addr)
                    .await
                    .map_err(|e| {
                        warn!("FIDL error setting stall: {:?}", e);
                        Status::INTERNAL
                    })?
                    .map_err(Status::err_from_raw)?;
                Ok(Vec::new())
            }
            VendorRequest::ClearStall => {
                let ep_addr = validate_vendor_out_request(setup, write)?;
                self.function_client
                    .endpoint_clear_stall(ep_addr)
                    .await
                    .map_err(|e| {
                        warn!("FIDL error clearing stall: {:?}", e);
                        Status::INTERNAL
                    })?
                    .map_err(Status::err_from_raw)?;
                Ok(Vec::new())
            }
            VendorRequest::ConfigureEndpoint => {
                let ep_addr = validate_vendor_out_request(setup, write)?;
                let ep_config = fusb_function::EndpointConfiguration {
                    descriptor: Some(fusb_function::EndpointDescriptor {
                        bm_attributes: fusb_descriptor::EndpointType::Bulk.into_primitive(),
                        w_max_packet_size: USB_MAX_PACKET_SIZE_HIGH_SPEED,
                        b_interval: 0,
                    }),
                    ..Default::default()
                };
                configure_ep(&self.function_client, ep_addr, &ep_config).await?;
                Ok(Vec::new())
            }
            VendorRequest::DisableEndpoint => {
                let ep_addr = validate_vendor_out_request(setup, write)?;
                self.function_client
                    .disable_endpoint(ep_addr)
                    .await
                    .map_err(|e| {
                        warn!("FIDL error disabling endpoint: {:?}", e);
                        Status::INTERNAL
                    })?
                    .map_err(Status::err_from_raw)?;
                Ok(Vec::new())
            }
            VendorRequest::ConnectEndpoint => {
                let ep_addr = validate_vendor_out_request(setup, write)?;
                // Create placeholder endpoint pair purely to test that core accepts connect_to_endpoint FIDL call.
                let (_ep_client, ep_server) =
                    fidl::endpoints::create_endpoints::<fusb_endpoint::EndpointMarker>();
                self.function_client
                    .connect_to_endpoint(ep_addr, ep_server)
                    .await
                    .map_err(|e| {
                        warn!("FIDL error connecting to endpoint: {:?}", e);
                        Status::INTERNAL
                    })?
                    .map_err(Status::err_from_raw)?;
                Ok(Vec::new())
            }
            VendorRequest::Deconfigure => {
                if (setup.bm_request_type & 0x80) != 0 || setup.w_length != 0 || !write.is_empty() {
                    return Err(Status::INVALID_ARGS);
                }
                self.endpoint_tasks = None;
                self.cleanup_endpoints().await;
                self.is_configured.store(false, Ordering::Relaxed);
                self.function_client
                    .deconfigure()
                    .await
                    .map_err(|e| {
                        warn!("FIDL error deconfiguring: {:?}", e);
                        Status::INTERNAL
                    })?
                    .map_err(Status::err_from_raw)?;
                Ok(Vec::new())
            }
            VendorRequest::WritePayload => {
                if (setup.bm_request_type & 0x80) != 0
                    || setup.w_length as usize != write.len()
                    || write != USB_ZERO_WRITE_PAYLOAD
                {
                    return Err(Status::INVALID_ARGS);
                }
                Ok(Vec::new())
            }
            VendorRequest::ReadPayload => {
                if (setup.bm_request_type & 0x80) == 0
                    || (setup.w_length as usize) < USB_ZERO_READ_PAYLOAD.len()
                    || !write.is_empty()
                {
                    return Err(Status::INVALID_ARGS);
                }
                Ok(USB_ZERO_READ_PAYLOAD.to_vec())
            }
            VendorRequest::SetTestMode => {
                let mode_val = validate_vendor_out_request(setup, write)?;
                self.mode = mode_val.try_into()?;
                Ok(Vec::new())
            }
            VendorRequest::ControlLoopbackOut | VendorRequest::ControlLoopbackIn => {
                Err(Status::NOT_SUPPORTED)
            }
        }
    }

    async fn handle_control_request(
        &mut self,
        setup: &fusb_descriptor::UsbSetup,
        write: &[u8],
    ) -> Result<Vec<u8>, Status> {
        match ControlRequest::parse(setup.bm_request_type, setup.b_request) {
            ControlRequest::Vendor(vendor_req) => {
                self.handle_vendor_request(vendor_req, setup, write).await
            }
            ControlRequest::Standard(req) => match req {
                USB_SETUP_REQ_GET_STATUS => Ok(vec![0x00, 0x00]),
                USB_SETUP_REQ_CLEAR_FEATURE => Ok(Vec::new()),
                USB_SETUP_REQ_SET_FEATURE => Ok(Vec::new()),
                USB_SETUP_REQ_GET_INTERFACE => Ok(vec![match self.mode {
                    TestMode::SourceSink => 0,
                    TestMode::Loopback => 1,
                }]),
                _ => Err(Status::NOT_SUPPORTED),
            },
            ControlRequest::Unsupported => Err(Status::NOT_SUPPORTED),
        }
    }
    /// Processes incoming FIDL requests on the `UsbFunctionInterface` request stream,
    /// handling control transfers and configuration changes for the USB device.
    async fn handle_requests(
        &mut self,
        mut stream: fusb_function::UsbFunctionInterfaceRequestStream,
    ) {
        while let Ok(Some(request)) = stream.try_next().await {
            match request {
                fusb_function::UsbFunctionInterfaceRequest::Control { setup, write, responder } => {
                    info!("Received control request: {:?}", setup);
                    let status = self.handle_control_request(&setup, &write).await;
                    let response = status.as_deref().map_err(|s| s.into_raw());
                    let _ = responder.send(response);
                }
                fusb_function::UsbFunctionInterfaceRequest::SetConfigured {
                    configured,
                    speed,
                    responder,
                } => {
                    info!("Set configured: {}", configured);
                    self.endpoint_tasks = None;

                    let status = self.handle_set_configured(configured, speed).await;

                    match status {
                        Ok(tasks) => {
                            self.endpoint_tasks = tasks;
                            self.is_configured.store(configured, Ordering::Relaxed);
                            let _ = responder.send(Ok(()));
                        }
                        Err(e) => {
                            self.is_configured.store(false, Ordering::Relaxed);
                            let _ = responder.send(Err(e.into_raw()));
                        }
                    }
                }
                fusb_function::UsbFunctionInterfaceRequest::SetInterface {
                    interface,
                    alt_setting,
                    responder,
                } => {
                    let response = if interface == self.interface_num && alt_setting <= 1 {
                        let new_mode = if alt_setting == 0 {
                            TestMode::SourceSink
                        } else {
                            TestMode::Loopback
                        };
                        if self.is_configured.load(Ordering::Relaxed) {
                            self.endpoint_tasks = None;
                            self.mode = new_mode;
                            let speed = self.speed.unwrap_or(fusb_descriptor::UsbSpeed::High);
                            match self.handle_set_configured(true, speed).await {
                                Ok(tasks) => {
                                    self.endpoint_tasks = tasks;
                                    Ok(())
                                }
                                Err(e) => {
                                    self.is_configured.store(false, Ordering::Relaxed);
                                    Err(e.into_raw())
                                }
                            }
                        } else {
                            self.mode = new_mode;
                            Ok(())
                        }
                    } else {
                        Err(Status::NOT_SUPPORTED.into_raw())
                    };
                    let _ = responder.send(response);
                }
                _ => {
                    info!("Received unknown request");
                }
            }
        }
        if self.is_configured.load(Ordering::Relaxed) {
            self.endpoint_tasks = None;
            self.cleanup_endpoints().await;
        }
        info!("handle_requests exiting");
    }
}

async fn register_vmo(
    ep: &fusb_endpoint::EndpointProxy,
    id: u64,
    size: u64,
) -> Result<zx::Vmo, Status> {
    let _ = ep.unregister_vmos(&[id]).await;
    let vmo_infos =
        vec![fusb_endpoint::VmoInfo { id: Some(id), size: Some(size), ..Default::default() }];

    let mut response = ep.register_vmos(&vmo_infos).await.map_err(|e| {
        warn!("FIDL error: {:?}", e);
        Status::INTERNAL
    })?;
    let vmo_handle = response.pop().ok_or(Status::INTERNAL)?;
    if vmo_handle.id != Some(id) {
        return Err(Status::INTERNAL);
    }
    let vmo = vmo_handle.vmo.ok_or(Status::INTERNAL)?;
    Ok(vmo)
}

fn make_bulk_request(vmo_id: u64, offset: u64, size: u64) -> fusb_request::Request {
    fusb_request::Request {
        data: Some(vec![fusb_request::BufferRegion {
            buffer: Some(fusb_request::Buffer::VmoId(vmo_id)),
            offset: Some(offset),
            size: Some(size),
            ..Default::default()
        }]),
        defer_completion: Some(false),
        information: Some(
            fusb_request::RequestInfo::Bulk(fusb_request::BulkRequestInfo::default()),
        ),
        ..Default::default()
    }
}

fn queue_requests_batch(ep: &fusb_endpoint::EndpointProxy, reqs: Vec<fusb_request::Request>) {
    if !reqs.is_empty() {
        if let Err(e) = ep.queue_requests(reqs) {
            warn!("Failed to queue endpoint requests batch: {:?}", e);
        }
    }
}

fn queue_request(ep: &fusb_endpoint::EndpointProxy, vmo_id: u64, size: u64) {
    queue_requests_batch(ep, vec![make_bulk_request(vmo_id, 0, size)]);
}

async fn handle_read_completion(
    c: fusb_endpoint::Completion,
    vmo_size: u64,
    vmo_out: &zx::Vmo,
    recycled_buf: &mut Option<Vec<u8>>,
    tx: &mpsc::UnboundedSender<Vec<u8>>,
    ack_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    ep_out_clone: &fusb_endpoint::EndpointProxy,
) -> bool {
    if c.status.is_some_and(|s| Status::ok(s).is_ok()) {
        let size = std::cmp::min(c.transfer_size.unwrap_or(0), vmo_size) as usize;
        let mut buf = recycled_buf.take().unwrap_or_default();
        buf.resize(size, 0);
        if let Err(e) = vmo_out.op_range(zx::VmoOp::CACHE_CLEAN_INVALIDATE, 0, size as u64) {
            warn!("VMO cache op failed: {:?}", e);
        }
        if let Err(e) = vmo_out.read(&mut buf, 0) {
            warn!("Failed to read OUT VMO: {:?}", e);
            return false;
        }
        if tx.unbounded_send(buf).is_err() {
            return false;
        }
        let Some(buf_back) = ack_rx.next().await else {
            return false;
        };
        *recycled_buf = Some(buf_back);
        queue_request(ep_out_clone, USB_ZERO_OUT_VMO_ID, vmo_size);
    } else {
        warn!("Read error status: {:?}", c.status);
        fasync::Timer::new(std::time::Duration::from_millis(10)).await;
        queue_request(ep_out_clone, USB_ZERO_OUT_VMO_ID, vmo_size);
    }
    true
}

fn handle_write_completion(
    completion: Vec<fusb_endpoint::Completion>,
    data: Vec<u8>,
    ack_tx: &mpsc::UnboundedSender<Vec<u8>>,
) {
    let mut ok = false;
    for c in completion {
        if c.status.is_some_and(|s| Status::ok(s).is_ok()) {
            ok = true;
        } else {
            warn!("Write error status: {:?}", c.status);
        }
    }
    let send_data = if ok { data } else { vec![] };
    let _ = ack_tx.unbounded_send(send_data);
}

fn spawn_endpoint_pump(
    ep: fusb_endpoint::EndpointProxy,
    vmo_id: u64,
    transfer_size: u64,
) -> fasync::Task<()> {
    fasync::Task::spawn(async move {
        let mut event_stream = ep.take_event_stream();
        queue_request(&ep, vmo_id, transfer_size);

        while let Ok(Some(event)) = event_stream.try_next().await {
            match event {
                fusb_endpoint::EndpointEvent::OnCompletion { completion } => {
                    let ok =
                        completion.iter().all(|c| c.status.is_some_and(|s| Status::ok(s).is_ok()));
                    if !ok {
                        warn!("Endpoint transfer completed with non-OK status");
                    }
                    queue_request(&ep, vmo_id, transfer_size);
                }
            }
        }
    })
}

/// Runs the driver in uncoupled Source/Sink testing mode.
///
/// In this mode, the IN endpoint continuously sources fixed test data payloads
/// while the OUT endpoint continuously receives and sinks host data. Unlike loopback
/// mode, the endpoints operate independently without lockstep synchronization.
async fn run_source_sink(
    ep_in: fusb_endpoint::EndpointProxy,
    ep_out: fusb_endpoint::EndpointProxy,
    vmos_registered: &mut bool,
    transfer_size: u64,
) -> Result<(fasync::Task<()>, fasync::Task<()>), Status> {
    info!("Starting source/sink loop");

    // Register VMOs
    let _vmo_out = register_vmo(&ep_out, USB_ZERO_OUT_VMO_ID, DEFAULT_VMO_SIZE).await?;
    let vmo_in = match register_vmo(&ep_in, USB_ZERO_IN_VMO_ID, DEFAULT_VMO_SIZE).await {
        Ok(vmo) => vmo,
        Err(e) => {
            let _ = ep_out.unregister_vmos(&[USB_ZERO_OUT_VMO_ID]).await;
            return Err(e);
        }
    };
    *vmos_registered = true;

    if let Err(e) = vmo_in.op_range(zx::VmoOp::CACHE_CLEAN, 0, DEFAULT_VMO_SIZE) {
        warn!("VMO cache op failed: {:?}", e);
    }

    let sink_task = spawn_endpoint_pump(ep_out, USB_ZERO_OUT_VMO_ID, DEFAULT_VMO_SIZE);
    let source_task = spawn_endpoint_pump(ep_in, USB_ZERO_IN_VMO_ID, transfer_size);

    Ok((sink_task, source_task))
}

// Note on Loopback Synchronization:
// By waiting for ack_rx before calling queue_request, the driver ensures it doesn't
// overwrite the single buffer in the VMO during processing. However, this lockstep execution
// means the OUT endpoint will NAK the host while the driver is processing the previous packet.
// TODO(https://fxbug.dev/540805677): For higher performance requirements, a multi-buffered
// approach with a larger VMO or multiple ring buffer regions can be used.
async fn run_loopback(
    ep_in: fusb_endpoint::EndpointProxy,
    ep_out: fusb_endpoint::EndpointProxy,
    vmos_registered: &mut bool,
) -> Result<(fasync::Task<()>, fasync::Task<()>), Status> {
    info!("Starting loopback loop");

    let vmo_size = 4096;

    // Register VMOs
    let vmo_out = register_vmo(&ep_out, USB_ZERO_OUT_VMO_ID, vmo_size).await?;
    let vmo_in = match register_vmo(&ep_in, USB_ZERO_IN_VMO_ID, vmo_size).await {
        Ok(vmo) => vmo,
        Err(e) => {
            let _ = ep_out.unregister_vmos(&[USB_ZERO_OUT_VMO_ID]).await;
            return Err(e);
        }
    };
    *vmos_registered = true;

    let (tx, mut rx) = mpsc::unbounded::<Vec<u8>>();
    let (ack_tx, mut ack_rx) = mpsc::unbounded::<Vec<u8>>();

    let ep_out_clone = ep_out.clone();
    let read_task = fasync::Task::spawn(async move {
        let mut event_stream = ep_out_clone.take_event_stream();

        queue_request(&ep_out_clone, USB_ZERO_OUT_VMO_ID, vmo_size);

        let mut recycled_buf: Option<Vec<u8>> = None;

        while let Ok(Some(event)) = event_stream.try_next().await {
            match event {
                fusb_endpoint::EndpointEvent::OnCompletion { completion } => {
                    for c in completion {
                        if !handle_read_completion(
                            c,
                            vmo_size,
                            &vmo_out,
                            &mut recycled_buf,
                            &tx,
                            &mut ack_rx,
                            &ep_out_clone,
                        )
                        .await
                        {
                            return;
                        }
                    }
                }
            }
        }
    });

    let ep_in_clone = ep_in.clone();
    let write_task = fasync::Task::spawn(async move {
        let mut event_stream = ep_in_clone.take_event_stream();

        while let Some(data) = rx.next().await {
            let write_len = std::cmp::min(data.len() as u64, vmo_size);
            if vmo_in.write(&data[..write_len as usize], 0).is_err() {
                return;
            }
            if let Err(e) = vmo_in.op_range(zx::VmoOp::CACHE_CLEAN, 0, write_len) {
                warn!("VMO cache op failed: {:?}", e);
            }
            queue_request(&ep_in_clone, USB_ZERO_IN_VMO_ID, write_len);

            // Wait for write completion
            if let Ok(Some(event)) = event_stream.try_next().await {
                match event {
                    fusb_endpoint::EndpointEvent::OnCompletion { completion } => {
                        handle_write_completion(completion, data, &ack_tx);
                    }
                }
            } else {
                return;
            }
        }
    });

    Ok((read_task, write_task))
}

#[cfg(test)]
mod tests;
