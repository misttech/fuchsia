// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdf_component::{Driver, DriverContext, DriverError, Node, ParentProperty, driver_register};
use fidl_fuchsia_hardware_usb_descriptor as fusb_descriptor;
use fidl_fuchsia_hardware_usb_endpoint as fusb_endpoint;
use fidl_fuchsia_hardware_usb_function as fusb_function;
use fidl_fuchsia_hardware_usb_request as fusb_request;
use fuchsia_async as fasync;
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt, TryStreamExt};
use log::{debug, error, info, warn};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use zerocopy::{FromBytes as _, little_endian};
use zx::Status;

const USB_INTERFACE_DESC_SIZE: u8 = 9;
const USB_ENDPOINT_DESC_SIZE: u8 = 7;

pub const DEFAULT_VMO_SIZE: u64 = 65536;

const USB_MAX_PACKET_SIZE_FULL_SPEED: u16 = 64;
const USB_MAX_PACKET_SIZE_HIGH_SPEED: u16 = 512;
const USB_MAX_PACKET_SIZE_SUPER_SPEED: u16 = 1024;

// USB Zero Function Specific Constants
const USB_ZERO_NUM_INTERFACES: u8 = 1;
const USB_ZERO_NUM_ENDPOINTS: u8 = 2;
const USB_ZERO_DEFAULT_MAX_PACKET_SIZE: u16 = USB_MAX_PACKET_SIZE_HIGH_SPEED;

const USB_ZERO_OUT_VMO_ID: u64 = 1;
const USB_ZERO_IN_VMO_ID: u64 = 100;

const QUEUE_DEPTH: usize = 32;

const SG_IN_SHORT_ZLP_FLAG: u16 = 1;

const USB_ZERO_WRITE_PAYLOAD: &[u8] = &[0xDE, 0xAD, 0xBE, 0xEF];
const USB_ZERO_READ_PAYLOAD: &[u8] = &[0x12, 0x34, 0x56, 0x78];

