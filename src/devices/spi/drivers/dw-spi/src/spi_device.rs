// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_next::Responder;
use fidl_next_fuchsia_hardware_gpio as fgpio;
use fidl_next_fuchsia_hardware_spiimpl::{
    self, SpiImplExchangeVectorResponse, SpiImplReceiveVectorResponse, spi_impl as fspi_impl,
};
use log::{error, warn};
use mmio::Register;
use mmio::region::MmioRegion;
use mmio::vmo::VmoMemory;
use std::time::Duration;
mod registers;
use registers::DwSpiRegsBlock;
use zx::Status;

const FIFO_SIZE: usize = 256;

pub enum SpiImplRequest {
    TransmitVector {
        chip_select: u32,
        data: Vec<u8>,
        responder: Responder<fspi_impl::TransmitVector>,
    },
    ReceiveVector {
        chip_select: u32,
        size: usize,
        responder: Responder<fspi_impl::ReceiveVector>,
    },
    ExchangeVector {
        chip_select: u32,
        txdata: Vec<u8>,
        responder: Responder<fspi_impl::ExchangeVector>,
    },
}

pub struct DwSpiDevice {
    mmio: DwSpiRegsBlock<MmioRegion<VmoMemory>>,
    cs_gpio: Option<fidl_next::Client<fgpio::Gpio>>,
}

impl DwSpiDevice {
    pub fn new(
        mmio: MmioRegion<VmoMemory>,
        cs_gpio: Option<fidl_next::Client<fgpio::Gpio>>,
    ) -> Self {
        DwSpiDevice { mmio: DwSpiRegsBlock { mmio }, cs_gpio }
    }

    fn set_baud_rate(
        &mut self,
        parent_clock_hz: u64,
        max_bus_clock_hz: u64,
        rx_sample_delay_ns: u64,
    ) -> Result<(), Status> {
        // Round the divider up to avoid overclocking.
        let Some(numerator) = parent_clock_hz.checked_add(max_bus_clock_hz - 1) else {
            error!(
                "Unsupported max bus clock {max_bus_clock_hz} for parent clock rate {parent_clock_hz}"
            );
            return Err(Status::INVALID_ARGS);
        };

        let divider = numerator / max_bus_clock_hz;
        if divider >= 0xffff {
            error!(
                "Unsupported max bus clock {max_bus_clock_hz} for parent clock rate {parent_clock_hz}"
            );
            return Err(Status::INVALID_ARGS);
        }

        // The divider must be even.
        let divider = if divider % 2 == 0 { divider } else { divider + 1 };

        self.mmio.baudr_mut().write({
            let mut baudr = registers::Baudr::from_raw(0);
            baudr.set_sckdv(divider as u32);
            baudr
        });

        // Convert the RX delay from nanoseconds to parent clock cycles.
        let Some(numerator) = rx_sample_delay_ns.checked_mul(parent_clock_hz) else {
            error!(
                "Unsupported RX delay {rx_sample_delay_ns} for parent clock rate {parent_clock_hz}"
            );
            return Err(Status::INVALID_ARGS);
        };

        let rx_sample_delay = Duration::from_nanos(numerator);
        let rx_sample_delay_clocks = rx_sample_delay.as_secs();
        // Verify that the clock count fits in the register, and that the conversion from
        // nanoseconds to clock cycles did not result in rounding.
        if rx_sample_delay_clocks > 0xff || rx_sample_delay.subsec_nanos() != 0 {
            error!(
                "Unsupported RX delay {rx_sample_delay_ns} for parent clock rate {parent_clock_hz}"
            );
            return Err(Status::INVALID_ARGS);
        }

        self.mmio.rx_sample_dly_mut().write({
            let mut rx_sample_dly = registers::RxSampleDly::from_raw(0);
            rx_sample_dly.set_rsd(rx_sample_delay_clocks as u32);
            rx_sample_dly
        });

        Ok(())
    }

