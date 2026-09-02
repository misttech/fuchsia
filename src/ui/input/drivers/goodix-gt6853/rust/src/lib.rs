// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Goodix GT6853 Touchscreen Driver for Fuchsia.

pub mod data_types;
pub mod hardware_units;
pub mod registers;

#[cfg(test)]
pub mod testing;

use crate::hardware_units::i2c::MessageInterfaceUnitI2c;
use crate::registers::product_info::ProductIdentification;
use crate::registers::status::FirmwareStatus;
use fdf_component::{Driver, DriverContext, DriverError, Node, driver_register};
use fidl_next_fuchsia_hardware_i2c as fidl_i2c;

struct GoodixGt6853Driver {
    _node: Node,
    _i2c: MessageInterfaceUnitI2c,
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
            .map_err(|e| {
                log::error!("Failed to connect to I2C service instance: {:?}", e);
                zx::Status::INTERNAL
            })?;
        let (i2c_client_end, i2c_server_end) = fidl_next::fuchsia::create_channel();
        i2c_service.device(i2c_server_end).map_err(|e| {
            log::error!("Failed to route I2C device endpoint: {:?}", e);
            zx::Status::INTERNAL
        })?;
        let i2c_client = i2c_client_end.spawn();
        Ok(MessageInterfaceUnitI2c::new(i2c_client))
    }

    /// Verifies hardware communication and firmware health on startup.
    ///
    /// Returns an error if the touch IC is unresponsive, identification checksum
    /// is invalid, or the firmware reports an unhealthy state.
    async fn verify_hardware(i2c: &MessageInterfaceUnitI2c) -> Result<(), zx::Status> {
        let prod_info = i2c.read_reg::<ProductIdentification>().await.map_err(|status| {
            log::error!("Failed to read ProductIdentification: {:?}", status);
            status
        })?;

        if !prod_info.is_checksum_valid() {
            log::error!("ProductIdentification checksum mismatch");
            return Err(zx::Status::IO_DATA_INTEGRITY);
        }

        log::info!(
            "Product identification: mask_name={:?}, patch_name={:?}, version={}",
            prod_info.mask_product_id_as_str().unwrap_or("<invalid utf8>"),
            prod_info.patch_product_id_as_str().unwrap_or("<invalid utf8>"),
            prod_info.patch_firmware_version()
        );

        let fw_status = i2c.read_reg::<FirmwareStatus>().await.map_err(|status| {
            log::error!("Failed to read FirmwareStatus: {:?}", status);
            status
        })?;

        if fw_status != FirmwareStatus::HEALTHY {
            log::error!("Firmware is not healthy: status_word=0x{:08x}", fw_status.status_word());
            return Err(zx::Status::BAD_STATE);
        }

        log::info!("Firmware status: status_word=0x{:08x} (healthy)", fw_status.status_word());

        Ok(())
    }
}

impl Driver for GoodixGt6853Driver {
    const NAME: &str = "goodix-gt6853";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        log::info!("Starting goodix_gt6853 driver");

        let i2c = Self::connect_i2c(&context)?;
        Self::verify_hardware(&i2c).await?;

        let _node = context.take_node()?;

        log::info!("goodix_gt6853 driver initialized successfully");
        Ok(Self { _node, _i2c: i2c })
    }

    async fn stop(&self) {
        log::info!("Stopping goodix_gt6853 driver");
    }
}