#[derive(Copy, Clone, Debug, PartialEq, Eq, enumn::N)]
#[repr(u8)]
pub enum VendorRequest {
    SetStall = 0x50,
    ClearStall = 0x51,
    ConfigureEndpoint = 0x52,
    DisableEndpoint = 0x53,
    ConnectEndpoint = 0x54,
    Deconfigure = 0x55,
    WritePayload = 0x56,
    ReadPayload = 0x57,
    SetTestMode = 0x58,
    GetTestMode = 0x59,
    // Loopback control transfer requests matching the Linux usbtest runner
    // (drivers/usb/misc/usbtest.c) and f_sourcesink gadget driver:
    // 0x5b is control write (OUT), 0x5c is control read (IN).
    ControlLoopbackOut = 0x5b,
    ControlLoopbackIn = 0x5c,
    ConfigureScatterGather = 0x5d,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ScatterGatherRegion {
    offset: u64,
    size: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ScatterGatherConfig {
    out_regions: Vec<ScatterGatherRegion>,
    in_regions: Vec<ScatterGatherRegion>,
    in_short_zlp: bool,
    in_flight_out: usize,
    requeue_on_cancel: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ControlRequest {
    Vendor(VendorRequest),
    Standard(fusb_descriptor::StandardRequest),
}

impl ControlRequest {
    fn parse(bm_request_type: u8, b_request: u8) -> Result<Self, Status> {
        match fusb_descriptor::RequestType::from_primitive_allow_unknown(
            bm_request_type & fusb_descriptor::REQUEST_TYPE_MASK,
        ) {
            fusb_descriptor::RequestType::Standard => Ok(ControlRequest::Standard(
                fusb_descriptor::StandardRequest::from_primitive_allow_unknown(b_request),
            )),
            fusb_descriptor::RequestType::Vendor => {
                VendorRequest::n(b_request).map(ControlRequest::Vendor).ok_or(Status::NOT_SUPPORTED)
            }
            _ => Err(Status::NOT_SUPPORTED),
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
    is_configured: bool,
    vmos_registered: bool,
    endpoint_tasks: Option<(fasync::Task<()>, fasync::Task<()>)>,
    requeue_rx: Option<mpsc::UnboundedReceiver<usize>>,
    mode: TestMode,
    speed: Option<fusb_descriptor::UsbSpeed>,
    stalled_endpoints: Vec<u8>,
    control_loopback_buf: Vec<u8>,
    sg_config: Arc<Mutex<ScatterGatherConfig>>,
}

const BIND_USB_PROTOCOL_KEY: &str = "fuchsia.BIND_USB_PROTOCOL";

/// Returns the first integer `fuchsia.BIND_USB_PROTOCOL` property among the parent node
/// properties, if any.
pub(crate) fn get_usb_protocol<'a>(
    properties: impl IntoIterator<Item = ParentProperty<'a>>,
) -> Option<u32> {
    properties.into_iter().find_map(|property| {
        if property.key == BIND_USB_PROTOCOL_KEY { property.value.as_int() } else { None }
    })
}

driver_register!(UsbZeroFunction);

impl Driver for UsbZeroFunction {
    const NAME: &str = "usb-zero-function";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        let node = context.take_node()?;
        let scope = Arc::new(fasync::Scope::new_with_name("driver"));

        info!("Starting usb-zero-function");

        let protocol = get_usb_protocol(context.node_properties()).unwrap_or(1);
        let initial_mode = match protocol {
            2 => TestMode::Loopback,
            _ => TestMode::SourceSink,
        };
        let mode_string = match initial_mode {
            TestMode::SourceSink => "source and sink data",
            TestMode::Loopback => "loop input to output",
        };
        info!(
            "UsbZeroFunctionDriver starting with initial mode {:?} ({})",
            initial_mode, mode_string
        );

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

        let make_ep = |direction, endpoint, ep_info| fusb_function::EndpointResource {
            direction,
            endpoint,
            ep_info,
            max_packet_size: USB_ZERO_DEFAULT_MAX_PACKET_SIZE.into(),
        };

        let endpoints = vec![
            make_ep(
                fusb_descriptor::EndpointDirection::In,
                ep_in_server,
                fusb_endpoint::EndpointInfo::Bulk(Default::default()),
            ),
            make_ep(
                fusb_descriptor::EndpointDirection::Out,
                ep_out_server,
                fusb_endpoint::EndpointInfo::Bulk(Default::default()),
            ),
        ];

        let alloc_result = function_client
            .alloc_resources(USB_ZERO_NUM_INTERFACES, endpoints, &[mode_string.to_string()])
            .await
            .map_err(|e| {
                warn!("FIDL error: {:?}", e);
                Status::INTERNAL
            })?
            .map_err(Status::err_from_raw)?;

        let (interfaces, endpoints, string_indices) = alloc_result;
        let &[interface_num] = interfaces.as_slice() else {
            error!("Invalid interfaces length from AllocResources");
            return Err(Status::NO_RESOURCES.into());
        };
        let &[ep_in_addr, ep_out_addr] = endpoints.as_slice() else {
            error!("Invalid endpoints length from AllocResources");
            return Err(Status::NO_RESOURCES.into());
        };
        let interface_str_idx = string_indices.first().copied().unwrap_or(0);

        if (ep_in_addr & fusb_descriptor::ENDPOINT_DIRECTION_MASK) == 0
            || (ep_out_addr & fusb_descriptor::ENDPOINT_DIRECTION_MASK) != 0
        {
            error!("Invalid endpoint direction bits assigned");
            return Err(Status::NO_RESOURCES.into());
        }

        let default_max_packet_size_bytes = USB_ZERO_DEFAULT_MAX_PACKET_SIZE.to_le_bytes();
        let desc = vec![
            // Interface Descriptor (AltSetting 0)
            USB_INTERFACE_DESC_SIZE, // bLength
            fusb_descriptor::DescriptorType::Interface.into_primitive(), // bDescriptorType (Interface)
            interface_num,                                               // bInterfaceNumber
            0x00,                                                        // bAlternateSetting
            USB_ZERO_NUM_ENDPOINTS,                                      // bNumEndpoints
            fusb_descriptor::UsbClass::Vendor.into_primitive(), // bInterfaceClass (Vendor Specific)
            0,                                                  // bInterfaceSubClass
            protocol as u8,                                     // bInterfaceProtocol
            interface_str_idx,                                  // iInterface
            // Endpoint Descriptor (IN)
            USB_ENDPOINT_DESC_SIZE, // bLength
            fusb_descriptor::DescriptorType::Endpoint.into_primitive(), // bDescriptorType (Endpoint)
            ep_in_addr,                                                 // bEndpointAddress
            fusb_descriptor::EndpointType::Bulk.into_primitive(),       // bmAttributes (Bulk)
            default_max_packet_size_bytes[0],
            default_max_packet_size_bytes[1], // wMaxPacketSize (little endian)
            0,                                // bInterval
            // Endpoint Descriptor (OUT)
            USB_ENDPOINT_DESC_SIZE, // bLength
            fusb_descriptor::DescriptorType::Endpoint.into_primitive(), // bDescriptorType (Endpoint)
            ep_out_addr,                                                // bEndpointAddress
            fusb_descriptor::EndpointType::Bulk.into_primitive(),       // bmAttributes (Bulk)
            default_max_packet_size_bytes[0],
            default_max_packet_size_bytes[1], // wMaxPacketSize (little endian)
            0,                                // bInterval
        ];

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
                initial_mode,
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
    if (setup.bm_request_type & fusb_descriptor::ENDPOINT_DIRECTION_MASK) != 0
        || setup.w_length != 0
        || !write.is_empty()
    {
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
        mode: TestMode,
    ) -> Self {
        Self {
            function_client,
            ep_in,
            ep_in_addr,
            ep_out,
            ep_out_addr,
            interface_num,
            is_configured: false,
            vmos_registered: false,
            endpoint_tasks: None,
            requeue_rx: None,
            mode,
            speed: None,
            stalled_endpoints: Vec::new(),
            control_loopback_buf: Vec::new(),
            sg_config: Arc::new(Mutex::new(ScatterGatherConfig::default())),
        }
    }

    /// Resets the halt state and data toggle for all endpoints associated with this interface,
    /// per USB 2.0 Specification §9.4.10.
    async fn reset_interface_endpoints(&mut self) {
        for ep_addr in [self.ep_in_addr, self.ep_out_addr] {
            if let Err(e) = self.clear_endpoint_stall(ep_addr).await {
                warn!("Failed to clear endpoint stall for {}: {:?}", ep_addr, e);
            }
        }
    }

    /// Handles USB Chapter 9 SetInterface requests (both control and FIDL).
    /// Each usb-zero-function instance operates in a fixed mode (SourceSink or Loopback)
    /// under alternate setting 0. Setting alternate setting 0 resets endpoint halts and
    /// data toggles per USB 2.0 §9.4.10. Alternate settings != 0 are rejected.
    async fn handle_set_interface(&mut self, interface: u8, alt_setting: u8) -> Result<(), Status> {
        if interface != self.interface_num || alt_setting != 0 {
            return Err(Status::NOT_SUPPORTED);
        }
        self.reset_interface_endpoints().await;
        Ok(())
    }

    async fn cleanup_endpoints(&mut self) {
        self.endpoint_tasks = None;
        self.requeue_rx = None;
        if self.vmos_registered {
            let in_ids: Vec<u64> =
                (USB_ZERO_IN_VMO_ID..USB_ZERO_IN_VMO_ID + QUEUE_DEPTH as u64).collect();
            let out_ids: Vec<u64> =
                (USB_ZERO_OUT_VMO_ID..USB_ZERO_OUT_VMO_ID + QUEUE_DEPTH as u64).collect();
            let _ = self.ep_in.unregister_vmos(&in_ids).await;
            let _ = self.ep_out.unregister_vmos(&out_ids).await;
            self.vmos_registered = false;
        }
        self.is_configured = false;
        for ep_addr in [self.ep_in_addr, self.ep_out_addr] {
            let _ = self.function_client.disable_endpoint(ep_addr).await;
        }
        self.stalled_endpoints.clear();
        self.control_loopback_buf.clear();
    }

    async fn set_endpoint_stall(&mut self, ep_addr: u8) -> Result<(), Status> {
        // EP0 (Control Endpoint) stall management is handled by hardware / driver stack
        // and cannot be stalled via this vendor request. Return INVALID_ARGS for EP0.
        if (ep_addr & fusb_descriptor::ENDPOINT_NUMBER_MASK) == 0 {
            return Err(Status::INVALID_ARGS);
        }
        self.function_client
            .endpoint_set_stall(ep_addr)
            .await
            .map_err(|e| {
                warn!("FIDL error setting stall: {:?}", e);
                Status::INTERNAL
            })?
            .map_err(Status::err_from_raw)?;
        if !self.stalled_endpoints.contains(&ep_addr) {
            self.stalled_endpoints.push(ep_addr);
        }
        Ok(())
    }

    async fn clear_endpoint_stall(&mut self, ep_addr: u8) -> Result<(), Status> {
        // Clearing stall on EP0 is a no-op because EP0 stall status automatically resets upon the next setup packet.
        if (ep_addr & fusb_descriptor::ENDPOINT_NUMBER_MASK) == 0 {
            return Ok(());
        }
        self.function_client
            .endpoint_clear_stall(ep_addr)
            .await
            .map_err(|e| {
                warn!("FIDL error clearing stall: {:?}", e);
                Status::INTERNAL
            })?
            .map_err(Status::err_from_raw)?;
        self.stalled_endpoints.retain(|&addr| addr != ep_addr);
        Ok(())
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
            super_speed_companion: super_speed_companion.clone(),
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
                TestMode::Loopback => run_loopback(
                    self.ep_in.clone(),
                    self.ep_out.clone(),
                    &mut self.vmos_registered,
                    transfer_size,
                    self.sg_config.clone(),
                )
                .await
                .map(|(r_task, w_task, requeue_rx)| {
                    self.requeue_rx = Some(requeue_rx);
                    (r_task, w_task)
                }),
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
        // Vendor control requests must have Vendor type in bm_request_type
        if (setup.bm_request_type & fusb_descriptor::REQUEST_TYPE_MASK)
            != fusb_descriptor::RequestType::Vendor.into_primitive()
        {
            return Err(Status::INVALID_ARGS);
        }
        match vendor_req {
            VendorRequest::SetStall => {
                let ep_addr = validate_vendor_out_request(setup, write)?;
                self.set_endpoint_stall(ep_addr).await?;
                Ok(Vec::new())
            }
            VendorRequest::ClearStall => {
                let ep_addr = validate_vendor_out_request(setup, write)?;
                self.clear_endpoint_stall(ep_addr).await?;
                Ok(Vec::new())
            }
            VendorRequest::ConfigureEndpoint => {
                let ep_addr = validate_vendor_out_request(setup, write)?;
                let speed = self.speed.unwrap_or(fusb_descriptor::UsbSpeed::High);
                let w_max_packet_size = Self::max_packet_size_for_speed(speed);
                let ep_config = fusb_function::EndpointConfiguration {
                    descriptor: Some(fusb_function::EndpointDescriptor {
                        bm_attributes: fusb_descriptor::EndpointType::Bulk.into_primitive(),
                        w_max_packet_size,
                        b_interval: 0,
                    }),
                    super_speed_companion: None,
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
                if (setup.bm_request_type & fusb_descriptor::ENDPOINT_DIRECTION_MASK) != 0
                    || setup.w_length != 0
                    || !write.is_empty()
                {
                    return Err(Status::INVALID_ARGS);
                }
                self.endpoint_tasks = None;
                self.cleanup_endpoints().await;
                self.is_configured = false;
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
                if (setup.bm_request_type & fusb_descriptor::ENDPOINT_DIRECTION_MASK) != 0
                    || setup.w_length as usize != write.len()
                    || write != USB_ZERO_WRITE_PAYLOAD
                {
                    return Err(Status::INVALID_ARGS);
                }
                Ok(Vec::new())
            }
            VendorRequest::ReadPayload => {
                if (setup.bm_request_type & fusb_descriptor::ENDPOINT_DIRECTION_MASK) == 0
                    || (setup.w_length as usize) < USB_ZERO_READ_PAYLOAD.len()
                    || !write.is_empty()
                {
                    return Err(Status::INVALID_ARGS);
                }
                Ok(USB_ZERO_READ_PAYLOAD.to_vec())
            }
            VendorRequest::SetTestMode => {
                // Dynamic mode switching is not supported; mode is fixed per configuration.
                Err(Status::NOT_SUPPORTED)
            }
            VendorRequest::GetTestMode => {
                if (setup.bm_request_type & fusb_descriptor::ENDPOINT_DIRECTION_MASK) == 0
                    || setup.w_value != 0
                    || setup.w_length != 1
                    || !write.is_empty()
                {
                    return Err(Status::INVALID_ARGS);
                }
                Ok(vec![self.mode as u8])
            }
            VendorRequest::ControlLoopbackOut => {
                if (setup.bm_request_type & fusb_descriptor::ENDPOINT_DIRECTION_MASK) != 0
                    || setup.w_length != write.len() as u16
                {
                    return Err(Status::INVALID_ARGS);
                }
                self.control_loopback_buf = write.to_vec();
                Ok(Vec::new())
            }
            VendorRequest::ControlLoopbackIn => {
                if (setup.bm_request_type & fusb_descriptor::ENDPOINT_DIRECTION_MASK) == 0
                    || !write.is_empty()
                {
                    return Err(Status::INVALID_ARGS);
                }
                let len = std::cmp::min(setup.w_length as usize, self.control_loopback_buf.len());
                Ok(self.control_loopback_buf[..len].to_vec())
            }
            VendorRequest::ConfigureScatterGather => {
                if (setup.bm_request_type & fusb_descriptor::ENDPOINT_DIRECTION_MASK) != 0
                    || usize::from(setup.w_length) != write.len()
                {
                    return Err(Status::INVALID_ARGS);
                }
                let mut new_config = if write.is_empty() {
                    ScatterGatherConfig::default()
                } else {
                    let (num_out, rest) = little_endian::U16::read_from_prefix(write)
                        .map_err(|_| Status::INVALID_ARGS)?;
                    let (num_in, mut rest) = little_endian::U16::read_from_prefix(rest)
                        .map_err(|_| Status::INVALID_ARGS)?;
                    let num_out = usize::from(num_out.get());
                    let num_in = usize::from(num_in.get());

                    let mut parse_regions =
                        |count: usize| -> Result<Vec<ScatterGatherRegion>, Status> {
                            let mut regions: Vec<ScatterGatherRegion> = Vec::with_capacity(count);
                            for _ in 0..count {
                                let (offset, next) = little_endian::U32::read_from_prefix(rest)
                                    .map_err(|_| Status::INVALID_ARGS)?;
                                let (size, next) = little_endian::U32::read_from_prefix(next)
                                    .map_err(|_| Status::INVALID_ARGS)?;
                                rest = next;
                                let offset = u64::from(offset.get());
                                let size = u64::from(size.get());
                                let Some(end) =
                                    offset.checked_add(size).filter(|&end| end <= DEFAULT_VMO_SIZE)
                                else {
                                    return Err(Status::INVALID_ARGS);
                                };
                                if size == 0
                                    || regions
                                        .iter()
                                        .any(|r| offset < r.offset + r.size && r.offset < end)
                                {
                                    return Err(Status::INVALID_ARGS);
                                }
                                regions.push(ScatterGatherRegion { offset, size });
                            }
                            Ok(regions)
                        };

                    let out_regions = parse_regions(num_out)?;
                    let in_regions = parse_regions(num_in)?;
                    if !rest.is_empty() {
                        return Err(Status::INVALID_ARGS);
                    }

                    ScatterGatherConfig {
                        out_regions,
                        in_regions,
                        in_short_zlp: (setup.w_value & SG_IN_SHORT_ZLP_FLAG) != 0,
                        in_flight_out: 0,
                        requeue_on_cancel: false,
                    }
                };

                if self.is_configured && self.mode == TestMode::Loopback {
                    let expected_requeued = {
                        let mut sg = self.sg_config.lock().unwrap();
                        let in_flight = sg.in_flight_out;
                        new_config.in_flight_out = in_flight;
                        new_config.requeue_on_cancel = in_flight > 0;
                        *sg = new_config;
                        if let Some(rx) = &mut self.requeue_rx {
                            while let Ok(_) = rx.try_recv() {}
                        }
                        in_flight
                    };
                    let _ = self.ep_in.cancel_all().await;
                    let _ = self.ep_out.cancel_all().await;
                    if let Some(rx) = &mut self.requeue_rx {
                        let mut requeued = 0;
                        while requeued < expected_requeued {
                            let Some(n) = rx.next().await else {
                                break;
                            };
                            requeued += n;
                        }
                    }
                    self.sg_config.lock().unwrap().requeue_on_cancel = false;
                } else {
                    *self.sg_config.lock().unwrap() = new_config;
                }
                Ok(Vec::new())
            }
        }
    }

    fn validate_endpoint_feature_request(
        &self,
        setup: &fusb_descriptor::UsbSetup,
        write: &[u8],
    ) -> Result<u8, Status> {
        if (setup.bm_request_type & fusb_descriptor::ENDPOINT_DIRECTION_MASK) != 0
            || (setup.bm_request_type & fusb_descriptor::REQUEST_RECIPIENT_MASK)
                != fusb_descriptor::RequestRecipient::Endpoint.into_primitive()
            || setup.w_value
                != fusb_descriptor::FeatureSelector::EndpointHalt.into_primitive() as u16
            || setup.w_length != 0
            || setup.w_index > 0xff
            || !write.is_empty()
        {
            return Err(Status::NOT_SUPPORTED);
        }
        let ep_addr = (setup.w_index & 0xff) as u8;
        if ![self.ep_in_addr, self.ep_out_addr].contains(&ep_addr) {
            return Err(Status::NOT_SUPPORTED);
        }
        Ok(ep_addr)
    }

    async fn handle_control_request(
        &mut self,
        setup: &fusb_descriptor::UsbSetup,
        write: &[u8],
    ) -> Result<Vec<u8>, Status> {
        let recipient = setup.bm_request_type & fusb_descriptor::REQUEST_RECIPIENT_MASK;
        let is_in = (setup.bm_request_type & fusb_descriptor::ENDPOINT_DIRECTION_MASK) != 0;
        match ControlRequest::parse(setup.bm_request_type, setup.b_request)? {
            ControlRequest::Vendor(vendor_req) => {
                self.handle_vendor_request(vendor_req, setup, write).await
            }
            ControlRequest::Standard(req) => match req {
                fusb_descriptor::StandardRequest::GetStatus => {
                    if !is_in || setup.w_value != 0 || setup.w_length != 2 || !write.is_empty() {
                        return Err(Status::NOT_SUPPORTED);
                    }
                    if recipient == fusb_descriptor::RequestRecipient::Endpoint.into_primitive()
                        && setup.w_index <= 0xff
                    {
                        let ep_addr = (setup.w_index & 0xff) as u8;
                        if [self.ep_in_addr, self.ep_out_addr].contains(&ep_addr)
                            || (ep_addr & fusb_descriptor::ENDPOINT_NUMBER_MASK) == 0
                        {
                            let is_stalled = self.stalled_endpoints.contains(&ep_addr);
                            let status_word = u16::from(is_stalled);
                            Ok(status_word.to_le_bytes().to_vec())
                        } else {
                            Err(Status::NOT_SUPPORTED)
                        }
                    } else if (recipient
                        == fusb_descriptor::RequestRecipient::Device.into_primitive()
                        && setup.w_index == 0)
                        || (recipient
                            == fusb_descriptor::RequestRecipient::Interface.into_primitive()
                            && setup.w_index == u16::from(self.interface_num))
                    {
                        Ok(vec![0x00, 0x00])
                    } else {
                        Err(Status::NOT_SUPPORTED)
                    }
                }
                fusb_descriptor::StandardRequest::ClearFeature => {
                    let ep_addr = self.validate_endpoint_feature_request(setup, write)?;
                    self.clear_endpoint_stall(ep_addr).await?;
                    Ok(Vec::new())
                }
                fusb_descriptor::StandardRequest::SetFeature => {
                    let ep_addr = self.validate_endpoint_feature_request(setup, write)?;
                    self.set_endpoint_stall(ep_addr).await?;
                    Ok(Vec::new())
                }
                fusb_descriptor::StandardRequest::GetInterface => {
                    if is_in
                        && recipient
                            == fusb_descriptor::RequestRecipient::Interface.into_primitive()
                        && setup.w_value == 0
                        && setup.w_index == u16::from(self.interface_num)
                        && setup.w_length == 1
                        && write.is_empty()
                    {
                        Ok(vec![0x00])
                    } else {
                        Err(Status::NOT_SUPPORTED)
                    }
                }
                fusb_descriptor::StandardRequest::SetInterface => {
                    if !is_in
                        && recipient
                            == fusb_descriptor::RequestRecipient::Interface.into_primitive()
                        && setup.w_length == 0
                        && write.is_empty()
                    {
                        let interface =
                            u8::try_from(setup.w_index).map_err(|_| Status::NOT_SUPPORTED)?;
                        let alt_setting =
                            u8::try_from(setup.w_value).map_err(|_| Status::NOT_SUPPORTED)?;
                        self.handle_set_interface(interface, alt_setting).await?;
                        Ok(Vec::new())
                    } else {
                        Err(Status::NOT_SUPPORTED)
                    }
                }
                _ => Err(Status::NOT_SUPPORTED),
            },
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
                    debug!("Received control request: {:?}", setup);
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
                            self.is_configured = configured;
                            let _ = responder.send(Ok(()));
                        }
                        Err(e) => {
                            self.is_configured = false;
                            let _ = responder.send(Err(e.into_raw()));
                        }
                    }
                }
                fusb_function::UsbFunctionInterfaceRequest::SetInterface {
                    interface,
                    alt_setting,
                    responder,
                } => {
                    let status = self.handle_set_interface(interface, alt_setting).await;
                    let _ = responder.send(status.map_err(|s| s.into_raw()));
                }
                _ => {
                    info!("Received unknown request");
                }
            }
        }
        if self.is_configured {
            self.endpoint_tasks = None;
            self.cleanup_endpoints().await;
        }
        info!("handle_requests exiting");
    }
}

async fn register_vmos(
    ep: &fusb_endpoint::EndpointProxy,
    base_id: u64,
    count: usize,
    size: u64,
) -> Result<Vec<zx::Vmo>, Status> {
    let unreg_ids: Vec<u64> = (base_id..base_id + count as u64).collect();
    let _ = ep.unregister_vmos(&unreg_ids).await;

    let vmo_infos: Vec<_> = (base_id..base_id + count as u64)
        .map(|id| fusb_endpoint::VmoInfo { id: Some(id), size: Some(size), ..Default::default() })
        .collect();

    let mut response = ep.register_vmos(&vmo_infos).await.map_err(|e| {
        warn!("FIDL error: {:?}", e);
        Status::INTERNAL
    })?;
    if response.len() != count {
        return Err(Status::INTERNAL);
    }
    response.sort_by_key(|h| h.id);
    for (i, h) in response.iter().enumerate() {
        if h.id != Some(base_id + i as u64) {
            return Err(Status::INTERNAL);
        }
    }
    response.into_iter().map(|mut h| h.vmo.take().ok_or(Status::INTERNAL)).collect()
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

fn make_sg_bulk_request(
    vmo_id: u64,
    regions: &[ScatterGatherRegion],
    short: bool,
) -> fusb_request::Request {
    fusb_request::Request {
        data: Some(
            regions
                .iter()
                .map(|r| fusb_request::BufferRegion {
                    buffer: Some(fusb_request::Buffer::VmoId(vmo_id)),
                    offset: Some(r.offset),
                    size: Some(r.size),
                    ..Default::default()
                })
                .collect(),
        ),
        defer_completion: Some(false),
        information: Some(
            fusb_request::RequestInfo::Bulk(fusb_request::BulkRequestInfo::default()),
        ),
        short: if short { Some(true) } else { None },
        ..Default::default()
    }
}

fn queue_requests_batch(
    ep: &fusb_endpoint::EndpointProxy,
    reqs: Vec<fusb_request::Request>,
) -> Result<(), fidl::Error> {
    if !reqs.is_empty() {
        if let Err(e) = ep.queue_requests(reqs) {
            error!("Failed to queue endpoint requests batch: {:?}", e);
            return Err(e);
        }
    }
    Ok(())
}

fn completion_vmo_id(c: &fusb_endpoint::Completion) -> Option<u64> {
    let region = c.request.as_ref()?.data.as_ref()?.first()?;
    match region.buffer.as_ref()? {
        fusb_request::Buffer::VmoId(id) => Some(*id),
        _ => None,
    }
}

fn spawn_endpoint_pump_ring(
    ep: fusb_endpoint::EndpointProxy,
    base_vmo_id: u64,
    transfer_size: u64,
    queue_depth: usize,
) -> fasync::Task<()> {
    fasync::Task::spawn(async move {
        let mut event_stream = ep.take_event_stream();
        let make_req = |vmo_id, size| make_bulk_request(vmo_id, 0, size);
        let reqs: Vec<_> =
            (0..queue_depth as u64).map(|i| make_req(base_vmo_id + i, transfer_size)).collect();
        let _ = queue_requests_batch(&ep, reqs);

        while let Ok(Some(event)) = event_stream.try_next().await {
            match event {
                fusb_endpoint::EndpointEvent::OnCompletion { completion } => {
                    let mut re_reqs = Vec::with_capacity(completion.len());
                    for c in completion {
                        if let Some(status) = c.status {
                            match status {
                                zx::sys::ZX_ERR_IO_REFUSED => {
                                    // Endpoint was halted/stalled by host or aborted by controller;
                                    // fall through to re-queue buffer to preserve pump ring depth
                                    // once halt is cleared.
                                }
                                zx::sys::ZX_ERR_IO_NOT_PRESENT => {
                                    warn!(
                                        "Endpoint disconnected (IO_NOT_PRESENT), stopping bulk pump loop"
                                    );
                                    return;
                                }
                                zx::sys::ZX_ERR_CANCELED => {
                                    // Explicitly canceled by client; drop request without re-queuing.
                                    continue;
                                }
                                zx::sys::ZX_OK => {
                                    // Transfer completed successfully; fall through to re-queue.
                                }
                                err => {
                                    warn!(
                                        "Endpoint transfer completed with non-OK status: {:?}",
                                        Status::err_from_raw(err)
                                    );
                                }
                            }
                        }

                        if let Some(vmo_id) = completion_vmo_id(&c) {
                            re_reqs.push(make_req(vmo_id, transfer_size));
                        } else {
                            warn!("Malformed completion request data: missing VmoId");
                        }
                    }
                    if !re_reqs.is_empty() {
                        let _ = queue_requests_batch(&ep, re_reqs);
                    }
                }
            }
        }
    })
}

/// Runs the driver in uncoupled Source/Sink testing mode for Bulk endpoints.
async fn run_source_sink(
    ep_in: fusb_endpoint::EndpointProxy,
    ep_out: fusb_endpoint::EndpointProxy,
    vmos_registered: &mut bool,
    transfer_size: u64,
) -> Result<(fasync::Task<()>, fasync::Task<()>), Status> {
    if transfer_size > DEFAULT_VMO_SIZE {
        return Err(Status::INVALID_ARGS);
    }
    info!("Starting source/sink loop (Bulk Only)");

    // Register VMOs
    let _vmo_out =
        register_vmos(&ep_out, USB_ZERO_OUT_VMO_ID, QUEUE_DEPTH, DEFAULT_VMO_SIZE).await?;
    let vmos_in =
        match register_vmos(&ep_in, USB_ZERO_IN_VMO_ID, QUEUE_DEPTH, DEFAULT_VMO_SIZE).await {
            Ok(vmos) => vmos,
            Err(e) => {
                let ids: Vec<u64> =
                    (USB_ZERO_OUT_VMO_ID..USB_ZERO_OUT_VMO_ID + QUEUE_DEPTH as u64).collect();
                let _ = ep_out.unregister_vmos(&ids).await;
                return Err(e);
            }
        };
    *vmos_registered = true;

    for vmo_in in &vmos_in {
        if let Err(e) = vmo_in.op_range(zx::VmoOp::CACHE_CLEAN, 0, DEFAULT_VMO_SIZE) {
            warn!("VMO cache op failed: {:?}", e);
        }
    }

    let sink_task =
        spawn_endpoint_pump_ring(ep_out, USB_ZERO_OUT_VMO_ID, DEFAULT_VMO_SIZE, QUEUE_DEPTH);
    let source_task =
        spawn_endpoint_pump_ring(ep_in, USB_ZERO_IN_VMO_ID, transfer_size, QUEUE_DEPTH);

    Ok((sink_task, source_task))
}

struct MappedVmo {
    _vmo: zx::Vmo,
    addr: usize,
    size: usize,
}

impl MappedVmo {
    pub fn as_ptr(&self) -> *const u8 {
        self.addr as *const u8
    }

    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.addr as *mut u8
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn flush_invalidate_range(&self, offset: usize, len: usize) {
        if len == 0 {
            return;
        }
        assert!(offset.checked_add(len).is_some_and(|end| end <= self.size));
        // SAFETY: `self.as_ptr()` points to the start of a valid VMAR mapping
        // of `self.size` bytes, and we asserted `offset + len <= self.size`, so
        // `offset` is in bounds of the allocated mapping and does not wrap
        // around the address space.
        let ptr = unsafe { self.as_ptr().add(offset) };
        // SAFETY: `ptr` is a valid mapped userspace pointer for `len` bytes
        // within `self.size` returned by `root_vmar.map`.
        unsafe {
            let _ = zx::sys::zx_cache_flush(
                ptr,
                len,
                zx::sys::ZX_CACHE_FLUSH_DATA | zx::sys::ZX_CACHE_FLUSH_INVALIDATE,
            );
        }
    }

    pub fn flush_range(&self, offset: usize, len: usize) {
        if len == 0 {
            return;
        }
        assert!(offset.checked_add(len).is_some_and(|end| end <= self.size));
        // SAFETY: `self.as_ptr()` points to the start of a valid VMAR mapping
        // of `self.size` bytes, and we asserted `offset + len <= self.size`, so
        // `offset` is in bounds of the allocated mapping and does not wrap
        // around the address space.
        let ptr = unsafe { self.as_ptr().add(offset) };
        // SAFETY: `ptr` is a valid mapped userspace pointer for `len` bytes
        // within `self.size` returned by `root_vmar.map`.
        unsafe {
            let _ = zx::sys::zx_cache_flush(ptr, len, zx::sys::ZX_CACHE_FLUSH_DATA);
        }
    }

    pub fn read_slice(&self, offset: usize, len: usize, dst: &mut Vec<u8>) {
        if len == 0 {
            return;
        }
        assert!(offset.checked_add(len).is_some_and(|end| end <= self.size));
        self.flush_invalidate_range(offset, len);
        // SAFETY: `self.as_ptr()` points to the start of a valid VMAR mapping
        // of `self.size` bytes, and we asserted `offset + len <= self.size`, so
        // `offset` is in bounds of the allocated mapping and does not wrap
        // around the address space.
        let ptr = unsafe { self.as_ptr().add(offset) };
        // SAFETY: `ptr` points to `len` initialized, contiguous bytes within
        // the VMAR mapping of `self.size` bytes. The USB controller transfer
        // has completed and the request will not be re-queued until after the
        // slice data is copied into `dst`.
        let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
        dst.extend_from_slice(slice);
    }

    pub fn write_slice(&self, offset: usize, src: &[u8]) {
        let len = src.len();
        if len == 0 {
            return;
        }
        assert!(offset.checked_add(len).is_some_and(|end| end <= self.size));
        // SAFETY: `self.as_mut_ptr()` points to the start of a valid VMAR
        // mapping of `self.size` bytes, and we asserted
        // `offset + len <= self.size`, so `offset` is in bounds of the
        // allocated mapping and does not wrap around the address space.
        let dst_ptr = unsafe { self.as_mut_ptr().add(offset) };
        // SAFETY: `src.as_ptr()` points to `len` valid contiguous bytes in
        // userspace memory, and `dst_ptr` points to a valid userspace VMAR
        // mapping for `len` bytes within `self.size`. Both pointers are
        // properly aligned and reside in disjoint allocations that do not
        // overlap.
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), dst_ptr, len);
        }
        self.flush_range(offset, len);
    }
}