    pub fn init_registers(
        &mut self,
        parent_clock_hz: u64,
        max_bus_clock_hz: u64,
        rx_sample_delay_ns: u64,
    ) -> Result<(), Status> {
        self.mmio.ssi_enr_mut().write(registers::SsiEnr::from_raw(0));

        self.mmio.ctrlr0_mut().write({
            let mut ctrlr0 = registers::CtrlR0::from_raw(0);
            ctrlr0.set_spi_frf(0); // Standard SPI
            ctrlr0.set_frf(0); // Motorola SPI
            ctrlr0.set_dfs(7); // 8-bit (values 3-15 correspond to 4-16 bits, so 7 means 8 bits)
            ctrlr0.set_tmod(0); // Transmit & Receive
            ctrlr0
        });

        if max_bus_clock_hz > 0 {
            self.set_baud_rate(parent_clock_hz, max_bus_clock_hz, rx_sample_delay_ns)?;
        } else {
            warn!("Max bus clock rate reported to be zero, skipping baud rate initialization");
        }

        // Mask all interrupts initially in IMR
        self.mmio.imr_mut().write(registers::Imr::from_raw(0));

        // Enable SSI
        self.mmio.ssi_enr_mut().write({
            let mut ssi_enr = registers::SsiEnr::from_raw(0);
            ssi_enr.set_ssi_en(true);
            ssi_enr
        });

        Ok(())
    }

    async fn exchange_pio(
        &mut self,
        chip_select: u32,
        mut txdata: &[u8],
        rx: bool,
        mut size: usize,
    ) -> Result<Vec<u8>, Status> {
        if size == 0 {
            return Ok(vec![]);
        }

        assert!(txdata.len() > 0 || rx); // If there is no TX data then we must be receiving.
        assert!(txdata.len() == 0 || txdata.len() == size); // TX size must match RX size.

        // Only one chip select is supported for now.
        if chip_select != 0 {
            return Err(Status::NOT_FOUND);
        }

        if let Some(cs_gpio) = &self.cs_gpio {
            cs_gpio.set_buffer_mode(fgpio::natural::BufferMode::OutputLow).wire().await.map_err(
                |e| {
                    error!("Failed to assert CS: {e}");
                    Status::IO
                },
            )?;
        }

        // TODO(https://fxbug.dev/500865936): Support DMA transfers for larger sizes.
        // This is a placeholder indicating where DMA support would be added.
        // For now, we only implement PIO.

        // A target must be selected before the transfer can begin.
        self.mmio.ser_mut().write({
            let mut ser = registers::Ser::from_raw(0);
            ser.set_ser(1);
            ser
        });

        let mut rxdata = Vec::<u8>::with_capacity(if rx { size } else { 0 });

        while size > 0 {
            if self.mmio.sr().read().rfne() {
                warn!("RX FIFO is not empty before starting transfer");
            }

            // Wait for the TX FIFO to be empty.
            while !self.mmio.sr().read().tfe() {}

            let transfer_size = std::cmp::min(size, FIFO_SIZE);

            // Fill the TX FIFO up to available space or remaining data.
            for i in 0..transfer_size {
                let data = if txdata.len() > 0 { txdata[i] } else { 0xFF };
                self.mmio.dr0_mut().write(registers::Dr0::from_raw(data as u32));
            }

            // Read the RX FIFO for the bytes we just sent.
            for _ in 0..transfer_size {
                // Wait for at least one byte to be in the RX FIFO.
                while !self.mmio.sr().read().rfne() {}

                let data = self.mmio.dr0().read().dr() as u8;
                if rx {
                    rxdata.push(data);
                }
            }

            size -= transfer_size;
            if txdata.len() > 0 {
                txdata = &txdata[transfer_size..];
            }
        }

        self.mmio.ser_mut().write(registers::Ser::from_raw(0));

        if let Some(cs_gpio) = &self.cs_gpio {
            cs_gpio.set_buffer_mode(fgpio::natural::BufferMode::OutputHigh).wire().await.map_err(
                |e| {
                    error!("Failed to deassert CS: {e}");
                    Status::IO
                },
            )?;
        }

        return Ok(rxdata);
    }

