// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdf_component::{Driver, DriverContext, Node, NodeBuilder, ServiceOffer, driver_register};
use fdf_metadata::MetadataServer;
use fidl::Serializable;
use fidl_fuchsia_hardware_platform_device as fpdev;
use fidl_fuchsia_hardware_spi_businfo as fspi_businfo;
use fidl_next::{Request, Responder, ServerEnd};
use fidl_next_fuchsia_hardware_clock as fclock;
use fidl_next_fuchsia_hardware_gpio as fgpio;
use fidl_next_fuchsia_hardware_powerdomain as fpowerdomain;
use fidl_next_fuchsia_hardware_reset as freset;
use fidl_next_fuchsia_hardware_spiimpl::{self, spi_impl as fspi_impl};
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use futures::StreamExt;
use log::{error, info};
use mmio::region::MmioRegion;
use mmio::vmo::VmoMemory;
use pdev::PlatformDevice;
use zx::Status;

struct DwSpiDriver {
    _node: Node,
    _scope: fasync::Scope,
    _businfo_server: Option<MetadataServer>,
    _mmio: MmioRegion<VmoMemory>,
    _cs_gpio: Option<fidl_next::Client<fgpio::Gpio>>,
}

driver_register!(DwSpiDriver);

struct SpiImplServer {}

impl fidl_next_fuchsia_hardware_spiimpl::SpiImplServerHandler for SpiImplServer {
    async fn get_chip_select_count(&mut self, responder: Responder<fspi_impl::GetChipSelectCount>) {
        let _ = responder.respond(1).await;
    }

    async fn transmit_vector(
        &mut self,
        _request: Request<fspi_impl::TransmitVector>,
        responder: Responder<fspi_impl::TransmitVector>,
    ) {
        let _ = responder.respond_err(Status::NOT_SUPPORTED.into_raw()).await;
    }

    async fn receive_vector(
        &mut self,
        _request: Request<fspi_impl::ReceiveVector>,
        responder: Responder<fspi_impl::ReceiveVector>,
    ) {
        let _ = responder.respond_err(Status::NOT_SUPPORTED.into_raw()).await;
    }

    async fn exchange_vector(
        &mut self,
        _request: Request<fspi_impl::ExchangeVector>,
        responder: Responder<fspi_impl::ExchangeVector>,
    ) {
        let _ = responder.respond_err(Status::NOT_SUPPORTED.into_raw()).await;
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
}

impl fidl_next_fuchsia_hardware_spiimpl::ServiceHandler for SpiImplService {
    fn device(&self, server_end: ServerEnd<fidl_next_fuchsia_hardware_spiimpl::SpiImpl>) {
        server_end.spawn_on(SpiImplServer {}, &self.scope);
    }
}

impl Driver for DwSpiDriver {
    const NAME: &str = "dw-spi";

    async fn start(mut context: DriverContext) -> Result<Self, Status> {
        info!("Starting dw-spi driver");

        let pdev: fpdev::DeviceProxy = context
            .incoming
            .service_marker(fpdev::ServiceMarker)
            .connect()?
            .connect_to_device()
            .map_err(|err| {
                error!("Failed to connect to platform device: {err}");
                Status::INTERNAL
            })?;

        let powerdomain_service: fdf_component::ServiceInstance<fpowerdomain::Service> =
            context.incoming.service().instance("power-domain").connect_next()?;
        let (powerdomain_client, powerdomain_server) = fidl_next::fuchsia::create_channel();
        powerdomain_service.domain(powerdomain_server).map_err(|_| Status::INTERNAL)?;
        let powerdomain = powerdomain_client.spawn();

        powerdomain.enable().await.map_err(|e| {
            error!("Failed to enable power domain: {:?}", e);
            Status::INTERNAL
        })?;
        info!("Power domain enabled successfully");

        const CLOCK_NAMES: [&str; 2] = ["clock-bus", "clock-registers"];
        for name in CLOCK_NAMES {
            let clock_service: fdf_component::ServiceInstance<fclock::Service> =
                context.incoming.service().instance(name).connect_next()?;
            let (clock_client, clock_server) = fidl_next::fuchsia::create_channel();
            clock_service.clock(clock_server).map_err(|_| Status::INTERNAL)?;
            let clock = clock_client.spawn();

            clock.enable().await.map_err(|e| {
                error!("Failed to enable clock: {:?}", e);
                Status::INTERNAL
            })?;
        }

        let reset_service: fdf_component::ServiceInstance<freset::Service> =
            context.incoming.service().instance("reset").connect_next()?;
        let (reset_client, reset_server) = fidl_next::fuchsia::create_channel();
        reset_service.reset(reset_server).map_err(|_| Status::INTERNAL)?;
        let reset = reset_client.spawn();

        reset.toggle().await.map_err(|e| {
            error!("Failed to toggle reset: {:?}", e);
            Status::INTERNAL
        })?;

        let mmio = pdev.map_mmio_by_id(0).await?;

        let cs_gpio = {
            let cs_gpio_service: fdf_component::ServiceInstance<fgpio::Service> =
                context.incoming.service().instance("cs-gpio-0").connect_next()?;
            let (cs_gpio_client, cs_gpio_server) = fidl_next::fuchsia::create_channel();
            cs_gpio_service.device(cs_gpio_server).map_err(|_| Status::INTERNAL)?;

            let cs_gpio = cs_gpio_client.spawn();

            // The chip select GPIO is optional. Make a call on it do determine whether or not it
            // has been provided to us.
            match cs_gpio.release_interrupt().await {
                Ok(Ok(_)) => Some(cs_gpio),
                _ => None,
            }
        };

        let mut outgoing = ServiceFs::new();

        let scope = fasync::Scope::new_with_name(Self::NAME);

        let offer = ServiceOffer::<fidl_next_fuchsia_hardware_spiimpl::Service>::new_next()
            .add_default_named_next(
                &mut outgoing,
                "default",
                SpiImplService { scope: scope.to_handle() },
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
                if e != zx::Status::NOT_FOUND {
                    error!("Failed to forward SPI bus metadata: {e}");
                    return Err(e);
                }
            }
        }

        let node = context.take_node()?;
        node.add_child(node_args.build()).await?;

        context.serve_outgoing(&mut outgoing)?;
        scope.spawn(outgoing.collect());

        info!("dw-spi driver initialized successfully");

        Ok(Self {
            _node: node,
            _scope: scope,
            _businfo_server: businfo_server.ok(),
            _mmio: mmio,
            _cs_gpio: cs_gpio,
        })
    }

    async fn stop(&self) {}
}

#[cfg(test)]
#[path = "../tests/dw_spi_test.rs"]
mod tests;