impl Drop for MappedVmo {
    fn drop(&mut self) {
        if self.addr != 0 && self.size != 0 {
            let root_vmar = fuchsia_runtime::vmar_root_self();
            // SAFETY: `self.addr` is a valid userspace virtual memory address
            // returned by `root_vmar.map` of size `self.size`, which has not
            // yet been unmapped. No other references alias this virtual memory
            // range upon destruction.
            if let Err(status) = unsafe { root_vmar.unmap(self.addr, self.size) } {
                warn!(
                    "MappedVmo: failed to unmap VMAR (addr={:#x}, size={}): {:?}",
                    self.addr, self.size, status
                );
            }
        }
    }
}

fn map_vmos(vmos: Vec<zx::Vmo>, size: u64, flags: zx::VmarFlags) -> Result<Vec<MappedVmo>, Status> {
    let size_usize = usize::try_from(size).map_err(|_| Status::INVALID_ARGS)?;
    let root_vmar = fuchsia_runtime::vmar_root_self();
    vmos.into_iter()
        .map(|vmo| {
            let addr = root_vmar.map(0, &vmo, 0, size_usize, flags)?;
            Ok(MappedVmo { _vmo: vmo, addr, size: size_usize })
        })
        .collect()
}

async fn run_loopback(
    ep_in: fusb_endpoint::EndpointProxy,
    ep_out: fusb_endpoint::EndpointProxy,
    vmos_registered: &mut bool,
    transfer_size: u64,
    sg_config: Arc<Mutex<ScatterGatherConfig>>,
) -> Result<(fasync::Task<()>, fasync::Task<()>, mpsc::UnboundedReceiver<usize>), Status> {
    if transfer_size > DEFAULT_VMO_SIZE {
        return Err(Status::INVALID_ARGS);
    }
    info!("Starting loopback loop (Bulk Only)");

    // Register and map OUT VMOs (PERM_WRITE is required on riscv64 for zx_cache_flush).
    let vmos_out =
        register_vmos(&ep_out, USB_ZERO_OUT_VMO_ID, QUEUE_DEPTH, DEFAULT_VMO_SIZE).await?;
    let mapped_out = match map_vmos(
        vmos_out,
        DEFAULT_VMO_SIZE,
        zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
    ) {
        Ok(m) => m,
        Err(e) => {
            let ids: Vec<u64> =
                (USB_ZERO_OUT_VMO_ID..USB_ZERO_OUT_VMO_ID + QUEUE_DEPTH as u64).collect();
            let _ = ep_out.unregister_vmos(&ids).await;
            return Err(e);
        }
    };

    // Register and map IN VMOs
    let vmos_in =
        match register_vmos(&ep_in, USB_ZERO_IN_VMO_ID, QUEUE_DEPTH, DEFAULT_VMO_SIZE).await {
            Ok(vmos) => vmos,
            Err(e) => {
                let ids: Vec<u64> =
                    (USB_ZERO_OUT_VMO_ID..USB_ZERO_OUT_VMO_ID + QUEUE_DEPTH as u64).collect();
                let _ = ep_out.unregister_vmos(&ids).await;
                return Err(e);
            }
        };
    let mapped_in = match map_vmos(
        vmos_in,
        DEFAULT_VMO_SIZE,
        zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
    ) {
        Ok(m) => m,
        Err(e) => {
            let out_ids: Vec<u64> =
                (USB_ZERO_OUT_VMO_ID..USB_ZERO_OUT_VMO_ID + QUEUE_DEPTH as u64).collect();
            let in_ids: Vec<u64> =
                (USB_ZERO_IN_VMO_ID..USB_ZERO_IN_VMO_ID + QUEUE_DEPTH as u64).collect();
            let _ = ep_out.unregister_vmos(&out_ids).await;
            let _ = ep_in.unregister_vmos(&in_ids).await;
            return Err(e);
        }
    };
    *vmos_registered = true;

    let (requeue_tx, requeue_rx) = mpsc::unbounded();
    let (read_bulk, write_bulk) = spawn_loopback_pair(
        ep_in,
        ep_out,
        mapped_in,
        mapped_out,
        USB_ZERO_IN_VMO_ID,
        USB_ZERO_OUT_VMO_ID,
        transfer_size,
        sg_config,
        requeue_tx,
    );

    Ok((read_bulk, write_bulk, requeue_rx))
}

