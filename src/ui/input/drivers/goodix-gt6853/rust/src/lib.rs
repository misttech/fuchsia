// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Goodix GT6853 Touchscreen Driver for Fuchsia.

pub mod data_types;
pub mod hardware_integration;
pub mod hardware_units;
pub mod registers;

#[cfg(test)]
pub mod testing;

use crate::hardware_integration::descriptors;
use crate::hardware_integration::fidl_input_device::{FidlInputDevice, InputDeviceServiceHandler};
use crate::hardware_units::controller::Controller;
use crate::hardware_units::i2c::MessageInterfaceUnitI2c;
use crate::registers::product_info::ProductIdentification;
use crate::registers::status::FirmwareStatus;
use fdf_component::{
    Driver, DriverContext, DriverError, Node, NodeBuilder, ServiceOffer, driver_register,
};
use fidl_fuchsia_driver_framework::NodeControllerMarker;
use fidl_next_fuchsia_hardware_gpio as fidl_gpio;
use fidl_next_fuchsia_hardware_i2c as fidl_i2c;
use fidl_next_fuchsia_input_report as fidl_input_report;
use fuchsia_component::server::ServiceFs;
use futures::StreamExt;
use std::sync::{Arc, Mutex};

struct GoodixGt6853Driver {
    #[expect(unused)]
    parent_node: Node,
    #[expect(unused)]
    child_node: fidl::endpoints::ClientEnd<NodeControllerMarker>,
    #[expect(unused)]
    fidl_input_device: FidlInputDevice,
    controller_task: Mutex<Option<fuchsia_async::Task<()>>>,
    fidl_service_task: Mutex<Option<fuchsia_async::Task<()>>>,
}

driver_register!(GoodixGt6853Driver);

impl GoodixGt6853Driver {
    /// Obtains the I2C transport from the incoming namespace.
    fn connect_i2c(context: &DriverContext) -> Result<MessageInterfaceUnitI2c, zx::Status> {
        let i2c_service = context
            .incoming
            .service::<fdf_component::ServiceInstance<fidl_i2c::Service>>()
            .instance("i2c")
            .connect_next()
            .map_err(|error| {
                log::error!("Failed to connect to I2C service instance: {error:?}");
                zx::Status::INTERNAL
            })?;
        let (i2c_client_end, i2c_server_end) = fidl_next::fuchsia::create_channel();
        i2c_service.device(i2c_server_end).map_err(|error| {
            log::error!("Failed to route I2C device endpoint: {error:?}");
            zx::Status::INTERNAL
        })?;
        let i2c_client = i2c_client_end.spawn();
        Ok(MessageInterfaceUnitI2c::new(i2c_client))
    }

    /// Obtains a GPIO transport for the given composite child node from the incoming namespace.
    fn connect_gpio(
        context: &DriverContext,
        instance_name: &str,
    ) -> Result<fidl_next::Client<fidl_gpio::Gpio>, zx::Status> {
        let gpio_service = context
            .incoming
            .service::<fdf_component::ServiceInstance<fidl_gpio::Service>>()
            .instance(instance_name)
            .connect_next()
            .map_err(|error| {
                log::error!("Failed to connect to {instance_name} service instance: {error:?}");
                zx::Status::INTERNAL
            })?;
        let (gpio_client_end, gpio_server_end) = fidl_next::fuchsia::create_channel();
        gpio_service.device(gpio_server_end).map_err(|error| {
            log::error!("Failed to route {instance_name} device endpoint: {error:?}");
            zx::Status::INTERNAL
        })?;
        Ok(gpio_client_end.spawn())
    }

    /// Verifies hardware communication and firmware health on startup.
    ///
    /// Returns an error if the touch IC is unresponsive, identification checksum
    /// is invalid, or the firmware reports an unhealthy state.
    async fn verify_hardware(i2c: &MessageInterfaceUnitI2c) -> Result<(), zx::Status> {
        let product_info = i2c.read_reg::<ProductIdentification>().await.map_err(|status| {
            log::error!("Failed to read ProductIdentification: {status:?}");
            status
        })?;

        if !product_info.is_checksum_valid() {
            log::error!("ProductIdentification checksum mismatch");
            return Err(zx::Status::IO_DATA_INTEGRITY);
        }

        log::info!(
            "Product identification: mask_name={:?}, patch_name={:?}, version={}",
            product_info.mask_product_id_as_str().unwrap_or("<invalid utf8>"),
            product_info.patch_product_id_as_str().unwrap_or("<invalid utf8>"),
            product_info.patch_firmware_version()
        );

        let firmware_status = i2c.read_reg::<FirmwareStatus>().await.map_err(|status| {
            log::error!("Failed to read FirmwareStatus: {status:?}");
            status
        })?;

        if firmware_status != FirmwareStatus::HEALTHY {
            log::error!(
                "Firmware is not healthy: status_word=0x{:08x}",
                firmware_status.status_word()
            );
            return Err(zx::Status::BAD_STATE);
        }

        log::info!(
            "Firmware status: status_word=0x{:08x} (healthy)",
            firmware_status.status_word()
        );

        Ok(())
    }
}

impl Driver for GoodixGt6853Driver {
    const NAME: &str = "goodix-gt6853";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        log::info!("Starting goodix_gt6853 driver");

        let i2c = Self::connect_i2c(&context)?;
        Self::verify_hardware(&i2c).await?;

        let gpio_int = Self::connect_gpio(&context, "gpio-int")?;

        let descriptor = descriptors::make_device_descriptor();
        let fidl_input_device = FidlInputDevice::new(descriptor);

        let mut outgoing_fs = ServiceFs::new();
        let service_handler =
            InputDeviceServiceHandler { fidl_input_device: fidl_input_device.clone() };
        let offer = ServiceOffer::<fidl_input_report::Service>::new_next().add_default_named_next(
            &mut outgoing_fs,
            "default",
            service_handler,
        );
        context.serve_outgoing(&mut outgoing_fs)?;
        let fidl_service_task = fuchsia_async::Task::spawn(outgoing_fs.collect());

        let node_args =
            NodeBuilder::new(Self::NAME).add_offer(offer.build_zircon_offer_next()).build();

        let parent_node = context.take_node()?;
        let child_node = parent_node.add_child(node_args).await?;

        let mut controller = Controller::new(i2c, gpio_int).await?;
        let fidl_input_device_clone = fidl_input_device.clone();
        controller.set_touch_report_handler(Arc::new(move |contacts, event_time| {
            fidl_input_device_clone.handle_touch_report(contacts, event_time);
        }));
        let controller_task = fuchsia_async::Task::spawn(controller.run());

        log::info!("goodix_gt6853 driver initialized successfully");
        Ok(Self {
            parent_node,
            child_node,
            fidl_input_device,
            controller_task: Mutex::new(Some(controller_task)),
            fidl_service_task: Mutex::new(Some(fidl_service_task)),
        })
    }

    async fn stop(&self) {
        log::info!("Stopping goodix_gt6853 driver");
        let controller_task = self.controller_task.lock().unwrap().take();
        if let Some(task) = controller_task {
            task.abort().await;
        }
        let fidl_service_task = self.fidl_service_task.lock().unwrap().take();
        if let Some(task) = fidl_service_task {
            task.abort().await;
        }
    }
}
