// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdf_component::{
    Driver, DriverContext, DriverError, Node, NodeBuilder, ServiceOffer, driver_register,
};
use fdf_metadata::MetadataServer;
use fidl::Serializable;

use fidl_fuchsia_hardware_platform_device as fpdev;
use fidl_fuchsia_hardware_spi_businfo as fspi_businfo;
use fidl_next::{Request, Responder, ServerEnd};
use fidl_next_fuchsia_hardware_clock as fclock;
use fidl_next_fuchsia_hardware_gpio as fgpio;
use fidl_next_fuchsia_hardware_powerdomain as fpowerdomain;
use fidl_next_fuchsia_hardware_reset as freset;
use fidl_next_fuchsia_hardware_spiimpl::{
    self, SpiImplExchangeVectorResponse, SpiImplReceiveVectorResponse, spi_impl as fspi_impl,
};
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use futures::StreamExt;
use futures::channel::mpsc;
use log::{error, info, warn};
use mmio::Register;
use mmio::region::MmioRegion;
use mmio::vmo::VmoMemory;
use pdev::{PdevExt, PlatformDevice};
use serde::Deserialize;
use zx::Status;
mod registers;

const FIFO_SIZE: usize = 256;

#[derive(Deserialize, Debug, PartialEq)]
struct DwSpiConfig {
    dw_spi_rx_sample_delay_ns: u64,
}

enum SpiImplRequest {
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

struct DwSpiDevice {
    mmio: registers::DwSpiRegsBlock<MmioRegion<VmoMemory>>,
    cs_gpio: Option<fidl_next::Client<fgpio::Gpio>>,
}

struct SpiImplServer {
    tx: mpsc::UnboundedSender<SpiImplRequest>,
}

impl DwSpiDevice {
    fn set_baud_rate(
        &mut self,
        parent_clock_hz: u64,
        max_bus_clock_hz: u64,
        rx_sample_delay_ns: u64,
    ) -> Result<(), Status> {
        let divider = {
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
            if divider % 2 == 0 { divider } else { divider + 1 }
        };

        self.mmio.baudr_mut().write({
            let mut baudr = registers::Baudr::from_raw(0);
            baudr.set_sckdv(divider as u32);
            baudr
        });

        // Convert the RX delay from nanoseconds to parent clock cycles.
        let rx_sample_delay_clocks = {
            const NS_PER_S: u64 = 1_000_000_000;

            let Some(numerator) = rx_sample_delay_ns.checked_mul(parent_clock_hz) else {
                error!(
                    "Unsupported RX delay {rx_sample_delay_ns} for parent clock rate {parent_clock_hz}"
                );
                return Err(Status::INVALID_ARGS);
            };

            let delay_clocks = numerator / NS_PER_S;
            // Verify that the clock count fits in the register, and that the conversion from
            // nanoseconds to clock cycles did not result in rounding.
            if delay_clocks > 0xff || (delay_clocks * NS_PER_S) != numerator {
                error!(
                    "Unsupported RX delay {rx_sample_delay_ns} for parent clock rate {parent_clock_hz}"
                );
                return Err(Status::INVALID_ARGS);
            }

            delay_clocks as u32
        };

        self.mmio.rx_sample_dly_mut().write({
            let mut rx_sample_dly = registers::RxSampleDly::from_raw(0);
            rx_sample_dly.set_rsd(rx_sample_delay_clocks);
            rx_sample_dly
        });

        Ok(())
    }

