// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdf_component::{
    Driver, DriverContext, DriverError, Node, NodeBuilder, ServiceOffer, driver_register,
};
use fdf_metadata::MetadataServer;
use fidl::Serializable;
use fspi_businfo::SpiBusMetadata;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use futures::StreamExt;
use futures::channel::mpsc;
use log::{error, info};
use pdev::{PdevExt, PlatformDevice};
use serde::Deserialize;
use zx::Status;

use fdf_power::PowerExt;
use fdf_resource::{ClockExt, GpioExt, ResetExt};

use fidl_next::{Request, Responder, ServerEnd};
use fidl_next_fuchsia_hardware_spiimpl::{self, spi_impl as fspi_impl};

use fidl_fuchsia_hardware_platform_device as fpdev;

use fidl_fuchsia_hardware_spi_businfo as fspi_businfo;
use fidl_next_fuchsia_hardware_clock::ClockGetRateResponse;

use anyhow::Context;

mod spi_device;
use spi_device::{DwSpiDevice, SpiImplRequest};

#[derive(Deserialize, Debug, PartialEq)]
struct DwSpiConfig {
    dw_spi_rx_sample_delay_ns: u64,
}

struct SpiImplServer {
    tx: mpsc::UnboundedSender<SpiImplRequest>,
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

impl DwSpiDriver {
    async fn get_max_bus_clock(pdev: &fpdev::DeviceProxy) -> u64 {
        if let Ok(metadata) = pdev.get_typed_metadata::<SpiBusMetadata>().await {
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
        let powerdomain = context.connect_to_powerdomain("power-domain")?;
        powerdomain.enable().await.context("Failed to enable power domain")?;

        let clock_bus = context.connect_to_clock("clock-bus")?;
        clock_bus.enable().await.context("Failed to enable bus clock")?;

        let parent_clock_hz = match clock_bus.get_rate().await {
            Ok(fidl_next::FlexibleResult::Ok(response)) => Ok(response),
            Ok(fidl_next::FlexibleResult::Err(e)) => Err(Status::from_raw(e)),
            _ => Err(Status::INTERNAL),
        };
        let parent_clock_hz = parent_clock_hz
            .inspect_err(|e| {
                error!("Failed to get bus clock rate: {e}");
            })
            .unwrap_or(ClockGetRateResponse { hz: 0 })
            .hz;

        let clock_regs = context.connect_to_clock("clock-registers")?;
        clock_regs.enable().await.context("Failed to enable registers clock")?;

        let reset = context.connect_to_reset("reset")?;
        reset.toggle().await.context("Failed to toggle reset")?;

        let cs_gpio = {
            let cs_gpio = context.connect_to_gpio("gpio-cs-0")?;

            // The chip select GPIO is optional. Make a call on it do determine whether or not it
            // has been provided to us.
            match cs_gpio.release_interrupt().await {
                Ok(_) => Some(cs_gpio),
                _ => None,
            }
        };

        let pdev = context.connect_to_pdev()?;
        let mut device = DwSpiDevice::new(pdev.map_mmio_by_id(0).await?, cs_gpio);

        let max_bus_clock_hz = DwSpiDriver::get_max_bus_clock(&pdev).await;
        let config: DwSpiConfig = pdev
            .get_deserialized_metadata()
            .await
            .inspect_err(|e| {
                info!("dw-spi config was not provided ({e})");
            })
            .unwrap_or(DwSpiConfig { dw_spi_rx_sample_delay_ns: 0 });

        device.init_registers(
            parent_clock_hz,
            max_bus_clock_hz,
            config.dw_spi_rx_sample_delay_ns,
        )?;

        let mut outgoing = ServiceFs::new();

        let scope = fasync::Scope::new_with_name(Self::NAME);

        let (spi_req_tx, mut spi_req_rx) = mpsc::unbounded();

        let offer = ServiceOffer::<fidl_next_fuchsia_hardware_spiimpl::Service>::new_next()
            .add_default_named_next(
                &mut outgoing,
                "default",
                SpiImplService { scope: scope.to_handle(), tx: spi_req_tx },
            )
            .build_driver_offer();

        let mut node_args = NodeBuilder::new(Self::NAME).add_offer(offer);

        let businfo_server =
            MetadataServer::new(SpiBusMetadata::SERIALIZABLE_NAME).forward_from_pdev(&pdev).await;
        match businfo_server {
            Ok(ref server) => {
                if let Some(offer) = server.serve(&mut outgoing, scope.to_handle(), "default") {
                    node_args = node_args.add_offer(offer);
                }
            }
            // Metadata must either be provided or not provided; other results are considered fatal
            // errors.
            Err(e) if e == Status::NOT_FOUND => {}
            Err(e) => {
                error!("Failed to forward SPI bus metadata: {e}");
                return Err(e.into());
            }
        }

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
mod tests;
