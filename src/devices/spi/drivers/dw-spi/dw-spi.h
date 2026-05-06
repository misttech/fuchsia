// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_SPI_DRIVERS_DW_SPI_DW_SPI_H_
#define SRC_DEVICES_SPI_DRIVERS_DW_SPI_DW_SPI_H_

#include <fidl/fuchsia.hardware.spiimpl/cpp/driver/wire.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/mmio/cpp/mmio.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/result.h>

namespace dw_spi {

class DwSpi : public fdf::WireServer<fuchsia_hardware_spiimpl::SpiImpl> {
 public:
  DwSpi(fdf::MmioBuffer mmio, zx::interrupt interrupt)
      : mmio_(std::move(mmio)), interrupt_(std::move(interrupt)) {}

  void InitRegisters();

  void Serve(fdf::ServerEnd<fuchsia_hardware_spiimpl::SpiImpl> request);

  // SpiImpl interface
  void GetChipSelectCount(fdf::Arena& arena,
                          GetChipSelectCountCompleter::Sync& completer) override {
    completer.buffer(arena).Reply(1);  // Assume 1 chip select for now
  }
  void TransmitVector(fuchsia_hardware_spiimpl::wire::SpiImplTransmitVectorRequest* request,
                      fdf::Arena& arena, TransmitVectorCompleter::Sync& completer) override;
  void ReceiveVector(fuchsia_hardware_spiimpl::wire::SpiImplReceiveVectorRequest* request,
                     fdf::Arena& arena, ReceiveVectorCompleter::Sync& completer) override;
  void ExchangeVector(fuchsia_hardware_spiimpl::wire::SpiImplExchangeVectorRequest* request,
                      fdf::Arena& arena, ExchangeVectorCompleter::Sync& completer) override;

  // VMO methods (not supported in PIO initial version)
  void RegisterVmo(fuchsia_hardware_spiimpl::wire::SpiImplRegisterVmoRequest* request,
                   fdf::Arena& arena, RegisterVmoCompleter::Sync& completer) override {
    completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
  }
  void UnregisterVmo(fuchsia_hardware_spiimpl::wire::SpiImplUnregisterVmoRequest* request,
                     fdf::Arena& arena, UnregisterVmoCompleter::Sync& completer) override {
    completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
  }
  void ReleaseRegisteredVmos(
      fuchsia_hardware_spiimpl::wire::SpiImplReleaseRegisteredVmosRequest* request,
      fdf::Arena& arena, ReleaseRegisteredVmosCompleter::Sync& completer) override {
    // VMOs not supported in PIO version.
    // This method is one-way in FIDL, so no reply is required.
  }
  void TransmitVmo(fuchsia_hardware_spiimpl::wire::SpiImplTransmitVmoRequest* request,
                   fdf::Arena& arena, TransmitVmoCompleter::Sync& completer) override {
    completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
  }
  void ReceiveVmo(fuchsia_hardware_spiimpl::wire::SpiImplReceiveVmoRequest* request,
                  fdf::Arena& arena, ReceiveVmoCompleter::Sync& completer) override {
    completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
  }
  void ExchangeVmo(fuchsia_hardware_spiimpl::wire::SpiImplExchangeVmoRequest* request,
                   fdf::Arena& arena, ExchangeVmoCompleter::Sync& completer) override {
    completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
  }
  void LockBus(fuchsia_hardware_spiimpl::wire::SpiImplLockBusRequest* request, fdf::Arena& arena,
               LockBusCompleter::Sync& completer) override {
    completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
  }
  void UnlockBus(fuchsia_hardware_spiimpl::wire::SpiImplUnlockBusRequest* request,
                 fdf::Arena& arena, UnlockBusCompleter::Sync& completer) override {
    completer.buffer(arena).ReplyError(ZX_ERR_NOT_SUPPORTED);
  }

 private:
  void ExchangePio(const uint8_t* txdata, uint8_t* out_rxdata, size_t size);

  fdf::MmioBuffer mmio_;
  zx::interrupt interrupt_;
  fdf::ServerBindingGroup<fuchsia_hardware_spiimpl::SpiImpl> bindings_;
};

class DwSpiDriver : public fdf::DriverBase2 {
 public:
  DwSpiDriver() : fdf::DriverBase2("dw-spi") {}

  zx::result<> Start(fdf::DriverContext context) override;

 protected:
  const std::shared_ptr<fdf::Namespace>& incoming() const { return incoming_; }

 private:
  std::shared_ptr<fdf::Namespace> incoming_;
  std::unique_ptr<DwSpi> device_;
  fidl::WireSyncClient<fuchsia_driver_framework::NodeController> controller_;
};

}  // namespace dw_spi

#endif  // SRC_DEVICES_SPI_DRIVERS_DW_SPI_DW_SPI_H_