    fn init_registers(
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
                    error!("Failed to assert CS: {:?}", e);
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
                    error!("Failed to deassert CS: {:?}", e);
                    Status::IO
                },
            )?;
        }

        return Ok(rxdata);
    }

    async fn handle_request(&mut self, req: SpiImplRequest) {
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

impl fidl_next_fuchsia_hardware_spiimpl::SpiImplServerHandler for SpiImplServer {
    async fn get_chip_select_count(&mut self, responder: Responder<fspi_impl::GetChipSelectCount>) {
        let _ = responder.respond(1).await;
    }

    async fn transmit_vector(
        &mut self,
        request: Request<fspi_impl::TransmitVector>,
        responder: Responder<fspi_impl::TransmitVector>,
    ) {
        let payload = request.payload();
        let _ = self.tx.unbounded_send(SpiImplRequest::TransmitVector {
            chip_select: payload.chip_select,
            data: payload.data,
            responder,
        });
    }

    async fn receive_vector(
        &mut self,
        request: Request<fspi_impl::ReceiveVector>,
        responder: Responder<fspi_impl::ReceiveVector>,
    ) {
        let payload = request.payload();
        let _ = self.tx.unbounded_send(SpiImplRequest::ReceiveVector {
            chip_select: payload.chip_select,
            size: payload.size as usize,
            responder,
        });
    }

    async fn exchange_vector(
        &mut self,
        request: Request<fspi_impl::ExchangeVector>,
        responder: Responder<fspi_impl::ExchangeVector>,
    ) {
        let payload = request.payload();
        let _ = self.tx.unbounded_send(SpiImplRequest::ExchangeVector {
            chip_select: payload.chip_select,
            txdata: payload.txdata,
            responder,
        });
    }

    async fn lock_bus(
        &mut self,
        _request: Request<fspi_impl::LockBus>,
        responder: Responder<fspi_impl::LockBus>,
    ) {
        let _ = responder.respond_err(Status::NOT_SUPPORTED.into_raw()).await;
    }

    async fn unlock_bus(
        &mut self,
        _request: Request<fspi_impl::UnlockBus>,
        responder: Responder<fspi_impl::UnlockBus>,
    ) {
        let _ = responder.respond_err(Status::NOT_SUPPORTED.into_raw()).await;
    }

    async fn register_vmo(
        &mut self,
        _request: Request<fspi_impl::RegisterVmo>,
        responder: Responder<fspi_impl::RegisterVmo>,
    ) {
        let _ = responder.respond_err(Status::NOT_SUPPORTED.into_raw()).await;
    }

    async fn unregister_vmo(
        &mut self,
        _request: Request<fspi_impl::UnregisterVmo>,
        responder: Responder<fspi_impl::UnregisterVmo>,
    ) {
        let _ = responder.respond_err(Status::NOT_SUPPORTED.into_raw()).await;
    }

    async fn release_registered_vmos(
        &mut self,
        _request: Request<fspi_impl::ReleaseRegisteredVmos>,
    ) {
    }

    async fn transmit_vmo(
        &mut self,
        _request: Request<fspi_impl::TransmitVmo>,
        responder: Responder<fspi_impl::TransmitVmo>,
    ) {
        let _ = responder.respond_err(Status::NOT_SUPPORTED.into_raw()).await;
    }

    async fn receive_vmo(
        &mut self,
        _request: Request<fspi_impl::ReceiveVmo>,
        responder: Responder<fspi_impl::ReceiveVmo>,
    ) {
        let _ = responder.respond_err(Status::NOT_SUPPORTED.into_raw()).await;
    }

    async fn exchange_vmo(
        &mut self,
        _request: Request<fspi_impl::ExchangeVmo>,
        responder: Responder<fspi_impl::ExchangeVmo>,
    ) {
        let _ = responder.respond_err(Status::NOT_SUPPORTED.into_raw()).await;
    }
}

struct SpiImplService {
    scope: fasync::ScopeHandle,
    tx: mpsc::UnboundedSender<SpiImplRequest>,
}

impl fidl_next_fuchsia_hardware_spiimpl::ServiceHandler for SpiImplService {
    fn device(&self, server_end: ServerEnd<fidl_next_fuchsia_hardware_spiimpl::SpiImpl>) {
        server_end.spawn_on(SpiImplServer { tx: self.tx.clone() }, &self.scope);
    }
}

struct DwSpiDriver {
    _node: Node,
    _scope: fasync::Scope,
    _businfo_server: Option<MetadataServer>,
}

driver_register!(DwSpiDriver);

fn fidl_result_to_result<T>(
    result: Result<fidl_next::FlexibleResult<T, i32>, fidl_next::Error<Status>>,
) -> Result<T, Status> {
    match result {
        Ok(fidl_next::FlexibleResult::Ok(response)) => Ok(response),
        Ok(fidl_next::FlexibleResult::Err(e)) => Err(Status::from_raw(e)),
        _ => Err(Status::INTERNAL),
    }
}

impl DwSpiDriver {
    async fn get_max_bus_clock(pdev: &fpdev::DeviceProxy) -> u64 {
        if let Ok(metadata) = pdev.get_typed_metadata::<fspi_businfo::SpiBusMetadata>().await {
            let channels = metadata.channels.unwrap_or(vec![]);
            channels.into_iter().filter_map(|c| c.max_frequency_hz).min().unwrap_or(0) as u64
        } else {
            0
        }
    }
}

impl Driver for DwSpiDriver {
    const NAME: &str = "dw-spi";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        info!("Starting dw-spi driver");

        let pdev = context.connect_to_pdev()?;

        let powerdomain_service: fdf_component::ServiceInstance<fpowerdomain::Service> =
            context.incoming.service().instance("power-domain").connect_next()?;
        let (powerdomain_client, powerdomain_server) = fidl_next::fuchsia::create_channel();
        powerdomain_service.domain(powerdomain_server)?;
        let powerdomain = powerdomain_client.spawn();

        powerdomain.enable().await.inspect_err(|e| {
            error!("Failed to enable power domain: {:?}", e);
        })?;
        info!("Power domain enabled successfully");

        let parent_clock_hz = {
            let clock_service: fdf_component::ServiceInstance<fclock::Service> =
                context.incoming.service().instance("clock-bus").connect_next()?;
            let (clock_client, clock_server) = fidl_next::fuchsia::create_channel();
            clock_service.clock(clock_server)?;
            let clock = clock_client.spawn();

            clock.enable().await.inspect_err(|e| {
                error!("Failed to enable bus clock: {:?}", e);
            })?;

            fidl_result_to_result(clock.get_rate().await)
                .inspect_err(|e| {
                    error!("Failed to get bus clock rate: {:?}", e);
                })
                .unwrap_or(fidl_next_fuchsia_hardware_clock::ClockGetRateResponse { hz: 0 })
                .hz
        };

        {
            let clock_service: fdf_component::ServiceInstance<fclock::Service> =
                context.incoming.service().instance("clock-registers").connect_next()?;
            let (clock_client, clock_server) = fidl_next::fuchsia::create_channel();
            clock_service.clock(clock_server)?;
            let clock = clock_client.spawn();

            clock.enable().await.inspect_err(|e| {
                error!("Failed to enable registers clock: {:?}", e);
            })?;
        }

        let reset_service: fdf_component::ServiceInstance<freset::Service> =
            context.incoming.service().instance("reset").connect_next()?;
        let (reset_client, reset_server) = fidl_next::fuchsia::create_channel();
        reset_service.reset(reset_server)?;
        let reset = reset_client.spawn();

        reset.toggle().await.inspect_err(|e| {
            error!("Failed to toggle reset: {:?}", e);
        })?;

        let mmio = registers::DwSpiRegsBlock { mmio: pdev.map_mmio_by_id(0).await? };

        let cs_gpio = {
            let cs_gpio_service: fdf_component::ServiceInstance<fgpio::Service> =
                context.incoming.service().instance("gpio-cs-0").connect_next()?;
            let (cs_gpio_client, cs_gpio_server) = fidl_next::fuchsia::create_channel();
            cs_gpio_service.device(cs_gpio_server)?;

            let cs_gpio = cs_gpio_client.spawn();

            // The chip select GPIO is optional. Make a call on it do determine whether or not it
            // has been provided to us.
            match cs_gpio.release_interrupt().await {
                Ok(_) => Some(cs_gpio),
                _ => None,
            }
        };

        let mut device = DwSpiDevice { mmio, cs_gpio };

        let mut outgoing = ServiceFs::new();

        let scope = fasync::Scope::new_with_name(Self::NAME);

        let (spi_req_tx, mut spi_req_rx) = mpsc::unbounded::<SpiImplRequest>();

        let offer = ServiceOffer::<fidl_next_fuchsia_hardware_spiimpl::Service>::new_next()
            .add_default_named_next(
                &mut outgoing,
                "default",
                SpiImplService { scope: scope.to_handle(), tx: spi_req_tx },
            )
            .build_driver_offer();

        let mut node_args = NodeBuilder::new(Self::NAME).add_offer(offer);

        let businfo_server = MetadataServer::new(fspi_businfo::SpiBusMetadata::SERIALIZABLE_NAME)
            .forward_from_pdev(&pdev)
            .await;
        match businfo_server {
            Ok(ref server) => {
                if let Some(offer) = server.serve(&mut outgoing, scope.to_handle(), "default") {
                    node_args = node_args.add_offer(offer);
                }
            }
            Err(e) => {
                if e != Status::NOT_FOUND {
                    error!("Failed to forward SPI bus metadata: {e}");
                    return Err(e.into());
                }
            }
        }

        let max_bus_clock_hz = DwSpiDriver::get_max_bus_clock(&pdev).await;
        let config: DwSpiConfig = pdev
            .get_deserialized_metadata()
            .await
            .inspect_err(|e| {
                info!("dw-spi config was not provided ({:?})", e);
            })
            .unwrap_or(DwSpiConfig { dw_spi_rx_sample_delay_ns: 0 });

        device.init_registers(
            parent_clock_hz,
            max_bus_clock_hz,
            config.dw_spi_rx_sample_delay_ns,
        )?;

        let node = context.take_node()?;
        node.add_child(node_args.build()).await?;

        context.serve_outgoing(&mut outgoing)?;
        scope.spawn(outgoing.collect());

        scope.spawn_local(async move {
            while let Some(req) = spi_req_rx.next().await {
                device.handle_request(req).await;
            }
        });

        info!("dw-spi driver initialized successfully");

        Ok(Self { _node: node, _scope: scope, _businfo_server: businfo_server.ok() })
    }

    async fn stop(&self) {}
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