    pub async fn handle_request(&mut self, req: SpiImplRequest) {
        match req {
            SpiImplRequest::TransmitVector { chip_select, data, responder } => {
                let result = self
                    .exchange_pio(chip_select, &data, false, data.len())
                    .await
                    .map(|_| ())
                    .map_err(|e| e.into_raw());
                let _ = responder.respond_with(result).await;
            }
            SpiImplRequest::ReceiveVector { chip_select, size, responder } => {
                let result = self
                    .exchange_pio(chip_select, &[], true, size)
                    .await
                    .map(|data| SpiImplReceiveVectorResponse { data })
                    .map_err(|e| e.into_raw());
                let _ = responder.respond_with(result).await;
            }
            SpiImplRequest::ExchangeVector { chip_select, txdata, responder } => {
                let result = self
                    .exchange_pio(chip_select, &txdata, true, txdata.len())
                    .await
                    .map(|rxdata| SpiImplExchangeVectorResponse { rxdata })
                    .map_err(|e| e.into_raw());
                let _ = responder.respond_with(result).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mmio::vmo::VmoMapping;
    use zx::Vmo;

    #[test]
    fn test_set_baud_rate_and_delay() {
        let vmo = Vmo::create(0x100).expect("Failed to create VMO");
        let mmio = VmoMapping::map(0, 0x100, vmo).expect("Failed to map VMO");
        let mut device = DwSpiDevice::new(mmio, None);

        device.set_baud_rate(200_000_000, 20_000_000, 25).unwrap();

        assert_eq!(device.mmio.baudr().read().sckdv(), 10);
        assert_eq!(device.mmio.rx_sample_dly().read().rsd(), 5);
    }

    #[test]
    fn test_set_baud_rate_too_slow() {
        let vmo = Vmo::create(0x100).expect("Failed to create VMO");
        let mmio = VmoMapping::map(0, 0x100, vmo).expect("Failed to map VMO");
        let mut device = DwSpiDevice::new(mmio, None);

        let result = device.set_baud_rate(200_000_000, 2_000, 0);
        assert_eq!(result.unwrap_err(), Status::INVALID_ARGS);
    }

    #[test]
    fn test_set_baud_divider_rounded_up() {
        let vmo = Vmo::create(0x100).expect("Failed to create VMO");
        let mmio = VmoMapping::map(0, 0x100, vmo).expect("Failed to map VMO");
        let mut device = DwSpiDevice::new(mmio, None);

        device.set_baud_rate(200_000_000, 3_600_000, 0).unwrap();

        assert_eq!(device.mmio.baudr().read().sckdv(), 56); // Divider rounded up to 56.
        assert_eq!(device.mmio.rx_sample_dly().read().rsd(), 0);
    }

    #[test]
    fn test_set_baud_rate_invalid_delay_remainder() {
        let vmo = Vmo::create(0x100).expect("Failed to create VMO");
        let mmio = VmoMapping::map(0, 0x100, vmo).expect("Failed to map VMO");
        let mut device = DwSpiDevice::new(mmio, None);

        let result = device.set_baud_rate(200_000_000, 20_000_000, 28);
        assert_eq!(result.unwrap_err(), Status::INVALID_ARGS);
    }

    #[test]
    fn test_set_baud_rate_invalid_delay_too_large() {
        let vmo = Vmo::create(0x100).expect("Failed to create VMO");
        let mmio = VmoMapping::map(0, 0x100, vmo).expect("Failed to map VMO");
        let mut device = DwSpiDevice::new(mmio, None);

        let result = device.set_baud_rate(200_000_000, 20_000_000, 5000);
        assert_eq!(result.unwrap_err(), Status::INVALID_ARGS);
    }
}
