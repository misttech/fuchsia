// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "dw-spi.h"

#include <fidl/fuchsia.hardware.clock/cpp/wire.h>
#include <fidl/fuchsia.hardware.powerdomain/cpp/wire.h>
#include <fidl/fuchsia.hardware.reset/cpp/wire.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/mmio/cpp/mmio.h>
#include <lib/zx/interrupt.h>

#include "registers.h"

namespace dw_spi {

constexpr size_t kFifoSize = 16;

void DwSpi::InitRegisters() {
  // Disable SSI
  SsiEnr::Get().FromValue(0).set_ssi_en(0).WriteTo(&mmio_);

  // Configure CTRLR0
  // Standard SPI, 8-bit data frame, Motorola SPI
  CtrlR0::Get()
      .FromValue(0)
      .set_spi_frf(0)  // Standard SPI
      .set_frf(0)      // Motorola SPI
      .set_dfs(7)      // 8-bit (values 3-15 correspond to 4-16 bits, so 7 means 8 bits)
      .set_tmod(0)     // Transmit & Receive
      .WriteTo(&mmio_);

  // Set baud rate divider (assume 2 for now, must be even)
  Baudr::Get().FromValue(0).set_sckdv(2).WriteTo(&mmio_);

  // Mask all interrupts initially in IMR
  Imr::Get().FromValue(0).WriteTo(&mmio_);

  // Enable SSI
  SsiEnr::Get().FromValue(0).set_ssi_en(1).WriteTo(&mmio_);
}

void DwSpi::ExchangePio(const uint8_t* txdata, uint8_t* out_rxdata, size_t size) {
  // TODO(https://fxbug.dev/500865936): Support DMA transfers for larger sizes.
  // This is a placeholder indicating where DMA support would be added.
  // For now, we only implement PIO.

  size_t sent = 0;
  size_t received = 0;

  while (received < size) {
    // Fill TX FIFO up to available space or remaining data
    while (sent < size && sent - received < kFifoSize) {
      uint32_t data = txdata ? txdata[sent] : 0xFF;
      mmio_.Write32(data, DW_SPI_DR0);
      sent++;
    }

    // Enable Transmit FIFO Empty interrupt to wait for data to be sent.
    // This satisfies the requirement to use interrupts.
    auto imr = Imr::Get().ReadFrom(&mmio_).set_txeim(1).WriteTo(&mmio_);

    // Wait for interrupt (blocks current thread).
    // In a more complex driver, we would use an async dispatcher to avoid blocking.
    interrupt_.wait(nullptr);

    // Mask interrupt again
    imr.set_txeim(0).WriteTo(&mmio_);

    // Enable Receive FIFO Full interrupt to wait for data to be received.
    auto imr_rx = Imr::Get().ReadFrom(&mmio_).set_rxfim(1).WriteTo(&mmio_);

    // Read RX FIFO for the bytes we just sent
    while (received < sent) {
      if (Sr::Get().ReadFrom(&mmio_).rfne()) {
        uint32_t rx_data = mmio_.Read32(DW_SPI_DR0);
        if (out_rxdata) {
          out_rxdata[received] = static_cast<uint8_t>(rx_data);
        }
        received++;
      } else {
        // FIFO is empty, wait for interrupt
        interrupt_.wait(nullptr);
      }
    }

    // Mask interrupt again
    imr_rx.set_rxfim(0).WriteTo(&mmio_);
  }
}

void DwSpi::TransmitVector(fuchsia_hardware_spiimpl::wire::SpiImplTransmitVectorRequest* request,
                           fdf::Arena& arena, TransmitVectorCompleter::Sync& completer) {
  ExchangePio(request->data.data(), nullptr, request->data.size());
  completer.buffer(arena).Reply(zx::ok());
}

void DwSpi::ReceiveVector(fuchsia_hardware_spiimpl::wire::SpiImplReceiveVectorRequest* request,
                          fdf::Arena& arena, ReceiveVectorCompleter::Sync& completer) {
  std::vector<uint8_t> rxdata(request->size);
  ExchangePio(nullptr, rxdata.data(), request->size);
  completer.buffer(arena).ReplySuccess(
      fidl::VectorView<uint8_t>::FromExternal(rxdata.data(), rxdata.size()));
}

void DwSpi::ExchangeVector(fuchsia_hardware_spiimpl::wire::SpiImplExchangeVectorRequest* request,
                           fdf::Arena& arena, ExchangeVectorCompleter::Sync& completer) {
  std::vector<uint8_t> rxdata(request->txdata.size());
  ExchangePio(request->txdata.data(), rxdata.data(), request->txdata.size());
  completer.buffer(arena).ReplySuccess(
      fidl::VectorView<uint8_t>::FromExternal(rxdata.data(), rxdata.size()));
}

zx::result<> DwSpiDriver::Start(fdf::DriverContext context) {
  fdf::info("Starting dw-spi driver");

  incoming_ = std::shared_ptr<fdf::Namespace>(context.take_incoming());

  // Connect to platform device
  zx::result<fdf::PDev> pdev = fdf::PDev::Connect(incoming_);
  if (pdev.is_error()) {
    fdf::error("Failed to connect to pdev: {}", pdev.status_string());
    return pdev.take_error();
  }

  // Connect to power domain
  auto power_domain_client =
      incoming()->Connect<fuchsia_hardware_powerdomain::Service::Domain>("power-domain");
  if (power_domain_client.is_error()) {
    fdf::error("Failed to connect to power domain: {}", power_domain_client.status_string());
    return power_domain_client.take_error();
  }
  auto power_domain = fidl::WireSyncClient<fuchsia_hardware_powerdomain::Domain>(
      std::move(power_domain_client.value()));
  auto power_result = power_domain->Enable();
  if (!power_result.ok()) {
    fdf::error("Failed to send enable power domain request: {}", power_result.status_string());
    return zx::error(power_result.status());
  }
  if (power_result->is_error()) {
    fdf::error("Failed to enable power domain: {}",
               zx_status_get_string(power_result->error_value()));
    return zx::error(power_result->error_value());
  }
  fdf::info("Power domain enabled successfully");

  // Enable clocks
  std::vector<std::string_view> clock_names = {"clock-ssi", "clock-pclk"};
  for (const auto& name : clock_names) {
    auto clock_client = incoming()->Connect<fuchsia_hardware_clock::Service::Clock>(name);
    if (clock_client.is_error()) {
      fdf::error("Failed to connect to clock: {}", clock_client.status_string());
      return clock_client.take_error();
    }
    auto clock =
        fidl::WireSyncClient<fuchsia_hardware_clock::Clock>(std::move(clock_client.value()));
    auto clock_result = clock->Enable();
    if (!clock_result.ok()) {
      fdf::error("Failed to send enable clock request: {}", clock_result.status_string());
      return zx::error(clock_result.status());
    }
    if (clock_result->is_error()) {
      fdf::error("Failed to enable clock: {}", zx_status_get_string(clock_result->error_value()));
      return zx::error(clock_result->error_value());
    }
  }

  // Trigger reset
  auto reset_client = incoming()->Connect<fuchsia_hardware_reset::Service::Reset>("reset");
  if (reset_client.is_error()) {
    fdf::error("Failed to connect to reset: {}", reset_client.status_string());
    return reset_client.take_error();
  }
  auto reset = fidl::WireSyncClient<fuchsia_hardware_reset::Reset>(std::move(reset_client.value()));
  auto reset_result = reset->Toggle();
  if (!reset_result.ok()) {
    fdf::error("Failed to send trigger reset request: {}", reset_result.status_string());
    return zx::error(reset_result.status());
  }
  if (reset_result->is_error()) {
    fdf::error("Failed to trigger reset: {}", zx_status_get_string(reset_result->error_value()));
    return zx::error(reset_result->error_value());
  }

  // Get MMIO synchronously
  auto mmio = pdev->MapMmio(0);
  if (mmio.is_error()) {
    fdf::error("Failed to get MMIO: {}", mmio.status_string());
    return mmio.take_error();
  }

  // Get Interrupt synchronously
  auto irq_result = pdev->GetInterrupt("dw-spi");
  if (irq_result.is_error()) {
    fdf::error("Failed to get IRQ: {}", irq_result.status_string());
    return irq_result.take_error();
  }

  device_ = std::make_unique<DwSpi>(*std::move(mmio), std::move(irq_result.value()));
  device_->InitRegisters();

  fdf::info("dw-spi driver initialized successfully");
  return zx::ok();
}

}  // namespace dw_spi

FUCHSIA_DRIVER_EXPORT2(dw_spi::DwSpiDriver);