fn make_out_req(vmo_id: u64, sg: &ScatterGatherConfig, buffer_size: u64) -> fusb_request::Request {
    if sg.out_regions.is_empty() {
        make_bulk_request(vmo_id, 0, buffer_size)
    } else {
        make_sg_bulk_request(vmo_id, &sg.out_regions, false)
    }
}

fn flush_out_vmo(mapped: &MappedVmo, sg: &ScatterGatherConfig, buffer_size: u64) {
    if sg.out_regions.is_empty() {
        mapped.flush_invalidate_range(0, buffer_size as usize);
    } else {
        for r in &sg.out_regions {
            mapped.flush_invalidate_range(r.offset as usize, r.size as usize);
        }
    }
}

fn release_in_slot(
    c: &fusb_endpoint::Completion,
    base_in_id: u64,
    count_in: usize,
    free_slots: &mut VecDeque<usize>,
    zlp_slots: &mut VecDeque<usize>,
) {
    if let Some(vmo_id) = completion_vmo_id(c) {
        if let Some(slot) = vmo_id
            .checked_sub(base_in_id)
            .and_then(|diff| usize::try_from(diff).ok())
            .filter(|&slot| slot < count_in && !free_slots.contains(&slot))
        {
            free_slots.push_back(slot);
        }
    } else if c.request.as_ref().is_some_and(|r| r.data.as_ref().is_none_or(|d| d.is_empty())) {
        if let Some(slot) = zlp_slots.pop_front() {
            if !free_slots.contains(&slot) {
                free_slots.push_back(slot);
            }
        }
    }
}

