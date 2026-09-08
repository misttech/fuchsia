// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Goodix GT6853 Touchscreen Driver for Fuchsia.

pub mod data_types;
pub mod hardware_units;
pub mod registers;

#[cfg(test)]
pub mod testing;

use crate::hardware_units::controller::Controller;
use crate::hardware_units::i2c::MessageInterfaceUnitI2c;
use crate::registers::product_info::ProductIdentification;
use crate::registers::status::FirmwareStatus;
use fdf_component::{Driver, DriverContext, DriverError, Node, driver_register};
use fidl_next_fuchsia_hardware_gpio as fidl_gpio;
use fidl_next_fuchsia_hardware_i2c as fidl_i2c;
use std::sync::Mutex;

struct GoodixGt6853Driver {
    _node: Node,
    controller_task: Mutex<Option<fuchsia_async::Task<()>>>,
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

        let _node = context.take_node()?;

        let controller = Controller::new(i2c, gpio_int).await?;
        let controller_task = fuchsia_async::Task::spawn(controller.run());

        log::info!("goodix_gt6853 driver initialized successfully");
        Ok(Self { _node, controller_task: Mutex::new(Some(controller_task)) })
    }

    async fn stop(&self) {
        log::info!("Stopping goodix_gt6853 driver");
        let controller_task = self.controller_task.lock().unwrap().take();
        if let Some(task) = controller_task {
            task.abort().await;
        }
    }
}
