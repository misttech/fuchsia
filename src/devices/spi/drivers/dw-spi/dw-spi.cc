// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "dw-spi.h"

#include <fidl/fuchsia.hardware.clock/cpp/wire.h>
#include <fidl/fuchsia.hardware.powerdomain/cpp/wire.h>
#include <fidl/fuchsia.hardware.reset/cpp/wire.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/platform-device/cpp/pdev.h>

#include <vector>

#include "registers.h"

namespace dw_spi {

constexpr size_t kFifoSize = 256;  // The TX and RX FIFO sizes in bytes.

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

zx::result<> DwSpi::ExchangePio(const uint8_t* txdata, uint8_t* out_rxdata, size_t size) {
  if (cs_gpio_.is_valid()) {
    auto result =
        cs_gpio_.sync()->SetBufferMode(fuchsia_hardware_gpio::wire::BufferMode::kOutputLow);
    if (!result.ok()) {
      fdf::error("Failed to send SetBufferMode request: {}", result.error().FormatDescription());
      return zx::error(result.error().status());
    }
    if (result->is_error()) {
      fdf::error("Failed to assert CS: {}", zx_status_get_string(result->error_value()));
      return zx::error(result->error_value());
    }
  }

  // TODO(https://fxbug.dev/500865936): Support DMA transfers for larger sizes.
  // This is a placeholder indicating where DMA support would be added.
  // For now, we only implement PIO.

  // A target must be selected before the transfer can begin.
  Ser::Get().FromValue(0).set_ser(1).WriteTo(&mmio_);

  while (size > 0) {
    if (Sr::Get().ReadFrom(&mmio_).rfne()) {
      fdf::warn("RX FIFO is not empty before starting transfer");
    }

    // Wait for the TX FIFO to be empty.
    while (!Sr::Get().ReadFrom(&mmio_).tfe()) {
    }

    const size_t transfer_size = std::min(size, kFifoSize);

    // Fill the TX FIFO up to available space or remaining data.
    for (size_t i = 0; i < transfer_size; i++) {
      uint32_t data = txdata ? txdata[i] : 0xFF;
      mmio_.Write32(data, DW_SPI_DR0);
    }

    // Read the RX FIFO for the bytes we just sent.
    for (size_t i = 0; i < transfer_size; i++) {
      // Wait for at least one byte to be in the RX FIFO.
      while (!Sr::Get().ReadFrom(&mmio_).rfne()) {
      }

      uint32_t rx_data = mmio_.Read32(DW_SPI_DR0);
      if (out_rxdata) {
        out_rxdata[i] = static_cast<uint8_t>(rx_data);
      }
    }

    size -= transfer_size;
    if (txdata) {
      txdata += transfer_size;
    }
    if (out_rxdata) {
      out_rxdata += transfer_size;
    }
  }

  Ser::Get().FromValue(0).set_ser(0).WriteTo(&mmio_);

  if (cs_gpio_.is_valid()) {
    auto result =
        cs_gpio_.sync()->SetBufferMode(fuchsia_hardware_gpio::wire::BufferMode::kOutputHigh);
    if (!result.ok()) {
      fdf::error("Failed to send SetBufferMode request: {}", result.error().FormatDescription());
      return zx::error(result.error().status());
    }
    if (result->is_error()) {
      fdf::error("Failed to deassert CS: {}", zx_status_get_string(result->error_value()));
      return zx::error(result->error_value());
    }
  }

  return zx::ok();
}

void DwSpi::TransmitVector(fuchsia_hardware_spiimpl::wire::SpiImplTransmitVectorRequest* request,
                           fdf::Arena& arena, TransmitVectorCompleter::Sync& completer) {
  zx::result<> result = ExchangePio(request->data.data(), nullptr, request->data.size());
  completer.buffer(arena).Reply(result);
}

void DwSpi::ReceiveVector(fuchsia_hardware_spiimpl::wire::SpiImplReceiveVectorRequest* request,
                          fdf::Arena& arena, ReceiveVectorCompleter::Sync& completer) {
  fidl::VectorView<uint8_t> rxdata(arena, request->size);
  if (zx::result<> result = ExchangePio(nullptr, rxdata.data(), request->size); result.is_error()) {
    completer.buffer(arena).ReplyError(result.error_value());
  } else {
    completer.buffer(arena).ReplySuccess(rxdata);
  }
}

void DwSpi::ExchangeVector(fuchsia_hardware_spiimpl::wire::SpiImplExchangeVectorRequest* request,
                           fdf::Arena& arena, ExchangeVectorCompleter::Sync& completer) {
  fidl::VectorView<uint8_t> rxdata(arena, request->txdata.size());
  if (zx::result<> result =
          ExchangePio(request->txdata.data(), rxdata.data(), request->txdata.size());
      result.is_error()) {
    completer.buffer(arena).ReplyError(result.error_value());
  } else {
    completer.buffer(arena).ReplySuccess(rxdata);
  }
}

void DwSpi::Serve(fdf::ServerEnd<fuchsia_hardware_spiimpl::SpiImpl> request) {
  bindings_.AddBinding(fdf::Dispatcher::GetCurrent()->get(), std::move(request), this,
                       fidl::kIgnoreBindingClosure);
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

  if (zx::result result = spi_metadata_server_.ForwardAndServe(*outgoing(), dispatcher(), *pdev);
      result.is_error()) {
    fdf::error("Failed to serve SPI metadata: {}", result.status_string());
    return result.take_error();
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
  std::vector<std::string_view> clock_names = {"clock-bus", "clock-registers"};
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

  fidl::ClientEnd<fuchsia_hardware_gpio::Gpio> cs_gpio;
  {
    auto cs_gpio_client = incoming()->Connect<fuchsia_hardware_gpio::Service::Device>("gpio-cs-0");
    if (cs_gpio_client.is_error()) {
      fdf::error("Failed to connect to GPIO: {}", cs_gpio_client.status_string());
      return cs_gpio_client.take_error();
    }

    // The chip select GPIO is optional. Make a call on it do determine whether or not it has been
    // provided to us.
    if (auto result = fidl::WireCall(*cs_gpio_client)->ReleaseInterrupt(); result.ok()) {
      cs_gpio = *std::move(cs_gpio_client);
    }
  }

  device_ =
      std::make_unique<DwSpi>(*std::move(mmio), std::move(irq_result.value()), std::move(cs_gpio));
  device_->InitRegisters();

  fuchsia_hardware_spiimpl::Service::InstanceHandler handler({
      .device = fit::bind_member<&DwSpi::Serve>(device_.get()),
  });
  if (zx::result<> result =
          outgoing()->AddService<fuchsia_hardware_spiimpl::Service>(std::move(handler));
      result.is_error()) {
    fdf::error("AddService failed: {}", result.status_string());
    return result.take_error();
  }

  std::vector<fuchsia_driver_framework::Offer> offers = {
      fdf::MakeOffer2<fuchsia_hardware_spiimpl::Service>(),
  };

  if (std::optional offer = spi_metadata_server_.CreateOffer(); offer.has_value()) {
    offers.push_back(offer.value());
  }

  if (zx::result result =
          AddChild(name(), cpp20::span<const fuchsia_driver_framework::NodeProperty2>(), offers);
      result.is_ok()) {
    controller_.Bind(*std::move(result));
  } else {
    fdf::error("Failed to add child node: {}", result.status_string());
    return result.take_error();
  }

  fdf::info("dw-spi driver initialized successfully");
  return zx::ok();
}

}  // namespace dw_spi

FUCHSIA_DRIVER_EXPORT2(dw_spi::DwSpiDriver);