fn spawn_loopback_pair(
    ep_in: fusb_endpoint::EndpointProxy,
    ep_out: fusb_endpoint::EndpointProxy,
    mapped_in: Vec<MappedVmo>,
    mapped_out: Vec<MappedVmo>,
    base_in_id: u64,
    base_out_id: u64,
    buffer_size: u64,
    sg_config: Arc<Mutex<ScatterGatherConfig>>,
    requeue_tx: mpsc::UnboundedSender<usize>,
) -> (fasync::Task<()>, fasync::Task<()>) {
    let (mut tx, mut rx) = mpsc::channel::<Vec<u8>>(QUEUE_DEPTH);
    let (mut ack_tx, mut ack_rx) = mpsc::channel::<Vec<u8>>(QUEUE_DEPTH);

    let ep_out_clone = ep_out.clone();
    let count_out = mapped_out.len();
    let sg_config_read = sg_config.clone();
    let mut event_stream = ep_out_clone.take_event_stream();

    // Queue initial ring of OUT requests synchronously so in_flight_out is
    // tracked immediately.
    {
        let mut initial_sg = sg_config_read.lock().unwrap();
        let mut initial_reqs = Vec::with_capacity(count_out);
        for (i, mapped) in mapped_out.iter().enumerate() {
            let vmo_id = base_out_id + (i as u64);
            flush_out_vmo(mapped, &initial_sg, buffer_size);
            initial_reqs.push(make_out_req(vmo_id, &initial_sg, buffer_size));
        }
        initial_sg.in_flight_out =
            if queue_requests_batch(&ep_out_clone, initial_reqs).is_ok() { count_out } else { 0 };
        debug!("read_task queued initial {} OUT requests of size {}", count_out, buffer_size);
    }

    let read_task = fasync::Task::spawn(async move {
        let mut recycled_bufs: Vec<Vec<u8>> = Vec::with_capacity(QUEUE_DEPTH);

        while let Ok(Some(event)) = event_stream.try_next().await {
            match event {
                fusb_endpoint::EndpointEvent::OnCompletion { completion } => {
                    // Harvest any recycled buffers from write_task without blocking
                    while let Ok(buf) = ack_rx.try_recv() {
                        recycled_bufs.push(buf);
                    }

                    let completed_count = completion.len();
                    let mut requeue_slots = Vec::with_capacity(completed_count);
                    let mut canceled_slots = Vec::new();
                    for c in completion {
                        let vmo_slot = completion_vmo_id(&c).and_then(|id| {
                            id.checked_sub(base_out_id)
                                .and_then(|diff| usize::try_from(diff).ok())
                                .filter(|&idx| idx < mapped_out.len())
                                .map(|idx| (id, idx))
                        });
                        if let Some(status) = c.status {
                            match status {
                                zx::sys::ZX_ERR_IO_REFUSED => {
                                    // Endpoint halted or transfer aborted; re-queue buffer to
                                    // preserve ring depth without forwarding stale/partial data.
                                    if let Some(slot) = vmo_slot {
                                        requeue_slots.push(slot);
                                    }
                                    continue;
                                }
                                zx::sys::ZX_ERR_CANCELED => {
                                    if let Some(slot) = vmo_slot {
                                        canceled_slots.push(slot);
                                    }
                                    continue;
                                }
                                zx::sys::ZX_ERR_IO_NOT_PRESENT => {
                                    warn!(
                                        "Endpoint disconnected (IO_NOT_PRESENT), stopping loopback read"
                                    );
                                    return;
                                }
                                zx::sys::ZX_OK => {}
                                err => {
                                    warn!(
                                        "Endpoint read completed with non-OK status: {:?}",
                                        Status::err_from_raw(err)
                                    );
                                    if let Some(slot) = vmo_slot {
                                        requeue_slots.push(slot);
                                    }
                                    continue;
                                }
                            }
                        }

                        if let Some((id, idx)) = vmo_slot {
                            let mut buf = recycled_bufs
                                .pop()
                                .unwrap_or_else(|| Vec::with_capacity(buffer_size as usize));
                            buf.clear();

                            let mut remaining = std::cmp::min(
                                c.transfer_size.unwrap_or(0) as usize,
                                mapped_out[idx].size(),
                            );
                            if let Some(regions) =
                                c.request.as_ref().and_then(|req| req.data.as_ref())
                            {
                                for r in regions {
                                    if remaining == 0 {
                                        break;
                                    }
                                    let offset = r.offset.unwrap_or(0) as usize;
                                    let size = r.size.unwrap_or(0) as usize;
                                    let Some(avail) = mapped_out[idx].size().checked_sub(offset)
                                    else {
                                        break;
                                    };
                                    let take = remaining.min(size).min(avail);
                                    mapped_out[idx].read_slice(offset, take, &mut buf);
                                    remaining -= take;
                                }
                            }

                            // Exert backpressure if write_task is busy: awaits when tx channel is full
                            if tx.send(buf).await.is_err() {
                                return;
                            }
                            requeue_slots.push((id, idx));
                        }
                    }
                    {
                        let mut sg = sg_config_read.lock().unwrap();
                        sg.in_flight_out = sg.in_flight_out.saturating_sub(completed_count);
                        if sg.requeue_on_cancel {
                            requeue_slots.extend(canceled_slots);
                        }
                        let mut re_reqs = Vec::with_capacity(requeue_slots.len());
                        for (id, idx) in requeue_slots {
                            flush_out_vmo(&mapped_out[idx], &sg, buffer_size);
                            re_reqs.push(make_out_req(id, &sg, buffer_size));
                        }
                        let requeued = re_reqs.len();
                        if requeued > 0 && queue_requests_batch(&ep_out_clone, re_reqs).is_ok() {
                            sg.in_flight_out += requeued;
                            if sg.requeue_on_cancel {
                                let _ = requeue_tx.unbounded_send(requeued);
                            }
                        }
                    }
                }
            }
        }
    });

    let ep_in_clone = ep_in.clone();
    let count_in = mapped_in.len();
    let sg_config_write = sg_config;
    let write_task = fasync::Task::spawn(async move {
        let mut event_stream = ep_in_clone.take_event_stream();
        let mut free_slots: VecDeque<usize> = (0..count_in).collect();
        let mut zlp_slots: VecDeque<usize> = VecDeque::new();

        enum Action {
            Data(Option<Vec<u8>>),
            Event(Option<Result<fusb_endpoint::EndpointEvent, fidl::Error>>),
        }

        loop {
            let action = if free_slots.is_empty() {
                Action::Event(event_stream.next().await)
            } else {
                futures::select! {
                    data = rx.next() => Action::Data(data),
                    event = event_stream.next() => Action::Event(event),
                }
            };

            match action {
                Action::Data(Some(mut data)) => {
                    let slot = free_slots.pop_front().expect("free_slots not empty");
                    let mapped = &mapped_in[slot];
                    let vmo_id = base_in_id + slot as u64;
                    let sg = sg_config_write.lock().unwrap().clone();

                    let is_zlp = data.is_empty();
                    let req = if is_zlp {
                        zlp_slots.push_back(slot);
                        make_sg_bulk_request(vmo_id, &[], sg.in_short_zlp)
                    } else if sg.in_regions.is_empty() {
                        let write_len = std::cmp::min(data.len(), mapped.size());
                        mapped.write_slice(0, &data[..write_len]);
                        make_sg_bulk_request(
                            vmo_id,
                            &[ScatterGatherRegion { offset: 0, size: write_len as u64 }],
                            sg.in_short_zlp,
                        )
                    } else {
                        let total_in_cap: usize =
                            sg.in_regions.iter().map(|r| r.size as usize).sum();
                        let write_len = data.len().min(total_in_cap).min(mapped.size());
                        let mut active_regions = Vec::new();
                        let mut copied = 0usize;
                        for r in &sg.in_regions {
                            if copied >= write_len {
                                break;
                            }
                            let chunk = (write_len - copied).min(r.size as usize);
                            mapped.write_slice(r.offset as usize, &data[copied..copied + chunk]);
                            active_regions
                                .push(ScatterGatherRegion { offset: r.offset, size: chunk as u64 });
                            copied += chunk;
                        }
                        make_sg_bulk_request(vmo_id, &active_regions, sg.in_short_zlp)
                    };

                    // Recycle data buffer back to read_task
                    data.clear();
                    let _ = ack_tx.try_send(data);

                    match queue_requests_batch(&ep_in_clone, vec![req]) {
                        Ok(()) => {}
                        Err(e) => {
                            error!("Failed to queue request on ep_in: {:?}", e);
                            if is_zlp {
                                let _ = zlp_slots.pop_back();
                            }
                            free_slots.push_front(slot);
                            return;
                        }
                    }
                }
                Action::Data(None) => {
                    while free_slots.len() < count_in {
                        if let Some(Ok(fusb_endpoint::EndpointEvent::OnCompletion { completion })) =
                            event_stream.next().await
                        {
                            for c in completion {
                                release_in_slot(
                                    &c,
                                    base_in_id,
                                    count_in,
                                    &mut free_slots,
                                    &mut zlp_slots,
                                );
                            }
                        } else {
                            return;
                        }
                    }
                    return;
                }
                Action::Event(Some(Ok(fusb_endpoint::EndpointEvent::OnCompletion {
                    completion,
                }))) => {
                    for c in completion {
                        if let Some(status) = c.status {
                            match status {
                                zx::sys::ZX_ERR_IO_NOT_PRESENT => {
                                    warn!(
                                        "Endpoint disconnected (IO_NOT_PRESENT), stopping loopback write"
                                    );
                                    return;
                                }
                                zx::sys::ZX_OK
                                | zx::sys::ZX_ERR_CANCELED
                                | zx::sys::ZX_ERR_IO_REFUSED => {}
                                err => {
                                    warn!(
                                        "Endpoint write completed with non-OK status: {:?}",
                                        Status::err_from_raw(err)
                                    );
                                }
                            }
                        }
                        release_in_slot(&c, base_in_id, count_in, &mut free_slots, &mut zlp_slots);
                    }
                }
                Action::Event(Some(Err(_))) | Action::Event(None) => return,
            }
        }
    });

    (read_task, write_task)
}

#[cfg(test)]
mod tests;
